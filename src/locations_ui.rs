use crate::drives::DriveList;
use crate::explorer::Item;
use crate::file_open::{OpenAction, OpenRequest};
use crate::global_search::GlobalController;
use crate::list_selection::ListSelection;
use crate::locations::Locations;
use crate::search::{item_name, SearchInput};
use crate::search_view::SearchView;
use eframe::egui;
use std::path::{Path, PathBuf};

#[derive(Default)]
pub struct LocationsView {
    pub global_mode: bool,
    pub selection_changed: bool,
    drives: DriveList,
    job: GlobalController,
    key: Option<(u64, SearchInput)>,
    selection: ListSelection,
    error: Option<String>,
}

impl LocationsView {
    pub fn choose_scope(&mut self, view: &mut SearchView, scope: crate::search_view::SearchScope) {
        view.invalidate();
        self.selection.clear();
        self.selection_changed = true;
        self.job.cancel();
        self.key = None;
        self.global_mode = scope == crate::search_view::SearchScope::Registered;
        view.whole_scan = scope == crate::search_view::SearchScope::Location;
    }
    pub fn select_result(&mut self, row: usize, ctrl: bool, shift: bool) {
        self.selection.click(row, ctrl, shift);
        self.selection_changed = true;
    }
    pub fn selection_summary(&self) -> crate::list_selection::SelectionSummary {
        crate::list_selection::SelectionSummary::from_items(
            self.selection
                .rows()
                .filter_map(|row| self.results()?.hits.get(row).map(|hit| hit.item)),
        )
    }
    pub fn drive_control(
        &mut self,
        ui: &mut egui::Ui,
        running: bool,
        current: &str,
    ) -> Option<PathBuf> {
        if !self.drives.started {
            self.drives.refresh();
        }
        self.drives.poll();
        let label = current
            .get(..2)
            .filter(|s| s.ends_with(':'))
            .unwrap_or("위치");
        let mut path = None;
        ui.add_enabled_ui(!running, |ui| {
            ui.menu_button(format!("{label}  ▾"), |ui| {
                if self.drives.running() {
                    ui.label("드라이브 조회 중…");
                }
                for drive in &self.drives.entries {
                    if ui.button(drive.path.display().to_string()).clicked() {
                        path = Some(drive.path.clone());
                        ui.close();
                    }
                }
                if let Some(error) = &self.drives.error {
                    ui.colored_label(egui::Color32::LIGHT_RED, error);
                }
                if ui.button("새로고침").clicked() {
                    self.drives.refresh();
                }
            });
        });
        path
    }
    pub fn selected_target(&self) -> Option<crate::map_job::Target> {
        if self.selection.count() != 1 {
            return None;
        }
        let results = self.job.results.as_ref()?;
        let hit = results.hits.get(self.selection.rows().next()?)?;
        let source = &results.sources[hit.source];
        Some((source.id, source.generation, hit.item))
    }
    pub fn select_target(&mut self, target: crate::map_job::Target) {
        self.selection.clear();
        if let Some(results) = &self.job.results {
            if let Some(row) = results.hits.iter().position(|hit| {
                let s = &results.sources[hit.source];
                (s.id, s.generation, hit.item) == target
            }) {
                self.select_result(row, false, false);
            }
        }
    }
    pub fn refresh(&mut self, locations: &Locations, input: &SearchInput) {
        self.job.poll();
        let key = (locations.epoch, input.clone());
        if self.key.as_ref() != Some(&key) {
            self.job.submit(locations.snapshots(), input.clone());
            self.key = Some(key);
            self.selection.clear();
            self.selection_changed = true;
        }
    }
    pub fn results(&self) -> Option<&crate::global_search::GlobalResults> {
        self.job.results.as_ref()
    }
    pub fn controls(
        &mut self,
        ui: &mut egui::Ui,
        locations: &mut Locations,
        target: Option<&Path>,
    ) -> Option<PathBuf> {
        if !self.drives.started {
            self.drives.refresh();
        }
        self.drives.poll();
        let mut path = None;
        ui.collapsing("드라이브·분석 위치", |ui| {
            if ui.button("드라이브 새로고침").clicked() {
                self.drives.refresh();
            }
            for drive in &self.drives.entries {
                let capacity = drive
                    .available
                    .map(|n| format!("여유 {n} B"))
                    .unwrap_or_else(|| "드라이브 정보를 확인할 수 없습니다".into());
                if ui
                    .add_enabled(
                        !locations.is_running(),
                        egui::Button::new(format!(
                            "{} · {} · {capacity}",
                            drive.path.display(),
                            if drive.kind == 2 {
                                "이동식"
                            } else {
                                "고정"
                            }
                        )),
                    )
                    .clicked()
                {
                    path = Some(drive.path.clone());
                }
            }
            if let Some(error) = &self.drives.error {
                ui.colored_label(egui::Color32::LIGHT_RED, error);
            }
            ui.horizontal(|ui| {
                if ui
                    .add_enabled(
                        !locations.is_running() && target.is_some(),
                        egui::Button::new("이 폴더 추가"),
                    )
                    .clicked()
                {
                    self.error = locations
                        .add(target.expect("등록 경로"))
                        .err()
                        .map(|e| e.to_string());
                }
                if ui
                    .add_enabled(
                        !locations.is_running(),
                        egui::Button::new("선택한 위치 스캔"),
                    )
                    .clicked()
                {
                    let ids: Vec<_> = locations
                        .entries
                        .iter()
                        .filter(|e| e.enabled)
                        .map(|e| e.id)
                        .collect();
                    self.error = locations.start_selected(&ids).err().map(|e| e.to_string());
                }
            });
            let mut choose = None;
            let mut remove = None;
            let running = locations.is_running();
            let selected = locations.selected_id();
            for entry in &mut locations.entries {
                ui.horizontal(|ui| {
                    ui.add_enabled_ui(!running, |ui| {
                        ui.checkbox(&mut entry.enabled, "");
                    });
                    if ui
                        .add_enabled(
                            !running,
                            egui::Button::new(entry.path.display().to_string())
                                .selected(selected == Some(entry.id)),
                        )
                        .clicked()
                    {
                        choose = Some(entry.id);
                        self.global_mode = false;
                    }
                    let state = if entry.stale {
                        "스캔 실패 · 이전 결과 표시"
                    } else if entry.scan.is_running() {
                        "스캔 중"
                    } else if entry.scan.completed().is_some() {
                        "스캔 완료"
                    } else {
                        "스캔 전"
                    };
                    ui.label(state);
                    if ui
                        .add_enabled(!running, egui::Button::new("목록에서 빼기"))
                        .clicked()
                    {
                        remove = Some(entry.id);
                    }
                });
            }
            if let Some(id) = choose {
                locations.choose(id);
                path = locations.get(id).map(|entry| entry.path.clone());
            }
            if let Some(id) = remove {
                self.error = locations.remove(id).err().map(|e| e.to_string());
            }
            if let Some(error) = &self.error {
                ui.colored_label(egui::Color32::LIGHT_RED, error);
            }
        });
        ui.checkbox(&mut self.global_mode, "모든 분석 위치 검색");
        if self.drives.running() {
            ui.ctx()
                .request_repaint_after(std::time::Duration::from_millis(100));
        }
        path
    }

    pub fn global(
        &mut self,
        ui: &mut egui::Ui,
        locations: &Locations,
        search: &mut SearchView,
    ) -> (Option<OpenRequest>, Option<(u64, u64, Item)>) {
        self.selection_changed = false;
        self.refresh(locations, &search.input);
        let excluded = locations
            .entries
            .iter()
            .filter(|e| e.stale || e.scan.completed().is_none())
            .count();
        let sources = locations.snapshots();
        let errors: u64 = sources.iter().map(|source| source.scan.error_count).sum();
        let skipped: u64 = sources
            .iter()
            .map(|source| source.scan.skipped_reparse_points)
            .sum();
        if self.job.running {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label("모든 분석 위치에서 찾는 중…");
                if ui.button("검색 취소").clicked() {
                    self.job.cancel();
                    self.selection.clear();
                }
            });
            ui.ctx()
                .request_repaint_after(std::time::Duration::from_millis(30));
        } else if self.job.results.is_none() && self.job.error.is_none() {
            ui.label("검색 취소됨 · 조건을 바꾸면 다시 검색합니다");
        }
        if let Some(error) = &self.job.error {
            ui.colored_label(egui::Color32::LIGHT_RED, error);
        }
        let mut open = None;
        let mut analyze = None;
        if let Some(results) = &self.job.results {
            if !search.editing
                && ui.input_mut(|i| i.consume_key(egui::Modifiers::CTRL, egui::Key::A))
            {
                self.selection.all(results.hits.len());
                self.selection_changed = true;
            }
            search.headers(ui);
            if results.hits.is_empty() {
                ui.weak(if results.sources.is_empty() {
                    "스캔된 위치가 없습니다. 분석할 위치를 먼저 스캔하세요."
                } else if search.input.is_active() {
                    "검색 결과가 없습니다. 검색어나 필터를 바꿔 보세요."
                } else {
                    "분석한 위치에 표시할 파일이나 폴더가 없습니다."
                });
            }
            let output = egui::ScrollArea::both()
                .id_salt("global_results")
                .horizontal_scroll_offset(search.horizontal_offset)
                .max_height((ui.available_height() - 28.0).max(30.0))
                .show_rows(ui, crate::ui_style::ROW, results.hits.len(), |ui, rows| {
                    for row in rows {
                        let hit = &results.hits[row];
                        let source = &results.sources[hit.source];
                        let response = crate::ui_style::row(
                            ui,
                            row,
                            search.widths.total(
                                search.show_path,
                                search.show_modified,
                                search.show_allocated,
                            ),
                            self.selection.contains(row),
                            |ui| {
                                crate::ui_style::name_cell(
                                    ui,
                                    search.widths.get(0),
                                    &item_name(&source.scan, hit.item).to_string_lossy(),
                                    matches!(hit.item, Item::Folder(_)),
                                );
                                search.widths.cells(
                                    ui,
                                    &source.scan,
                                    hit.item,
                                    search.show_path.then_some(search.show_source),
                                    search.show_modified,
                                    search.show_allocated,
                                );
                            },
                        )
                        .on_hover_text(format!(
                            "분석 위치: {}",
                            source.scan.folders[0].path.display()
                        ));
                        if response.clicked() {
                            let m = ui.input(|i| i.modifiers);
                            self.selection.click(row, m.ctrl, m.shift);
                            self.selection_changed = true;
                        }
                        if response.secondary_clicked() && !self.selection.contains(row) {
                            self.selection.click(row, false, false);
                            self.selection_changed = true;
                        }
                        if response.double_clicked() && self.selection.count() <= 1 {
                            match hit.item {
                                Item::Folder(_) => {
                                    analyze = Some((source.id, source.generation, hit.item))
                                }
                                Item::File(_) => {
                                    open = Some(OpenRequest::for_item(
                                        &source.scan,
                                        hit.item,
                                        OpenAction::OpenFile,
                                    ))
                                }
                            }
                        }
                        response.context_menu(|ui| {
                            ui.add_enabled_ui(self.selection.count() == 1, |ui| {
                                if ui.button("용량 분석으로 이동").clicked() {
                                    analyze = Some((source.id, source.generation, hit.item));
                                    ui.close();
                                }
                                let action = match hit.item {
                                    Item::Folder(_) => OpenAction::OpenFolder,
                                    Item::File(_) => OpenAction::OpenFile,
                                };
                                if ui.button("열기").clicked() {
                                    open =
                                        Some(OpenRequest::for_item(&source.scan, hit.item, action));
                                    ui.close();
                                }
                                if matches!(hit.item, Item::File(_))
                                    && ui.button("파일 위치 열기").clicked()
                                {
                                    open = Some(OpenRequest::for_item(
                                        &source.scan,
                                        hit.item,
                                        OpenAction::RevealFile,
                                    ));
                                    ui.close();
                                }
                            });
                        });
                    }
                });
            search.horizontal_offset = output.state.clamped_offset().x;
            egui::Frame::new().fill(crate::ui_style::SIDEBAR).inner_margin(egui::Margin::symmetric(12,6)).show(ui,|ui| {
                ui.set_min_width(ui.available_width());
                ui.small(format!("{}개 항목 · {}개 선택{}",results.hits.len(),self.selection.count(),if results.unknown_dates>0{format!(" · 수정 날짜 확인 불가 {}개 제외",results.unknown_dates)}else{String::new()}))
                    .on_hover_text(format!("마지막 성공 스캔 기준 · 스캔 전/갱신이 필요한 위치 {excluded}개 제외 · 접근 오류 {errors}건 · 특수 경로 제외 {skipped}개"));
            });
        }
        (open, analyze)
    }
}

#[cfg(test)]
mod search_ui_tests {
    use super::*;
    use crate::search_view::SearchScope;
    use std::time::{Duration, Instant};
    #[test]
    fn scope_switch_keeps_conditions_and_routes_real_results_without_old_selection() {
        let root = std::env::temp_dir().join(format!("diskweave-ui-scopes-{}", std::process::id()));
        for name in ["a", "b"] {
            std::fs::create_dir_all(root.join(name).join("child")).unwrap();
            std::fs::write(root.join(name).join("child/model.bin"), [0; 3]).unwrap();
        }
        let mut locations = Locations::default();
        let a = locations.add(&root.join("a")).unwrap();
        let b = locations.add(&root.join("b")).unwrap();
        locations.start_selected(&[a, b]).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while locations.is_running() && Instant::now() < deadline {
            locations.poll();
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(!locations.is_running());
        locations.choose(a);
        let scan = locations
            .get(a)
            .unwrap()
            .scan
            .completed()
            .unwrap()
            .result
            .clone();
        let child = scan.folder_index(std::path::Path::new("child")).unwrap();
        let mut view = SearchView::default();
        let mut global = LocationsView::default();
        view.input.text = "model".into();
        view.input.sort = crate::search::SortField::Name;
        view.selection.click(0, false, false);
        let query = view.input.clone();
        global.choose_scope(&mut view, SearchScope::Location);
        assert_eq!(view.input, query);
        assert_eq!(view.selection.count(), 0);
        view.clear_conditions();
        view.refresh(&scan, child);
        while view.job.results().is_none() && Instant::now() < deadline {
            view.poll();
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(view.job.results().unwrap().items, vec![Item::Folder(child)]);
        view.selection.all(1);
        assert_eq!(view.selection_summary().folders, 1);
        view.input.text = "model".into();
        global.choose_scope(&mut view, SearchScope::Registered);
        while global.results().is_none() && Instant::now() < deadline {
            global.refresh(&locations, &view.input);
            std::thread::sleep(Duration::from_millis(1));
        }
        let results = global.results().unwrap();
        assert_eq!(results.hits.len(), 2);
        for hit in &results.hits {
            assert!(
                crate::search::item_path(&results.sources[hit.source].scan, hit.item)
                    .ends_with("child/model.bin")
            );
        }
        global.select_result(0, false, false);
        global.select_result(1, true, false);
        let summary = global.selection_summary();
        assert_eq!((summary.files, summary.folders), (2, 0));
        assert!(global.selected_target().is_none());
        global.choose_scope(&mut view, SearchScope::Folder);
        assert_eq!(global.selection_summary().count(), 0);
        assert_eq!(view.input.text, "model");
        std::fs::remove_dir_all(root).unwrap();
    }
}
