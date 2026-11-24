use super::{
    accent, egui, item_name, DiskWeaveApp, Item, OpenAction, OpenRequest, PaneMemory, SearchInput,
    Side, SizeDisplay, Snapshot, Target,
};
use std::{path::PathBuf, time::Duration};

pub(super) fn pane_path_label(ui: &egui::Ui, path: &std::path::Path, width: f32) -> String {
    let full = path.display().to_string();
    let text = ui.painter().layout_no_wrap(
        full.clone(),
        egui::FontId::proportional(13.0),
        crate::ui_style::TEXT,
    );
    if text.size().x <= width {
        return full;
    }
    let Some(name) = path.file_name() else {
        return full;
    };
    match path.parent().and_then(std::path::Path::file_name) {
        Some(parent) => format!(
            "…\\{}\\{}",
            parent.to_string_lossy(),
            name.to_string_lossy()
        ),
        None => name.to_string_lossy().into_owned(),
    }
}

impl DiskWeaveApp {
    pub(super) fn remove_registered_location(&mut self, id: u64) -> std::io::Result<()> {
        let current = if self.scan.selected_id() == Some(id)
            && (self.folder_tabs.len() > 1 || self.folder_tabs.is_split())
        {
            self.capture_navigation()
        } else {
            None
        };
        self.scan.remove(id)?;
        if let Some(view) = current {
            self.clear_folder_pane();
            self.unavailable_pane = Some(view);
            self.input_error = Some("이 화면의 분석 위치가 제거됐습니다".into());
        }
        Ok(())
    }
    fn park_folder(&mut self) -> PaneMemory {
        PaneMemory {
            view: self.capture_navigation(),
            history: std::mem::take(&mut self.history),
        }
    }

    fn load_folder(&mut self, pane: PaneMemory) {
        self.other_rows.clear();
        self.parked_live_rows = Default::default();
        self.live_rows = Default::default();
        self.history = pane.history;
        self.unavailable_pane = None;
        self.input_error = None;
        let Some(view) = pane.view else {
            self.clear_folder_pane();
            return;
        };
        let resolved = self
            .scan
            .completed_snapshot(view.location)
            .ok_or_else(|| {
                std::io::Error::other("이 화면의 분석 위치가 제거됐거나 스캔 결과가 없습니다")
            })
            .and_then(|source| view.resolve_on(&source));
        match resolved {
            Ok(resolved) => self.restore_folder_view(view, resolved),
            Err(error) => {
                self.clear_folder_pane();
                self.unavailable_pane = Some(view);
                self.input_error = Some(error.to_string());
            }
        }
    }

    fn clear_folder_pane(&mut self) {
        self.scan.clear_selection();
        self.explorer = None;
        self.search.reset();
        self.locations_view.set_global_mode(false);
        self.tab = 0;
        self.detail_target = None;
    }

    pub(super) fn open_folder_tab(
        &mut self,
        source: &Snapshot,
        folder: usize,
        right: bool,
    ) -> std::io::Result<()> {
        let target = source
            .scan
            .folders
            .get(folder)
            .filter(|folder| !folder.access.unreadable())
            .ok_or_else(|| std::io::Error::other("이 폴더의 목록을 열 수 없습니다"))?;
        if self.archive_record.is_some()
            || self
                .scan
                .get(source.id)
                .is_none_or(|entry| entry.generation != source.generation)
        {
            return Err(std::io::Error::other(
                "분석 결과가 변경되었습니다. 폴더를 다시 선택하세요",
            ));
        }
        let view = crate::navigation::ViewState {
            location: source.id,
            folder: target.path.clone(),
            query: SearchInput {
                sort: self.search.input.sort,
                descending: self.search.input.descending,
                ..SearchInput::default()
            },
            whole_scan: false,
            global: false,
            selected: Vec::new(),
            scroll: [0.0; 2],
            tab: 0,
        };
        let current = self.park_folder();
        if right {
            self.folder_tabs.open_right(current);
        } else {
            self.folder_tabs.open_tab(current);
        }
        self.load_folder(PaneMemory::at(view));
        Ok(())
    }

    pub(super) fn switch_folder_tab(&mut self, index: usize) {
        if index != self.folder_tabs.selected() {
            let current = self.park_folder();
            let pane = self.folder_tabs.select_tab(index, current);
            self.load_folder(pane);
        }
    }

    pub(super) fn close_folder_tab(&mut self, index: usize) {
        if self.folder_tabs.len() > 1 {
            if let Some(pane) = self.folder_tabs.close_tab(index) {
                self.load_folder(pane);
            }
        }
    }

    pub(super) fn focus_folder_side(&mut self, side: Side) {
        if self.folder_tabs.is_split() && self.folder_tabs.side() != side {
            let current = self.park_folder();
            let pane = self.folder_tabs.select_other(current);
            self.load_folder(pane);
        }
    }

    pub(super) fn close_folder_split(&mut self) {
        self.other_rows.clear();
        if let Some(pane) = self.folder_tabs.close_right() {
            self.load_folder(pane);
        }
    }

    pub(super) fn other_folder_path(&self) -> Option<PathBuf> {
        let view = self.folder_tabs.other().filter(|view| !view.global)?;
        if self.scan.get(view.location)?.stale {
            return None;
        }
        let source = self.scan.completed_snapshot(view.location)?;
        let (folder, _) = view.resolve_folder_on(&source).ok()?;
        let path = &source.scan.folders[if view.whole_scan { 0 } else { folder }].path;
        if self
            .visible_watches
            .get(path)
            .is_some_and(|watch| watch.error().is_some())
        {
            return None;
        }
        Some(path.clone())
    }
    fn folder_tab_bar(&mut self, ui: &mut egui::Ui) {
        if self.folder_tabs.len() == 1 {
            return;
        }
        let active = self.folder_tabs.selected();
        let source = self.source();
        let active_path = source
            .as_ref()
            .zip(self.explorer.as_ref())
            .map(|(source, explorer)| source.scan.folders[explorer.folder()].path.as_path())
            .or_else(|| {
                self.unavailable_pane
                    .as_ref()
                    .map(|view| view.folder.as_path())
            });
        let mut switch = None;
        let mut close = None;
        egui::ScrollArea::horizontal()
            .id_salt("folder_tab_bar")
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 3.0;
                    for index in 0..self.folder_tabs.len() {
                        let path = if index == active {
                            active_path
                        } else {
                            self.folder_tabs
                                .tab_view(index)
                                .map(|view| view.folder.as_path())
                        };
                        let name = path.map_or_else(
                            || "폴더".into(),
                            |path| {
                                path.file_name()
                                    .unwrap_or(path.as_os_str())
                                    .to_string_lossy()
                            },
                        );
                        let tab = ui.add(
                            egui::Button::new(name.as_ref())
                                .truncate()
                                .selected(index == active),
                        );
                        if tab
                            .on_hover_text(
                                path.map(|path| path.display().to_string())
                                    .unwrap_or_default(),
                            )
                            .clicked()
                        {
                            switch = Some(index);
                        }
                        if ui.small_button("×").on_hover_text("폴더 탭 닫기").clicked() {
                            close = Some(index);
                        }
                        ui.separator();
                    }
                });
            });
        if let Some(index) = close {
            self.close_folder_tab(index);
        } else if let Some(index) = switch {
            self.switch_folder_tab(index);
        }
    }

    pub(super) fn folder_workspace(&mut self, ui: &mut egui::Ui) {
        use crate::workspace::folders::{split_widths, SPLIT_DIVIDER};
        self.folder_tab_bar(ui);
        if self.inline_search.visible {
            self.visible_watches.clear();
            self.center(ui);
            return;
        }
        let tab_id = self.folder_tabs.id();
        if !self.folder_tabs.is_split() {
            self.sync_visible_watches(ui.ctx(), false);
            if self.folder_tabs.len() == 1 {
                self.center(ui);
            } else {
                ui.push_id(("folder_tab", tab_id), |ui| self.center(ui));
            }
            return;
        }

        let fraction = self.split_fraction.unwrap_or(0.5);
        let widths = split_widths(ui.available_width(), fraction);
        ui.horizontal(|ui| {
            for (side, label) in [(Side::Left, "왼쪽"), (Side::Right, "오른쪽")] {
                if ui
                    .selectable_label(self.folder_tabs.side() == side, label)
                    .clicked()
                {
                    self.focus_folder_side(side);
                }
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("분할 닫기").clicked() {
                    self.close_folder_split();
                }
            });
        });
        if !self.folder_tabs.is_split() || widths.is_none() {
            self.sync_visible_watches(ui.ctx(), false);
            self.other_rows.clear();
            ui.push_id(("folder_tab", tab_id, self.folder_tabs.side()), |ui| {
                self.center(ui)
            });
            return;
        }

        self.sync_visible_watches(ui.ctx(), true);
        let [left, right] = widths.expect("두 목록을 배치할 수 있는 너비");
        let (area, _) = ui.allocate_exact_size(ui.available_size(), egui::Sense::hover());
        let divider = egui::Rect::from_min_size(
            area.min + egui::vec2(left, 0.0),
            egui::vec2(SPLIT_DIVIDER, area.height()),
        );
        let handle = ui
            .interact(
                divider,
                ui.scope_id().with("folder_split_divider"),
                egui::Sense::drag(),
            )
            .on_hover_cursor(egui::CursorIcon::ResizeHorizontal);
        if let Some(pointer) = handle.interact_pointer_pos().filter(|_| handle.dragged()) {
            self.split_fraction =
                Some(((pointer.x - area.left()) / (area.width() - SPLIT_DIVIDER)).clamp(0.0, 1.0));
        }
        ui.painter()
            .rect_filled(divider, 0.0, crate::ui_style::SIDEBAR);
        ui.painter().rect_filled(
            egui::Rect::from_center_size(divider.center(), egui::vec2(3.0, 30.0)),
            2.0,
            crate::ui_style::SOFT,
        );

        let active = self.folder_tabs.side();
        let rect_for = |side| match side {
            Side::Left => egui::Rect::from_min_size(area.min, egui::vec2(left, area.height())),
            Side::Right => {
                egui::Rect::from_min_size(divider.right_top(), egui::vec2(right, area.height()))
            }
        };
        // 활성 목록의 드래그가 시작된 뒤 반대쪽 대상 영역을 판정하도록 표시 순서 고정.
        for side in [active, active.other()] {
            if !self.folder_tabs.is_split() {
                break;
            }
            let rect = rect_for(side);
            let mut child = ui.new_child(
                egui::UiBuilder::new()
                    .id_salt(("folder_tab", tab_id, side))
                    .max_rect(rect),
            );
            child.set_clip_rect(ui.clip_rect().intersect(rect));
            if side == active {
                child
                    .painter()
                    .hline(rect.x_range(), rect.top(), egui::Stroke::new(2.0, accent()));
                child.add_space(2.0);
                self.center(&mut child);
            } else {
                self.parked_folder(&mut child, rect, side);
            }
        }
    }

    fn parked_folder(&mut self, ui: &mut egui::Ui, rect: egui::Rect, side: Side) {
        let mut picked = None;
        let mut live_pick = None;
        if let Some(picked) = self.draw_parked_live_rows(ui) {
            live_pick = picked;
        } else if let Some(view) = self.folder_tabs.other() {
            self.other_rows.refresh(view, &self.scan);
            if let Some(path) = self.other_rows.folder(view.whole_scan) {
                egui::Frame::new()
                    .inner_margin(egui::Margin::symmetric(10, 8))
                    .show(ui, |ui| {
                        ui.add(
                            egui::Label::new(if view.global {
                                "모든 스캔 위치".into()
                            } else {
                                pane_path_label(ui, path, ui.available_width())
                            })
                            .truncate(),
                        )
                        .on_hover_text(path.display().to_string());
                    });
                if let Some(watch) = self
                    .visible_watches
                    .get(path)
                    .filter(|watch| watch.error().is_some())
                {
                    super::folder_updates::watch_status(ui, watch);
                }
                if view.query.is_active() {
                    ui.add(egui::Label::new(format!("검색 · {}", view.query.text)).truncate());
                }
                if self.other_rows.running() {
                    ui.small("목록 갱신 중…");
                    ui.ctx().request_repaint_after(Duration::from_millis(30));
                } else if let Some(error) = self.other_rows.error() {
                    ui.colored_label(egui::Color32::LIGHT_RED, error);
                } else {
                    let count = self.other_rows.len();
                    egui::Frame::new()
                        .fill(crate::ui_style::SURFACE)
                        .inner_margin(egui::Margin::symmetric(12, 5))
                        .show(ui, |ui| {
                            ui.horizontal(|ui| {
                                ui.label("이름");
                                ui.with_layout(
                                    egui::Layout::right_to_left(egui::Align::Center),
                                    |ui| {
                                        ui.label("크기");
                                    },
                                );
                            });
                        });
                    egui::ScrollArea::vertical()
                        .id_salt("inactive_rows")
                        .vertical_scroll_offset(view.scroll[1])
                        .show_rows(ui, crate::ui_style::ROW, count, |ui, rows| {
                            for row in rows {
                                let Some((source, item, selected)) = self.other_rows.row(row)
                                else {
                                    continue;
                                };
                                let width = ui.available_width();
                                let response =
                                    crate::ui_style::row(ui, row, width, selected, |ui| {
                                        crate::ui_style::name_cell(
                                            ui,
                                            (width - 82.0).max(0.0),
                                            &item_name(&source.scan, item).to_string_lossy(),
                                            matches!(item, Item::Folder(_)),
                                        );
                                        ui.with_layout(
                                            egui::Layout::right_to_left(egui::Align::Center),
                                            |ui| {
                                                ui.add_space(12.0);
                                                ui.small(
                                                    SizeDisplay::new(crate::search::item_bytes(
                                                        &source.scan,
                                                        item,
                                                    ))
                                                    .text,
                                                );
                                            },
                                        );
                                    });
                                if response.clicked() || response.secondary_clicked() {
                                    picked = Some((
                                        (source.id, source.generation, item),
                                        response.double_clicked(),
                                    ));
                                }
                            }
                        });
                    if count == 0 {
                        ui.weak("표시할 항목이 없습니다.");
                    }
                }
            } else {
                ui.weak("이 화면의 분석 위치가 제거됐거나 스캔 결과가 없습니다.");
            }
        }

        let hovered = ui.rect_contains_pointer(rect);
        let drop_target = (hovered
            && self.file_actions_available()
            && egui::DragAndDrop::has_payload_of_type::<crate::file_ui::DragPayload>(ui.ctx()))
        .then(|| self.other_folder_path())
        .flatten();
        if let Some(path) = drop_target {
            ui.painter().rect_stroke(
                rect,
                3.0,
                egui::Stroke::new(2.0, accent()),
                egui::StrokeKind::Inside,
            );
            if ui.input(|input| input.pointer.any_released()) {
                if let Some(payload) =
                    egui::DragAndDrop::take_payload::<crate::file_ui::DragPayload>(ui.ctx())
                {
                    self.file_ui.drop_requested = Some((payload.paths.clone(), path));
                }
            }
        } else if hovered
            && ui
                .input(|input| input.pointer.primary_clicked() || input.pointer.secondary_clicked())
        {
            self.focus_folder_side(side);
            if let Some((path, open)) = live_pick {
                self.pick_parked_live_row(path, open);
            } else if let Some((target, open)) = picked {
                self.pick_parked_row(target, open);
            }
            ui.ctx().request_repaint();
        }
    }

    fn pick_parked_row(&mut self, (id, generation, item): Target, open: bool) {
        let Some(source) = self
            .scan
            .completed_snapshot(id)
            .filter(|source| source.generation == generation)
        else {
            return;
        };
        if open {
            match item {
                Item::Folder(folder) if !self.locations_view.global_mode => {
                    self.navigate(folder, true)
                }
                Item::Folder(_) => self.analyze_at(id, generation, item),
                Item::File(_) => self.start_open(OpenRequest::for_item(
                    &source.scan,
                    item,
                    OpenAction::OpenFile,
                )),
            }
        } else {
            self.detail_target = Some((id, generation, item));
            if self.locations_view.global_mode {
                self.locations_view.restore_selection(
                    self.search.input.clone(),
                    [(id, crate::search::item_path(&source.scan, item))]
                        .into_iter()
                        .collect(),
                );
            } else {
                self.search.focus_item(item);
                if let Some(explorer) = &mut self.explorer {
                    explorer.select(item);
                }
            }
        }
    }
}
