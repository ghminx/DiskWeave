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

type LiveSelection = (
    SearchInput,
    std::collections::HashMap<(u64, PathBuf), Option<crate::usn::entries::Key>>,
);

#[derive(Default)]
pub struct LocationsView {
    pub file_requested: Option<crate::file_ui::Kind>,
    pub drop_requested: Option<(Vec<PathBuf>, PathBuf)>,
    live_selection: Option<LiveSelection>,
    pub global_mode: bool,
    pub selection_changed: bool,
    drives: DriveList,
    job: GlobalController,
    key: Option<(u64, SearchInput)>,
    selection: ListSelection,
    error: Option<String>,
}

impl LocationsView {
    pub fn restore_selection(
        &mut self,
        input: SearchInput,
        paths: std::collections::HashSet<(u64, PathBuf)>,
    ) {
        self.selection.clear();
        self.key = None;
        self.live_selection = Some((
            input,
            paths.into_iter().map(|target| (target, None)).collect(),
        ));
    }
    pub fn selected_targets(&self) -> Vec<crate::map_job::Target> {
        let Some(results) = &self.job.results else {
            return vec![];
        };
        self.selection
            .rows()
            .filter_map(|row| {
                let hit = results.hits.get(row)?;
                let source = &results.sources[hit.source];
                Some((source.id, source.generation, hit.item))
            })
            .collect()
    }
    pub fn preserve_live_selection(&mut self, locations: &Locations) {
        if !self.global_mode {
            return;
        }
        if self.live_selection.is_some() {
            self.reconnect_selection(locations);
            return;
        }
        if let (Some(results), Some((_, input))) = (&self.job.results, &self.key) {
            let paths = self
                .selection
                .rows()
                .filter_map(|row| {
                    results.hits.get(row).and_then(|hit| {
                        let source = &results.sources[hit.source];
                        let key = match &source.entries {
                            Some(entries) => Some(entries.key(hit.item)?),
                            None => None,
                        };
                        Some((
                            (source.id, crate::search::item_path(&source.scan, hit.item)),
                            key,
                        ))
                    })
                })
                .collect();
            self.live_selection = Some((input.clone(), paths));
        }
        self.reconnect_selection(locations);
    }
    fn reconnect_selection(&mut self, locations: &Locations) {
        let Some((_, paths)) = &mut self.live_selection else {
            return;
        };
        paths.retain(|(id, path), key| {
            let Some(location) = locations.get(*id).filter(|location| !location.stale) else {
                return false;
            };
            let Some(completed) = location.scan.completed() else {
                return false;
            };
            if let Some(entries) = &completed.entries {
                let Some(relative) = path.strip_prefix(&completed.result.folders[0].path).ok()
                else {
                    return false;
                };
                let Some(next) = entries.path_key(relative) else {
                    return false;
                };
                if key.is_some_and(|old| entries.same_scope(old) && old != next) {
                    return false;
                }
                // 검색 완료를 기다리지 않고 새 수명의 기준점에 연결하여 다음 교체와 구분.
                *key = Some(next);
            } else {
                *key = None;
            }
            true
        });
    }
    pub fn choose_scope(&mut self, view: &mut SearchView, scope: crate::search_view::SearchScope) {
        view.invalidate();
        self.invalidate_global_search();
        self.global_mode = scope == crate::search_view::SearchScope::Registered;
        view.whole_scan = scope == crate::search_view::SearchScope::Location;
    }
    pub(crate) fn set_global_mode(&mut self, global: bool) {
        if self.global_mode != global {
            self.global_mode = global;
            self.invalidate_global_search();
        }
    }
    fn invalidate_global_search(&mut self) {
        self.live_selection = None;
        self.selection.clear();
        self.selection_changed = true;
        self.job.cancel();
        self.key = None;
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
    pub fn drive_control(&mut self, ui: &mut egui::Ui, running: bool) -> Option<PathBuf> {
        if !self.drives.started {
            self.drives.refresh();
        }
        self.drives.poll();
        let mut path = None;
        ui.add_enabled_ui(!running, |ui| {
            ui.menu_button("드라이브 선택", |ui| {
                if self.drives.running() {
                    ui.label("드라이브 조회 중…");
                }
                for drive in &self.drives.entries {
                    let label = format!(
                        "{} · {}",
                        drive.path.display(),
                        drive
                            .available
                            .map(|n| format!(
                                "여유 {}",
                                crate::presentation::SizeDisplay::new(n).text
                            ))
                            .unwrap_or_else(|| "용량 확인 불가".into())
                    );
                    if ui.button(label).clicked() {
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
        if self.drives.running() {
            ui.ctx()
                .request_repaint_after(std::time::Duration::from_millis(100));
        }
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
            if self
                .live_selection
                .as_ref()
                .is_some_and(|(previous, _)| previous != input)
            {
                self.live_selection = None;
            }
            self.reconnect_selection(locations);
            self.job.submit(locations.snapshots(), input.clone());
            self.key = Some(key);
            self.selection.clear();
            self.selection_changed = true;
        }
        if !self.job.running {
            if let (Some((_, paths)), Some(results)) = (
                self.live_selection
                    .take()
                    .filter(|(_, paths)| !paths.is_empty()),
                &self.job.results,
            ) {
                let names: std::collections::HashSet<_> = paths
                    .keys()
                    .filter_map(|(id, path)| Some((*id, path.file_name()?)))
                    .collect();
                for (row, hit) in results.hits.iter().enumerate() {
                    let source = &results.sources[hit.source];
                    if !names.contains(&(source.id, item_name(&source.scan, hit.item))) {
                        continue;
                    }
                    if paths
                        .get(&(source.id, crate::search::item_path(&source.scan, hit.item)))
                        .is_some_and(|key| match (key, &source.entries) {
                            (Some(key), Some(entries)) if entries.same_scope(*key) => {
                                entries.key(hit.item) == Some(*key)
                            }
                            _ => true,
                        })
                    {
                        self.selection.click(row, true, false);
                    }
                }
            }
        }
    }
    pub fn results(&self) -> Option<&crate::global_search::GlobalResults> {
        self.job.results.as_deref()
    }
    pub fn controls(
        &mut self,
        ui: &mut egui::Ui,
        locations: &mut Locations,
        target: Option<&Path>,
    ) -> Option<u64> {
        let mut choose = None;
        ui.scope(|ui| {
            ui.strong("등록한 분석 위치");
            ui.small("목록에서 제거해도 원본 파일은 삭제되지 않습니다.");
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
                    }
                    let state = if entry.scan.is_running() {
                        "스캔 중"
                    } else if entry.stale {
                        if entry.scan.completed().is_some() {
                            "갱신 필요 · 이전 결과 표시"
                        } else {
                            "스캔 실패 · 결과 없음"
                        }
                    } else if entry.scan.completed().is_some() {
                        "스캔 완료"
                    } else {
                        "스캔 전"
                    };
                    ui.label(state).on_hover_text(crate::local_date::display(
                        entry.scan.completed().map(|c| c.finished),
                    ));
                    if ui
                        .add_enabled(!running, egui::Button::new("목록에서 빼기"))
                        .clicked()
                    {
                        remove = Some(entry.id);
                    }
                });
            }
            if let Some(id) = remove {
                self.error = locations.remove(id).err().map(|e| e.to_string());
            }
            if let Some(error) = &self.error {
                ui.colored_label(egui::Color32::LIGHT_RED, error);
            }
        });
        let mut global = self.global_mode;
        if locations.snapshots().len() > 1 || global {
            ui.checkbox(&mut global, "모든 스캔 위치에서 검색");
            self.set_global_mode(global);
        }
        choose
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
                crate::ui_style::activity_indicator(ui);
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
                .vertical_scroll_offset(
                    search
                        .reveal_row
                        .take()
                        .map_or(search.vertical_offset, |r| r as f32 * crate::ui_style::ROW),
                )
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
                        let response = response.interact(egui::Sense::click_and_drag());
                        if !locations.is_running() {
                            if response.drag_started() {
                                let paths = if self.selection.contains(row) {
                                    self.selection
                                        .rows()
                                        .filter_map(|r| results.hits.get(r))
                                        .map(|h| {
                                            crate::search::item_path(
                                                &results.sources[h.source].scan,
                                                h.item,
                                            )
                                        })
                                        .collect()
                                } else {
                                    vec![crate::search::item_path(&source.scan, hit.item)]
                                };
                                response
                                    .dnd_set_drag_payload(crate::file_ui::DragPayload { paths });
                            }
                            if let Item::Folder(id) = hit.item {
                                crate::file_ui::drop_hint(ui, &response);
                                if let Some(payload) =
                                    response.dnd_release_payload::<crate::file_ui::DragPayload>()
                                {
                                    self.drop_requested = Some((
                                        payload.paths.clone(),
                                        source.scan.folders[id].path.clone(),
                                    ));
                                }
                            }
                        }
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
                            crate::ui_style::menu_spacing(ui);
                            if ui.button("선택한 경로 복사").clicked() {
                                let paths: Vec<_> = self
                                    .selection
                                    .rows()
                                    .filter_map(|row| results.hits.get(row))
                                    .map(|hit| {
                                        crate::search::item_path(
                                            &results.sources[hit.source].scan,
                                            hit.item,
                                        )
                                    })
                                    .collect();
                                if let Err(error) = crate::path_copy::copy_with(&paths, |text| {
                                    ui.ctx().copy_text(text);
                                    Ok(())
                                }) {
                                    ui.label(error.to_string());
                                } else {
                                    ui.close();
                                }
                            }

                            for kind in [
                                crate::file_ui::Kind::Rename,
                                crate::file_ui::Kind::Copy,
                                crate::file_ui::Kind::Move,
                                crate::file_ui::Kind::Recycle,
                            ] {
                                if ui
                                    .add_enabled(
                                        !locations.is_running()
                                            && (kind != crate::file_ui::Kind::Rename
                                                || self.selection.count() == 1),
                                        egui::Button::new(kind.label()),
                                    )
                                    .clicked()
                                {
                                    self.file_requested = Some(kind);
                                    ui.close();
                                }
                            }
                            ui.separator();
                            ui.add_enabled_ui(self.selection.count() == 1, |ui| {
                                if crate::ui_style::menu_action(ui, "용량 분석으로 이동", true)
                                    .clicked()
                                {
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
                                    && crate::ui_style::menu_action(ui, "파일 위치 열기", true)
                                        .clicked()
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
            search.vertical_offset = output.state.clamped_offset().y;
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
    #[test]
    fn tracked_searches_keep_existing_entries_but_do_not_select_replaced_paths() {
        use crate::usn::{Event, Index, Journal, Source};
        struct Files {
            volume: String,
            next: i64,
            events: Vec<Event>,
        }
        impl Source for Files {
            fn journal(&mut self) -> std::io::Result<Journal> {
                Ok(Journal {
                    volume: self.volume.clone(),
                    id: 7,
                    first: 0,
                    lowest: 0,
                    next: self.next,
                })
            }
            fn read(&mut self, start: i64, end: i64) -> std::io::Result<Vec<Event>> {
                Ok(self
                    .events
                    .iter()
                    .filter(|event| event.usn >= start && event.usn < end)
                    .cloned()
                    .collect())
            }
            fn inspect(
                &mut self,
                path: &std::path::Path,
                detailed: bool,
            ) -> std::io::Result<crate::ntfs::Info> {
                crate::ntfs_native::inspect_file(path, detailed, &self.volume)
            }
            fn list(&mut self, path: &std::path::Path) -> std::io::Result<Vec<std::path::PathBuf>> {
                std::fs::read_dir(path)?
                    .map(|entry| entry.map(|entry| entry.path()))
                    .collect()
            }
        }
        fn settle_global(
            view: &mut LocationsView,
            locations: &Locations,
            input: &crate::search::SearchInput,
        ) {
            let until = Instant::now() + Duration::from_secs(5);
            loop {
                view.refresh(locations, input);
                if view.results().is_some_and(|results| {
                    results.sources.iter().all(|source| {
                        source.generation == locations.get(source.id).unwrap().generation
                    })
                }) {
                    return;
                }
                assert!(Instant::now() < until);
                std::thread::yield_now();
            }
        }
        fn settle_search(view: &mut crate::search_view::SearchView) {
            let until = Instant::now() + Duration::from_secs(5);
            while matches!(view.job.status(), crate::search_job::SearchStatus::Running) {
                view.poll();
                assert!(Instant::now() < until);
                std::thread::yield_now();
            }
        }
        let fixture = crate::duplicates::tests::Fixture::new();
        let path = fixture.0.join("keep.bin");
        std::fs::write(&path, b"123").unwrap();
        let drive = fixture
            .0
            .components()
            .take(2)
            .collect::<std::path::PathBuf>();
        let mut files = Files {
            volume: crate::ntfs_native::target(&drive).unwrap().volume,
            next: 100,
            events: vec![],
        };
        let mut index = Index::begin(&fixture.0, true, &mut files, &|| false).unwrap();
        let before = index.snapshot(&|| false).unwrap();
        let mut locations = Locations::default();
        let id = locations.add(&fixture.0).unwrap();
        locations.accept_tracking(id, &before).unwrap();
        let mut search = crate::search_view::SearchView::default();
        search.input.text = "bin".into();
        search.refresh(&before.scan, 0);
        settle_search(&mut search);
        search.selection.click(0, false, false);
        let mut explorer = crate::explorer::Explorer::new(&before.scan);
        explorer.select(crate::explorer::Item::File(0));
        let mut global = LocationsView {
            global_mode: true,
            ..Default::default()
        };
        settle_global(&mut global, &locations, &search.input);
        global.select_result(0, false, false);
        global.preserve_live_selection(&locations);
        let added = fixture.0.join("added.bin");
        std::fs::write(&added, b"123456789").unwrap();
        let crate::ntfs::Id::V2(new_id) = files.inspect(&added, false).unwrap().reference.unwrap();
        files.events.push(Event {
            usn: 100,
            id: new_id,
            parent: index.checkpoint.root,
            reason: 0x80000100,
        });
        files.next = 108;
        index.refresh(&mut files, &|| false).unwrap();
        let after = index.snapshot(&|| false).unwrap();
        locations.accept_tracking(id, &after).unwrap();
        explorer = search.replace_result(
            Some(&explorer),
            Some(&before.scan),
            &after.scan,
            before.entries.as_deref().zip(after.entries.as_deref()),
        );
        settle_search(&mut search);
        settle_global(&mut global, &locations, &search.input);
        let chosen = search
            .selection
            .single(&search.job.results().unwrap().items)
            .unwrap();
        assert_eq!(crate::search::item_path(&after.scan, chosen), path);
        assert_eq!(
            crate::search::item_path(&after.scan, global.selected_target().unwrap().2),
            path
        );
        assert_eq!(
            crate::search::item_path(&after.scan, explorer.selected()),
            path
        );
        global.preserve_live_selection(&locations);
        let mut restarted = Index::begin(&fixture.0, true, &mut files, &|| false).unwrap();
        let resumed = restarted.snapshot(&|| false).unwrap();
        locations.accept_tracking(id, &resumed).unwrap();
        explorer = search.replace_result(
            Some(&explorer),
            Some(&after.scan),
            &resumed.scan,
            after.entries.as_deref().zip(resumed.entries.as_deref()),
        );
        settle_search(&mut search);
        settle_global(&mut global, &locations, &search.input);
        let chosen = search
            .selection
            .single(&search.job.results().unwrap().items)
            .expect("새 추적 수명에서도 기존 경로 선택 복구 필요");
        assert_eq!(crate::search::item_path(&resumed.scan, chosen), path);
        assert_eq!(
            crate::search::item_path(&resumed.scan, global.selected_target().unwrap().2),
            path
        );
        global.preserve_live_selection(&locations);
        let mut rapid_restart = Index::begin(&fixture.0, true, &mut files, &|| false).unwrap();
        let rapid_baseline = rapid_restart.snapshot(&|| false).unwrap();
        locations.accept_tracking(id, &rapid_baseline).unwrap();
        explorer = search.replace_result(
            Some(&explorer),
            Some(&resumed.scan),
            &rapid_baseline.scan,
            resumed
                .entries
                .as_deref()
                .zip(rapid_baseline.entries.as_deref()),
        );
        // 검색 완료 전에 같은 새 수명의 다음 변경을 적용하는 입력 순서 재현.
        global.refresh(&locations, &search.input);
        index = rapid_restart;
        let after = rapid_baseline;
        global.preserve_live_selection(&locations);
        std::fs::rename(&path, fixture.0.join("previous.bin")).unwrap();
        std::fs::write(&path, b"12345").unwrap();
        let crate::ntfs::Id::V2(replaced) = files.inspect(&path, false).unwrap().reference.unwrap();
        files.events.push(Event {
            usn: 108,
            id: replaced,
            parent: index.checkpoint.root,
            reason: 0x80000100,
        });
        files.next = 116;
        index.refresh(&mut files, &|| false).unwrap();
        let replacement = index.snapshot(&|| false).unwrap();
        locations.accept_tracking(id, &replacement).unwrap();
        explorer = search.replace_result(
            Some(&explorer),
            Some(&after.scan),
            &replacement.scan,
            after.entries.as_deref().zip(replacement.entries.as_deref()),
        );
        settle_search(&mut search);
        settle_global(&mut global, &locations, &search.input);
        assert_eq!(explorer.selected(), crate::explorer::Item::Folder(0));
        assert_eq!(search.selection.count(), 0);
        assert!(global.selected_target().is_none());
        assert_eq!(search.input.text, "bin");
    }
    #[test]
    fn live_global_refresh_keeps_the_selected_source_and_path() {
        let root =
            std::env::temp_dir().join(format!("diskweave-global-live-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("keep"), b"123").unwrap();
        let mut locations = super::Locations::default();
        locations.start(root.clone()).unwrap();
        let until = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while locations.is_running() {
            locations.poll();
            assert!(std::time::Instant::now() < until);
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        let mut view = super::LocationsView {
            global_mode: true,
            ..Default::default()
        };
        let input = crate::search::SearchInput::default();
        while view.results().is_none() {
            view.refresh(&locations, &input);
            assert!(std::time::Instant::now() < until);
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        view.select_result(0, false, false);
        view.preserve_live_selection(&locations);
        std::fs::write(root.join("big"), b"123456789").unwrap();
        locations.start(root.clone()).unwrap();
        while locations.is_running() {
            locations.poll();
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        view.refresh(&locations, &input);
        while view.results().is_none() {
            view.refresh(&locations, &input);
            assert!(std::time::Instant::now() < until);
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        let (id, _, item) = view.selected_target().unwrap();
        assert_eq!(
            crate::search::item_path(
                &locations.get(id).unwrap().scan.completed().unwrap().result,
                item
            ),
            root.join("keep")
        );
        std::fs::remove_dir_all(root).unwrap();
    }
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
