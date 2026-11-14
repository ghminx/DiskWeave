use crate::explorer::Item;
use crate::list_selection::ListSelection;
use crate::scanner::ScanResult;
use crate::search::{EntryKind, SearchInput, SearchMode, SortField};
use crate::search_job::{SearchController, SearchStatus};
use eframe::egui;
use std::sync::Arc;

#[derive(Default)]
pub struct SearchView {
    pub input: SearchInput,
    pub whole_scan: bool,
    pub selection: ListSelection,
    pub job: SearchController,
    pub editing: bool,
    pub reveal_row: Option<usize>,
    focus_after_analysis: Option<Item>,
    last_request: Option<(SearchInput, usize, bool)>,
}

impl SearchView {
    pub fn analyze(
        &mut self,
        scan: &Arc<ScanResult>,
        explorer: &mut crate::explorer::Explorer,
    ) -> bool {
        let Some(results) = self.job.results() else {
            return false;
        };
        if !self
            .selection
            .analyze(scan, &results.items, explorer, &mut self.input)
        {
            return false;
        }
        self.whole_scan = false;
        self.refresh(scan, explorer.folder());
        self.focus_after_analysis = Some(explorer.selected());
        if let Some(row) = self.job.results().and_then(|results| {
            results
                .items
                .iter()
                .position(|&item| item == explorer.selected())
        }) {
            self.selection.click(row, false, false);
            self.reveal_row = Some(row);
            self.focus_after_analysis = None;
        }
        true
    }
    pub fn reset(&mut self) {
        self.job.cancel();
        self.input = SearchInput::default();
        self.whole_scan = false;
        self.selection.clear();
        self.last_request = None;
        self.focus_after_analysis = None;
        self.reveal_row = None;
    }

    pub fn poll(&mut self) {
        if self.job.poll() {
            if let Some(item) = self.focus_after_analysis.take() {
                if let Some(row) = self.job.results().and_then(|results| {
                    results
                        .items
                        .iter()
                        .position(|&candidate| candidate == item)
                }) {
                    self.selection.click(row, false, false);
                    self.reveal_row = Some(row);
                }
            }
        }
    }

    pub fn refresh(&mut self, scan: &Arc<ScanResult>, folder: usize) {
        let scope = if self.whole_scan && self.input.is_active() {
            0
        } else {
            folder
        };
        let key = (self.input.clone(), scope, self.whole_scan);
        if self.last_request.as_ref() != Some(&key) {
            self.selection.clear();
            self.reveal_row = None;
            self.focus_after_analysis = None;
            self.job.submit(Arc::clone(scan), self.input.clone(), scope);
            self.last_request = Some(key);
        }
    }

    pub fn controls(&mut self, ui: &mut egui::Ui) {
        self.editing = false;
        ui.horizontal_wrapped(|ui| {
            self.editing |= ui
                .add(
                    egui::TextEdit::singleline(&mut self.input.text)
                        .id(egui::Id::unique("search_query"))
                        .hint_text("파일·폴더 검색")
                        .desired_width(240.0),
                )
                .has_focus();
            ui.selectable_value(&mut self.input.mode, SearchMode::Name, "이름");
            ui.selectable_value(&mut self.input.mode, SearchMode::Path, "전체 경로");
            ui.checkbox(&mut self.whole_scan, "이번 스캔 전체");
            ui.checkbox(&mut self.input.recursive, "하위 포함");
            if ui.button("검색·필터 해제").clicked() {
                self.input = SearchInput::default();
                self.whole_scan = false;
            }
        });
        ui.collapsing("검색 필터", |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.selectable_value(&mut self.input.kind, EntryKind::All, "전체");
                ui.selectable_value(&mut self.input.kind, EntryKind::Files, "파일");
                ui.selectable_value(&mut self.input.kind, EntryKind::Folders, "폴더");
                ui.label("확장자");
                self.editing |= ui
                    .add(
                        egui::TextEdit::singleline(&mut self.input.extensions)
                            .desired_width(150.0)
                            .hint_text("npy;nii.gz"),
                    )
                    .has_focus();
            });
            ui.horizontal_wrapped(|ui| {
                ui.label("최소 Byte");
                self.editing |= ui
                    .add(egui::TextEdit::singleline(&mut self.input.min_size).desired_width(100.0))
                    .has_focus();
                ui.label("최대 Byte");
                self.editing |= ui
                    .add(egui::TextEdit::singleline(&mut self.input.max_size).desired_width(100.0))
                    .has_focus();
            });
            ui.horizontal_wrapped(|ui| {
                ui.label("수정일: 로컬 날짜 양끝 포함");
                self.editing |= ui
                    .add(
                        egui::TextEdit::singleline(&mut self.input.from_date)
                            .desired_width(110.0)
                            .hint_text("YYYY-MM-DD"),
                    )
                    .has_focus();
                ui.label("~");
                self.editing |= ui
                    .add(
                        egui::TextEdit::singleline(&mut self.input.to_date)
                            .desired_width(110.0)
                            .hint_text("YYYY-MM-DD"),
                    )
                    .has_focus();
            });
        });
    }

    pub fn headers(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            for (field, label, width) in [
                (SortField::Name, "이름", 200.0),
                (SortField::Path, "경로", 260.0),
                (SortField::Size, "크기", 110.0),
                (SortField::Modified, "수정 시각", 145.0),
            ] {
                let suffix = if self.input.sort == field {
                    if self.input.descending {
                        " ↓"
                    } else {
                        " ↑"
                    }
                } else {
                    ""
                };
                if ui
                    .add_sized([width, 22.0], egui::Button::new(format!("{label}{suffix}")))
                    .clicked()
                {
                    if self.input.sort == field {
                        self.input.descending = !self.input.descending;
                    } else {
                        self.input.sort = field;
                        self.input.descending = false;
                    }
                }
            }
        });
    }

    pub fn status(&mut self, ui: &mut egui::Ui) {
        match self.job.status() {
            SearchStatus::Running => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label("검색·정렬 중…");
                    if ui.button("검색 취소").clicked() {
                        self.job.cancel();
                        self.selection.clear();
                    }
                });
                ui.ctx()
                    .request_repaint_after(std::time::Duration::from_millis(30));
            }
            SearchStatus::Failed(error) => {
                ui.colored_label(egui::Color32::LIGHT_RED, error);
            }
            SearchStatus::Cancelled => {
                ui.label("검색 취소됨 · 조건을 변경하면 다시 검색합니다");
            }
            SearchStatus::Completed => {
                if let Some(results) = self.job.results() {
                    ui.label(format!(
                        "{}개 항목 · {}개 선택 · 수정 시각 미수집으로 제외 {}개",
                        results.items.len(),
                        self.selection.count(),
                        results.unknown_dates
                    ));
                }
            }
            SearchStatus::Idle => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::explorer::{Explorer, Item};
    use std::time::{Duration, Instant};

    fn wait(view: &mut SearchView) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while matches!(view.job.status(), SearchStatus::Running) && Instant::now() < deadline {
            view.poll();
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(matches!(view.job.status(), SearchStatus::Completed));
    }

    #[test]
    fn condition_changes_clear_selection_and_analysis_reselects_the_file() {
        let root =
            std::env::temp_dir().join(format!("diskweave-search-view-{}", std::process::id()));
        std::fs::create_dir_all(root.join("child")).unwrap();
        std::fs::write(root.join("child").join("model.bin"), [0; 3]).unwrap();
        std::fs::write(root.join("child").join("large.bin"), [0; 128]).unwrap();
        std::fs::write(root.join("child").join("middle.bin"), [0; 7]).unwrap();
        let scan = Arc::new(crate::scanner::scan(&root, || false).unwrap());
        let parent = scan.folder_index(std::path::Path::new("child")).unwrap();
        let file = scan
            .files
            .iter()
            .position(|file| file.name == "model.bin")
            .unwrap();
        let mut explorer = Explorer::new(&scan);
        let mut view = SearchView::default();
        view.input.text = "model".into();
        view.refresh(&scan, 0);
        wait(&mut view);
        view.selection.click(0, false, false);
        view.input.descending = false;
        view.refresh(&scan, 0);
        assert_eq!(view.selection.count(), 0);
        wait(&mut view);
        view.selection.click(0, false, false);
        assert!(view.analyze(&scan, &mut explorer));
        wait(&mut view);
        assert_eq!(explorer.folder(), parent);
        assert_eq!(view.reveal_row, Some(2));
        assert_eq!(
            view.selection.single(&view.job.results().unwrap().items),
            Some(Item::File(file))
        );
        assert!(view.analyze(&scan, &mut explorer));
        assert_eq!(
            view.selection.single(&view.job.results().unwrap().items),
            Some(Item::File(file))
        );
        view.input.text = "absent".into();
        view.refresh(&scan, parent);
        assert_eq!(view.selection.count(), 0);
        wait(&mut view);
        assert!(view.job.results().unwrap().items.is_empty());
        view.reset();
        assert_eq!(view.input, SearchInput::default());
        assert_eq!(view.selection.count(), 0);
        std::fs::remove_dir_all(root).unwrap();
    }
}
