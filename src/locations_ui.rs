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
                self.selection.click(row, false, false);
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
        ui.collapsing("드라이브·등록 위치", |ui| {
            if ui.button("드라이브 새로고침").clicked() {
                self.drives.refresh();
            }
            for drive in &self.drives.entries {
                let capacity = drive
                    .available
                    .map(|n| format!("여유 {n} B"))
                    .unwrap_or_else(|| "준비 안 됨 / 조회 불가".into());
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
                        egui::Button::new("입력 경로 등록"),
                    )
                    .clicked()
                {
                    self.error = locations
                        .add(target.expect("등록 경로"))
                        .err()
                        .map(|e| e.to_string());
                }
                if ui
                    .add_enabled(!locations.is_running(), egui::Button::new("선택 위치 스캔"))
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
                        "오프라인/실패 · 이전 결과 오래됨"
                    } else if entry.scan.is_running() {
                        "스캔 중"
                    } else if entry.scan.completed().is_some() {
                        "수집 완료"
                    } else {
                        "미수집"
                    };
                    ui.label(state);
                    if ui
                        .add_enabled(!running, egui::Button::new("등록 제거"))
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
        ui.checkbox(&mut self.global_mode, "등록 위치 전체 검색");
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
        search.headers(ui);
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
        ui.label(format!("마지막 성공 스캔 기준 · 미수집/오래된 위치 {excluded}개 제외 · 접근 오류 {errors}건 · Reparse 제외 {skipped}개"));
        if self.job.running {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label("등록 위치 검색 중…");
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
            ui.label(format!(
                "결과 {}개 · 선택 {}개 · 날짜 미수집 제외 {}개",
                results.hits.len(),
                self.selection.count(),
                results.unknown_dates
            ));
            if !search.editing
                && ui.input_mut(|i| i.consume_key(egui::Modifiers::CTRL, egui::Key::A))
            {
                self.selection.all(results.hits.len());
                self.selection_changed = true;
            }
            let output = egui::ScrollArea::both()
                .id_salt("global_results")
                .horizontal_scroll_offset(search.horizontal_offset)
                .max_height(ui.available_height().max(30.0))
                .show_rows(ui, 28.0, results.hits.len(), |ui, rows| {
                    for row in rows {
                        let hit = &results.hits[row];
                        let source = &results.sources[hit.source];
                        let background = ui.painter().add(egui::Shape::Noop);
                        let row_ui = ui.horizontal(|ui| {
                            ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Extend);
                            let response = ui
                                .add_sized(
                                    [search.widths.get(0), 26.0],
                                    egui::Button::new(
                                        item_name(&source.scan, hit.item).to_string_lossy(),
                                    )
                                    .truncate()
                                    .frame(false)
                                    .selected(self.selection.contains(row)),
                                )
                                .on_hover_text(format!(
                                    "출처: {}",
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
                                    if ui.button("분석에서 보기").clicked() {
                                        analyze = Some((source.id, source.generation, hit.item));
                                        ui.close();
                                    }
                                    let action = match hit.item {
                                        Item::Folder(_) => OpenAction::OpenFolder,
                                        Item::File(_) => OpenAction::OpenFile,
                                    };
                                    if ui.button("열기").clicked() {
                                        open = Some(OpenRequest::for_item(
                                            &source.scan,
                                            hit.item,
                                            action,
                                        ));
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
                            search.widths.cells(
                                ui,
                                &source.scan,
                                hit.item,
                                search.show_path,
                                search.show_modified,
                            );
                        });
                        if self.selection.contains(row) {
                            ui.painter().set(
                                background,
                                egui::Shape::rect_filled(
                                    row_ui.response.rect,
                                    0.0,
                                    egui::Color32::from_rgb(27, 52, 71),
                                ),
                            );
                        }
                    }
                });
            search.horizontal_offset = output.state.clamped_offset().x;
        }
        (open, analyze)
    }
}
