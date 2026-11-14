use crate::analysis::{compute, Statistics, StatsScope};
use crate::explorer::Item;
use crate::file_open::{OpenAction, OpenRequest};
use crate::locations::Snapshot;
use crate::search::SearchInput;
use std::io;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    mpsc, Arc,
};

#[derive(Clone, PartialEq, Eq)]
pub struct PanelKey {
    pub versions: Vec<(u64, u64)>,
    pub folder: Option<usize>,
    pub query: Option<SearchInput>,
}
pub struct Output {
    pub sources: Vec<Snapshot>,
    pub stats: Statistics,
}
struct Request {
    id: u64,
    sources: Vec<Snapshot>,
    scope: StatsScope,
}
pub struct AnalysisPanel {
    pub picked: Option<(u64, u64, Item)>,
    pub matched: bool,
    pub large_files: bool,
    pub top_count: usize,
    pub running: bool,
    pub error: Option<String>,
    pub output: Option<Output>,
    key: Option<PanelKey>,
    latest: Arc<AtomicU64>,
    active: Option<mpsc::Receiver<(u64, io::Result<Output>)>>,
    pending: Option<Request>,
    selected: Option<usize>,
}
impl Default for AnalysisPanel {
    fn default() -> Self {
        Self {
            picked: None,
            matched: false,
            large_files: false,
            top_count: 10,
            running: false,
            error: None,
            output: None,
            key: None,
            latest: Arc::default(),
            active: None,
            pending: None,
            selected: None,
        }
    }
}
impl AnalysisPanel {
    pub fn needs(&self, key: &PanelKey) -> bool {
        self.key.as_ref() != Some(key)
    }
    pub fn select_target(&mut self, target: (u64, u64, Item)) {
        self.selected = self.output.as_ref().and_then(|output| {
            output.stats.top(self.top_count).iter().position(|hit| {
                let source = &output.sources[hit.source];
                (source.id, source.generation, hit.item) == target
            })
        });
    }
    pub fn submit(&mut self, key: PanelKey, sources: Vec<Snapshot>, scope: StatsScope) {
        let id = self.latest.fetch_add(1, Ordering::Relaxed) + 1;
        self.key = Some(key);
        self.output = None;
        self.error = None;
        self.running = true;
        self.selected = None;
        self.pending = Some(Request { id, sources, scope });
        self.launch();
    }
    pub fn clear(&mut self) {
        self.latest.fetch_add(1, Ordering::Relaxed);
        self.pending = None;
        self.output = None;
        self.error = None;
        self.key = None;
        self.running = false;
        self.selected = None;
    }
    fn launch(&mut self) {
        if self.active.is_some() {
            return;
        }
        let Some(request) = self.pending.take() else {
            return;
        };
        let latest = Arc::clone(&self.latest);
        let (sender, receiver) = mpsc::channel();
        std::thread::spawn(move || {
            let stats = compute(&request.sources, &request.scope, || {
                latest.load(Ordering::Relaxed) != request.id
            });
            let _ = sender.send((
                request.id,
                stats.map(|stats| Output {
                    sources: request.sources,
                    stats,
                }),
            ));
        });
        self.active = Some(receiver);
    }
    pub fn poll(&mut self) {
        let Some(receiver) = &self.active else {
            return;
        };
        let response = match receiver.try_recv() {
            Ok(response) => Some(response),
            Err(mpsc::TryRecvError::Empty) => return,
            Err(mpsc::TryRecvError::Disconnected) => None,
        };
        self.active = None;
        match response {
            Some((id, result)) if id == self.latest.load(Ordering::Relaxed) => {
                self.running = false;
                match result {
                    Ok(output) => self.output = Some(output),
                    Err(error) => self.error = Some(error.to_string()),
                }
            }
            None if self.pending.is_none() && self.running => {
                self.running = false;
                self.error = Some("통계 작업이 예기치 않게 종료됐습니다".into());
            }
            _ => {}
        }
        self.launch();
    }
    pub fn route(&self, row: usize) -> Option<(u64, u64, Item)> {
        let output = self.output.as_ref()?;
        let hit = output.stats.top(self.top_count).get(row)?;
        let source = &output.sources[hit.source];
        Some((source.id, source.generation, hit.item))
    }
    pub fn open(&self, row: usize, action: OpenAction) -> Option<OpenRequest> {
        let output = self.output.as_ref()?;
        let hit = output.stats.top(self.top_count).get(row)?;
        Some(OpenRequest::for_item(
            &output.sources[hit.source].scan,
            hit.item,
            action,
        ))
    }

    pub fn show(
        &mut self,
        ui: &mut eframe::egui::Ui,
    ) -> (Option<OpenRequest>, Option<(u64, u64, Item)>) {
        use eframe::egui;
        self.picked = None;
        self.poll();
        if self.running {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label("통계 집계 중…");
            });
            ui.ctx()
                .request_repaint_after(std::time::Duration::from_millis(30));
        }
        if let Some(error) = &self.error {
            ui.colored_label(egui::Color32::LIGHT_RED, error);
        }
        let Some(output) = &self.output else {
            return (None, None);
        };
        let size = crate::presentation::SizeDisplay::new(output.stats.bytes);
        ui.label(format!(
            "논리 크기 {} · 파일 {}개 · Hard Link는 경로별 집계",
            size.text, output.stats.files
        ))
        .on_hover_text(size.exact);
        let mut action = None;
        let mut analyze = None;
        let mut picked = None;
        if self.large_files {
            let top = output.stats.top(self.top_count);
            if top.is_empty() {
                ui.label("집계할 파일이 없습니다");
            }
            egui::ScrollArea::both()
                .id_salt("statistics_top")
                .max_height(ui.available_height().max(40.0))
                .show_rows(ui, 24.0, top.len(), |ui, rows| {
                    for row in rows {
                        let hit = &top[row];
                        let source = &output.sources[hit.source];
                        ui.horizontal(|ui| {
                            let path = crate::search::item_path(&source.scan, hit.item);
                            let response = ui.selectable_label(
                                self.selected == Some(row),
                                path.display().to_string(),
                            );
                            if response.clicked() || response.secondary_clicked() {
                                self.selected = Some(row);
                                picked = Some(row);
                            }
                            if response.double_clicked() {
                                action = Some((row, OpenAction::OpenFile));
                            }
                            response.context_menu(|ui| {
                                if ui.button("기본 앱으로 열기").clicked() {
                                    action = Some((row, OpenAction::OpenFile));
                                    ui.close();
                                }
                                if ui.button("파일 위치 열기").clicked() {
                                    action = Some((row, OpenAction::RevealFile));
                                    ui.close();
                                }
                                if ui.button("분석에서 보기").clicked() {
                                    analyze = Some(row);
                                    ui.close();
                                }
                            });
                            let size = crate::presentation::SizeDisplay::new(
                                crate::search::item_bytes(&source.scan, hit.item),
                            );
                            ui.label(size.text).on_hover_text(size.exact);
                        });
                    }
                });
        } else {
            ui.label("확장자 / 논리 크기 / 파일 수 / 집계 범위 대비");
            egui::ScrollArea::both()
                .id_salt("statistics_extensions")
                .max_height(ui.available_height().max(40.0))
                .show_rows(ui, 24.0, output.stats.extensions.len(), |ui, rows| {
                    for row in rows {
                        let extension = &output.stats.extensions[row];
                        ui.horizontal(|ui| {
                            ui.add_sized(
                                [150.0, 24.0],
                                egui::Label::new(&extension.name).truncate(),
                            );
                            let size = crate::presentation::SizeDisplay::new(extension.bytes);
                            ui.add_sized([125.0, 24.0], egui::Label::new(size.text))
                                .on_hover_text(size.exact);
                            ui.add_sized(
                                [90.0, 24.0],
                                egui::Label::new(extension.files.to_string()),
                            );
                            ui.label(crate::presentation::percentage(
                                extension.bytes,
                                output.stats.bytes,
                            ));
                        });
                    }
                });
        }
        self.picked = picked.and_then(|row| self.route(row));
        (
            action.and_then(|(row, action)| self.open(row, action)),
            analyze.and_then(|row| self.route(row)),
        )
    }
}
impl Drop for AnalysisPanel {
    fn drop(&mut self) {
        self.latest.fetch_add(1, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::global_search::Hit;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    fn wait(panel: &mut AnalysisPanel) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while panel.running && Instant::now() < deadline {
            panel.poll();
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(!panel.running);
        assert!(panel.error.is_none(), "{:?}", panel.error);
    }

    #[test]
    fn panel_invalidates_scope_and_new_scan_and_routes_top_files_to_their_source() {
        let root = std::env::temp_dir().join(format!("diskweave-panel-{}", std::process::id()));
        std::fs::create_dir_all(root.join("a")).unwrap();
        std::fs::create_dir(root.join("b")).unwrap();
        std::fs::write(root.join("a/a.bin"), [0; 3]).unwrap();
        std::fs::write(root.join("b/b.bin"), [0; 20]).unwrap();
        let sources: Vec<_> = ["a", "b"]
            .iter()
            .enumerate()
            .map(|(i, p)| Snapshot {
                id: i as u64 + 10,
                generation: 1,
                scan: Arc::new(crate::scanner::scan(&root.join(p), || false).unwrap()),
            })
            .collect();
        let key = PanelKey {
            versions: vec![(10, 1), (11, 1)],
            folder: None,
            query: None,
        };
        let mut panel = AnalysisPanel::default();
        assert_eq!(panel.top_count, 10);
        panel.submit(key.clone(), sources.clone(), StatsScope::All);
        wait(&mut panel);
        assert!(!panel.needs(&key));
        assert_eq!(panel.output.as_ref().unwrap().stats.bytes, 23);
        let (id, generation, item) = panel.route(0).unwrap();
        assert_eq!((id, generation), (11, 1));
        let source = &sources[1];
        let explorer = source.analyze(item);
        assert_eq!(explorer.selected(), item);
        let request = panel.open(0, OpenAction::RevealFile).unwrap();
        assert_eq!(request.path(), root.join("b/b.bin"));
        crate::file_open::open_with(request, |_| Ok(())).unwrap();
        let matched = PanelKey {
            query: Some(SearchInput::default()),
            ..key.clone()
        };
        panel.submit(
            matched,
            sources.clone(),
            StatsScope::Matches(vec![Hit {
                source: 0,
                item: crate::explorer::Item::File(0),
            }]),
        );
        assert!(panel.output.is_none());
        assert!(panel.route(0).is_none());
        wait(&mut panel);
        assert_eq!(panel.output.as_ref().unwrap().stats.bytes, 3);
        panel.submit(key.clone(), sources.clone(), StatsScope::All);
        std::fs::write(root.join("a/a.bin"), [0; 4]).unwrap();
        let fresh = Snapshot {
            id: 10,
            generation: 2,
            scan: Arc::new(crate::scanner::scan(&root.join("a"), || false).unwrap()),
        };
        let changed = PanelKey {
            versions: vec![(10, 2)],
            folder: Some(0),
            query: None,
        };
        panel.submit(changed, vec![fresh], StatsScope::Subtree(0));
        wait(&mut panel);
        assert_eq!(panel.output.as_ref().unwrap().stats.bytes, 4);
        assert_eq!(
            panel.route(0).map(|(id, generation, _)| (id, generation)),
            Some((10, 2))
        );
        panel.clear();
        assert!(panel.output.is_none());
        assert!(panel.needs(&key));
        std::fs::remove_dir_all(root).unwrap();
    }
}
