use super::{egui, DiskWeaveApp, Item, OpenAction, OpenRequest, PathBuf, SizeDisplay, Snapshot};
use crate::{
    file_open::TargetKind,
    list_selection::ListSelection,
    search::{item_bytes, item_modified, SearchInput, SortField},
    visible_watch::{Entry, FolderWatch},
};
use std::{
    collections::{BTreeMap, HashMap},
    ffi::OsString,
    path::Path,
};

struct Row {
    name: OsString,
    current: Entry,
    known: Option<Item>,
    bytes: Option<u64>,
}

#[derive(PartialEq, Eq)]
struct Key {
    folder: PathBuf,
    version: (u64, u64),
    generation: u64,
    sort: SortField,
    descending: bool,
}

#[derive(Default)]
pub(super) struct LiveRows {
    key: Option<Key>,
    rows: Vec<Row>,
    selection: ListSelection,
    seed: Option<Vec<PathBuf>>,
}

impl LiveRows {
    pub(super) fn restore(&mut self, paths: Vec<PathBuf>) {
        *self = Self {
            seed: Some(paths),
            ..Self::default()
        };
    }

    fn paths(&self, folder: &Path) -> Option<Vec<PathBuf>> {
        if !self.key.as_ref().is_some_and(|key| key.folder == folder) {
            return self.seed.clone();
        }
        Some(
            self.selection
                .rows()
                .map(|row| folder.join(&self.rows[row].name))
                .collect(),
        )
    }

    fn order(
        &mut self,
        watch: &FolderWatch,
        source: &Snapshot,
        folder: usize,
        query: &SearchInput,
        initial: impl FnOnce() -> Vec<PathBuf>,
    ) -> bool {
        let Some(entries) = watch.entries() else {
            return false;
        };
        let scan = &source.scan;
        let root = &scan.folders[folder].path;
        let key = Key {
            folder: root.clone(),
            version: watch.version(),
            generation: source.generation,
            sort: query.sort,
            descending: query.descending,
        };
        if self.key.as_ref() == Some(&key) {
            return true;
        }
        let previous = self.key.as_ref().is_some_and(|key| key.folder == *root);
        let mut selected = BTreeMap::new();
        if previous {
            for index in self.selection.rows() {
                let row = &self.rows[index];
                selected.insert(
                    row.name.clone(),
                    Some((row.current.folder, row.current.created)),
                );
            }
        } else {
            for path in self.seed.take().unwrap_or_else(initial) {
                if path.parent() == Some(root.as_path()) {
                    if let Some(name) = path.file_name() {
                        selected.insert(name.to_owned(), None);
                    }
                }
            }
        }
        let scanned_folders: HashMap<_, _> = scan.children[folder]
            .iter()
            .filter_map(|&id| scan.folders[id].path.file_name().map(|name| (name, id)))
            .collect();
        let scanned_files: HashMap<_, _> = scan.file_children[folder]
            .iter()
            .map(|&id| (scan.files[id].name.as_os_str(), id))
            .collect();
        let mut rows: Vec<_> = entries
            .iter()
            .map(|(name, current)| {
                let known = if current.folder {
                    scanned_folders
                        .get(name.as_os_str())
                        .copied()
                        .filter(|&id| scan.folders[id].created == current.created)
                        .map(Item::Folder)
                } else {
                    scanned_files
                        .get(name.as_os_str())
                        .copied()
                        .filter(|&id| scan.files[id].created == current.created)
                        .map(Item::File)
                };
                let bytes = if current.folder {
                    known.map(|item| item_bytes(scan, item))
                } else {
                    Some(current.bytes)
                };
                (
                    name.to_string_lossy().to_lowercase(),
                    Row {
                        name: name.clone(),
                        current: current.clone(),
                        known,
                        bytes,
                    },
                )
            })
            .collect();
        rows.sort_unstable_by(|(left_name, left), (right_name, right)| {
            let order = match query.sort {
                SortField::Name | SortField::Path => left_name.cmp(right_name),
                SortField::Size => left.bytes.cmp(&right.bytes),
                SortField::Modified => left.current.modified.cmp(&right.current.modified),
            };
            right
                .current
                .folder
                .cmp(&left.current.folder)
                .then(if query.descending {
                    order.reverse()
                } else {
                    order
                })
                .then_with(|| left_name.cmp(right_name))
                .then_with(|| left.name.cmp(&right.name))
        });
        self.selection.clear();
        self.rows = rows
            .into_iter()
            .enumerate()
            .map(|(i, (_, row))| {
                if selected.get(&row.name).is_some_and(|old| {
                    old.is_none_or(|old| old == (row.current.folder, row.current.created))
                }) {
                    self.selection.insert(i);
                }
                row
            })
            .collect();
        self.key = Some(key);
        true
    }

    fn single(&self) -> Option<&Row> {
        (self.selection.count() == 1)
            .then(|| {
                self.selection
                    .rows()
                    .next()
                    .and_then(|index| self.rows.get(index))
            })
            .flatten()
    }
}

pub(super) fn watch_status(ui: &mut egui::Ui, watch: &FolderWatch) {
    let (label, color) = if watch.error().is_some() {
        ("목록 갱신 불가", egui::Color32::LIGHT_RED)
    } else if watch.entries().is_some() {
        ("목록 자동 갱신", egui::Color32::from_rgb(116, 207, 160))
    } else {
        ("목록 확인 중…", crate::ui_style::MUTED)
    };
    ui.horizontal(|ui| {
        ui.add_space(10.0);
        let info = ui.colored_label(color, label);
        if let Some(error) = watch.error() {
            info.on_hover_text(error);
            ui.add(egui::Label::new(error).truncate());
        } else {
            let note = "용량 통계·전체 검색은 마지막 스캔 기준";
            ui.add(egui::Label::new(egui::RichText::new(note).weak()).truncate())
                .on_hover_text(note);
        }
    });
}

impl DiskWeaveApp {
    pub(super) fn sync_visible_watches(&mut self, ctx: &egui::Context, both: bool) {
        if self.folder_refresh_disabled
            || self.archive_record.is_some()
            || self.desktop.life.hidden
            || self.file_close_requested
            || ctx.input(|i| i.viewport().minimized.unwrap_or(false))
        {
            self.visible_watches.clear();
            return;
        }
        let mut visible = Vec::with_capacity(2);
        if let Some(path) = self.current_live_folder() {
            visible.push(path);
        }
        if both {
            if let Some((source, folder)) = self
                .folder_tabs
                .other()
                .and_then(|view| self.parked_live_source(view))
            {
                visible.push(source.scan.folders[folder].path.clone());
            }
        }
        self.visible_watches.sync(&visible, ctx);
    }

    fn parked_live_source(&self, view: &crate::navigation::ViewState) -> Option<(Snapshot, usize)> {
        if view.tab != 0 || view.query.is_active() || view.whole_scan || view.global {
            return None;
        }
        let source = self.scan.completed_snapshot(view.location)?;
        let (folder, _) = view.resolve_folder_on(&source).ok()?;
        (!source.scan.folders[folder].access.unreadable()).then_some((source, folder))
    }

    fn current_live_folder(&self) -> Option<PathBuf> {
        if self.folder_refresh_disabled
            || self.archive_record.is_some()
            || self.show_preview()
            || self.tab != 0
            || self.search.whole_scan
            || self.locations_view.global_mode
            || self.search.input.is_active()
        {
            return None;
        }
        let source = self.source()?;
        let folder = &source.scan.folders[self.explorer.as_ref()?.folder()];
        (!folder.access.unreadable()).then(|| folder.path.clone())
    }

    pub(super) fn live_selection_paths(&self) -> Option<Vec<PathBuf>> {
        let folder = self.current_live_folder()?;
        let entries = self.visible_watches.get(&folder)?.entries()?;
        let mut paths = self.live_rows.paths(&folder)?;
        paths.retain(|path| {
            path.file_name()
                .is_some_and(|name| entries.contains_key(name))
        });
        Some(paths)
    }

    pub(super) fn live_folder_writable(&self) -> bool {
        self.current_live_folder()
            .and_then(|path| self.visible_watches.get(&path))
            .is_none_or(|watch| watch.error().is_none())
    }

    pub(super) fn live_selection_count(&self) -> Option<usize> {
        self.live_selected_item()
            .map(|_| self.live_rows.selection.count())
    }

    pub(super) fn live_selected_item(&self) -> Option<Option<Item>> {
        let path = self.current_live_folder()?;
        let entries = self.visible_watches.get(&path)?.entries()?;
        let source = self.source()?;
        self.live_rows
            .key
            .as_ref()
            .filter(|key| key.folder == path && key.generation == source.generation)?;
        if self.live_rows.selection.count() == 0 {
            return Some(
                self.explorer
                    .as_ref()
                    .map(|view| Item::Folder(view.folder())),
            );
        }
        Some(
            self.live_rows
                .single()
                .filter(|row| {
                    entries.get(&row.name).is_some_and(|entry| {
                        entry.created == row.current.created && entry.folder == row.current.folder
                    })
                })
                .and_then(|row| row.known),
        )
    }

    pub(super) fn live_select_hit(&mut self, (id, generation, item): super::Target) {
        let Some(path) = self.current_live_folder() else {
            return;
        };
        if self
            .source()
            .is_none_or(|source| source.id != id || source.generation != generation)
            || self
                .live_rows
                .key
                .as_ref()
                .is_none_or(|key| key.folder != path || key.generation != generation)
        {
            return;
        }
        if let Some(index) = self
            .live_rows
            .rows
            .iter()
            .position(|row| row.known == Some(item))
        {
            self.live_rows.selection.click(index, false, false);
        }
    }

    pub(super) fn live_keyboard_open(&mut self) -> bool {
        let Some(folder) = self.current_live_folder() else {
            return false;
        };
        if self
            .visible_watches
            .get(&folder)
            .and_then(FolderWatch::entries)
            .is_none()
            || self
                .live_rows
                .key
                .as_ref()
                .is_none_or(|key| key.folder != folder)
        {
            return false;
        }
        if let Some(row) = self.live_rows.single() {
            self.open_live_item(folder.join(&row.name), row.current.folder, row.known, None);
        }
        true
    }

    pub(super) fn live_details(&self, ui: &mut egui::Ui) -> bool {
        let Some(folder) = self.current_live_folder() else {
            return false;
        };
        if self
            .live_rows
            .key
            .as_ref()
            .is_none_or(|key| key.folder != folder)
        {
            return false;
        }
        let Some(entries) = self
            .visible_watches
            .get(&folder)
            .and_then(FolderWatch::entries)
        else {
            return false;
        };
        if self.live_rows.selection.count() > 1 {
            let picked: Vec<_> = self
                .live_rows
                .selection
                .rows()
                .filter_map(|index| entries.get(&self.live_rows.rows[index].name))
                .collect();
            let folders = picked.iter().filter(|entry| entry.folder).count();
            ui.strong(format!("{}개 선택", picked.len()));
            crate::ui_style::property(ui, "파일", &format!("{}개", picked.len() - folders));
            crate::ui_style::property(ui, "폴더", &format!("{folders}개"));
            return true;
        }
        let Some(row) = self.live_rows.single() else {
            return false;
        };
        let Some(entry) = entries.get(&row.name) else {
            return false;
        };
        if entry.folder && row.known.is_some() {
            return false;
        }
        if let (Some(Item::File(id)), Some(source)) = (row.known, self.source()) {
            let file = &source.scan.files[id];
            if file.logical_bytes == entry.bytes && file.modified == entry.modified {
                return false;
            }
        }
        ui.strong(row.name.to_string_lossy());
        let path = folder.join(&row.name).display().to_string();
        ui.add(egui::Label::new(&path).truncate())
            .on_hover_text(&path);
        crate::ui_style::property(
            ui,
            "크기",
            &if entry.folder {
                "미분석".into()
            } else {
                SizeDisplay::new(entry.bytes).text
            },
        );
        crate::ui_style::property(ui, "수정 날짜", &crate::local_date::display(entry.modified));
        true
    }

    pub(super) fn draw_live_folder(&mut self, ui: &mut egui::Ui, source: &Snapshot) -> bool {
        let Some(folder_path) = self.current_live_folder() else {
            return false;
        };
        let Some(watch) = self.visible_watches.get(&folder_path) else {
            return false;
        };
        watch_status(ui, watch);
        let Some(folder) = self.explorer.as_ref().map(|view| view.folder()) else {
            return false;
        };
        let seed = || {
            self.search.restoring_paths().unwrap_or_else(|| {
                self.search
                    .job
                    .results()
                    .map(|result| {
                        self.search
                            .selection
                            .rows()
                            .filter_map(|row| result.items.get(row))
                            .map(|&item| crate::search::item_path(&source.scan, item))
                            .collect()
                    })
                    .unwrap_or_default()
            })
        };
        if !self
            .live_rows
            .order(watch, source, folder, &self.search.input, seed)
        {
            return false;
        }
        self.search.headers(ui);
        if self.live_rows.rows.is_empty() {
            ui.weak("이 폴더는 비어 있습니다.");
        }
        let writable = self.file_actions_available();
        let mut open = None;
        let mut open_tab = None;
        let widths = &self.search.widths;
        let cells = widths.total(
            self.search.show_path,
            self.search.show_modified,
            self.search.show_allocated,
        );
        if !self.search.editing
            && ui.input_mut(|i| i.consume_key(egui::Modifiers::CTRL, egui::Key::A))
        {
            self.live_rows.selection.all(self.live_rows.rows.len());
        }
        let scroll = egui::ScrollArea::both()
            .id_salt("live_folder_rows")
            .horizontal_scroll_offset(self.search.horizontal_offset)
            .vertical_scroll_offset(self.search.vertical_offset)
            .max_height((ui.available_height() - 26.0).max(20.0))
            .show_rows(
                ui,
                crate::ui_style::ROW,
                self.live_rows.rows.len(),
                |ui, rows| {
                    for index in rows {
                        let row = &self.live_rows.rows[index];
                        let selected = self.live_rows.selection.contains(index);
                        let full_path = folder_path.join(&row.name);
                        let response = crate::ui_style::row(ui, index, cells, selected, |ui| {
                            crate::ui_style::name_cell(
                                ui,
                                widths.get(0),
                                &row.name.to_string_lossy(),
                                row.current.folder,
                            );
                            if let Some(item) = row.known.filter(|&item| {
                                item_modified(&source.scan, item) == row.current.modified
                                    && Some(item_bytes(&source.scan, item)) == row.bytes
                            }) {
                                widths.cells(
                                    ui,
                                    &source.scan,
                                    item,
                                    self.search.show_path.then_some(false),
                                    self.search.show_modified,
                                    self.search.show_allocated,
                                );
                            } else {
                                let amount = row
                                    .bytes
                                    .map_or_else(|| "—".into(), |n| SizeDisplay::new(n).text);
                                crate::ui_style::cell(ui, widths.get(2), &amount, true);
                                if self.search.show_allocated {
                                    crate::ui_style::cell(ui, widths.get(7), "—", true);
                                }
                                crate::ui_style::cell(ui, widths.get(4), "—", true);
                                if self.search.show_modified {
                                    crate::ui_style::cell(
                                        ui,
                                        widths.get(3),
                                        &crate::local_date::display(row.current.modified),
                                        false,
                                    );
                                }
                                for column in [5, 6] {
                                    crate::ui_style::cell(ui, widths.get(column), "—", true);
                                }
                                if self.search.show_path {
                                    crate::ui_style::cell(
                                        ui,
                                        widths.get(1),
                                        &full_path.display().to_string(),
                                        false,
                                    )
                                    .on_hover_text(full_path.display().to_string());
                                }
                            }
                        })
                        .interact(egui::Sense::click_and_drag());
                        if response.clicked() {
                            let modifiers = ui.input(|i| i.modifiers);
                            self.live_rows
                                .selection
                                .click(index, modifiers.ctrl, modifiers.shift);
                        }
                        if response.secondary_clicked() && !self.live_rows.selection.contains(index)
                        {
                            self.live_rows.selection.click(index, false, false);
                        }
                        if response.double_clicked() {
                            open = Some((index, None));
                        }
                        if writable && response.drag_started() {
                            let paths = if selected {
                                self.live_rows.paths(&folder_path).unwrap_or_default()
                            } else {
                                vec![full_path.clone()]
                            };
                            response.dnd_set_drag_payload(crate::file_ui::DragPayload { paths });
                        }
                        if writable && row.current.folder {
                            crate::file_ui::drop_hint(ui, &response);
                            if let Some(payload) =
                                response.dnd_release_payload::<crate::file_ui::DragPayload>()
                            {
                                self.file_ui.drop_requested =
                                    Some((payload.paths.clone(), full_path));
                            }
                        }
                        response.context_menu(|ui| {
                            if watch.error().is_none() && self.live_rows.selection.count() <= 1 {
                                if let Some(Item::Folder(id)) = row.known {
                                    if let Some(right) = super::folder_open_menu(ui, true) {
                                        open_tab = Some((id, right));
                                    }
                                }
                                let kind = if row.current.folder {
                                    TargetKind::Folder
                                } else {
                                    TargetKind::File
                                };
                                if let Some(action) = super::open_menu(ui, kind) {
                                    open = Some((index, Some(action)));
                                }
                            }
                            self.file_ui.menu(
                                ui,
                                writable,
                                None,
                                self.live_rows.selection.count(),
                                true,
                            );
                        });
                    }
                },
            );
        self.search.horizontal_offset = scroll.state.clamped_offset().x;
        self.search.vertical_offset = scroll.state.clamped_offset().y;
        self.detail_target = self
            .live_rows
            .single()
            .and_then(|row| row.known)
            .map(|item| (source.id, source.generation, item));
        ui.horizontal(|ui| {
            ui.small(format!(
                "{}개 항목 · {}개 선택",
                self.live_rows.rows.len(),
                self.live_rows.selection.count()
            ));
            if self.live_rows.selection.count() > 0 {
                ui.menu_button("선택 항목 작업", |ui| {
                    self.file_ui
                        .menu(ui, writable, None, self.live_rows.selection.count(), true);
                });
            }
        });
        if let Some((index, action)) = open {
            let row = &self.live_rows.rows[index];
            self.open_live_item(
                folder_path.join(&row.name),
                row.current.folder,
                row.known,
                action,
            );
        }
        if let Some((folder, right)) = open_tab {
            self.input_error = self
                .open_folder_tab(source, folder, right)
                .err()
                .map(|error| error.to_string());
        }
        true
    }

    fn open_live_item(
        &mut self,
        path: PathBuf,
        folder: bool,
        known: Option<Item>,
        action: Option<OpenAction>,
    ) {
        if path
            .parent()
            .and_then(|parent| self.visible_watches.get(parent))
            .is_none_or(|watch| watch.error().is_some())
        {
            return;
        }
        if action.is_none() {
            if let Some(Item::Folder(id)) = known {
                self.navigate(id, true);
                return;
            }
        }
        let (kind, default_action) = if folder {
            (TargetKind::Folder, OpenAction::OpenFolder)
        } else {
            (TargetKind::File, OpenAction::OpenFile)
        };
        self.start_open(OpenRequest::new(
            path,
            kind,
            action.unwrap_or(default_action),
        ));
    }

    pub(super) fn draw_parked_live_rows(
        &mut self,
        ui: &mut egui::Ui,
    ) -> Option<Option<(PathBuf, bool)>> {
        let view = self.folder_tabs.other()?;
        let (source, folder) = self.parked_live_source(view)?;
        let path = &source.scan.folders[folder].path;
        let watch = self.visible_watches.get(path)?;
        if !self
            .parked_live_rows
            .order(watch, &source, folder, &view.query, || {
                view.selected.clone()
            })
        {
            return None;
        }
        let label = super::folders::pane_path_label(ui, path, ui.available_width());
        ui.add(egui::Label::new(label).truncate())
            .on_hover_text(path.display().to_string());
        watch_status(ui, watch);
        let mut picked = None;
        let live = &self.parked_live_rows;
        let scroll = egui::ScrollArea::vertical()
            .id_salt("parked_live_rows")
            .vertical_scroll_offset(view.scroll[1])
            .show_rows(ui, crate::ui_style::ROW, live.rows.len(), |ui, range| {
                for index in range {
                    let row = &live.rows[index];
                    let width = ui.available_width();
                    let response = crate::ui_style::row(
                        ui,
                        index,
                        width,
                        live.selection.contains(index),
                        |ui| {
                            crate::ui_style::name_cell(
                                ui,
                                (width - 80.0).max(0.0),
                                &row.name.to_string_lossy(),
                                row.current.folder,
                            );
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    ui.add_space(10.0);
                                    ui.small(row.bytes.map_or_else(
                                        || "—".into(),
                                        |bytes| SizeDisplay::new(bytes).text,
                                    ));
                                },
                            );
                        },
                    );
                    if response.clicked() || response.secondary_clicked() {
                        picked = Some((path.join(&row.name), response.double_clicked()));
                    }
                }
            });
        if live.rows.is_empty() {
            ui.weak("이 폴더는 비어 있습니다.");
        }
        self.folder_tabs
            .set_other_vertical_scroll(scroll.state.clamped_offset().y);
        Some(picked)
    }

    pub(super) fn pick_parked_live_row(&mut self, path: PathBuf, open: bool) {
        let Some(parent) = path.parent() else { return };
        if open {
            let Some(name) = path.file_name() else { return };
            let Some(current) = self
                .visible_watches
                .get(parent)
                .and_then(FolderWatch::entries)
                .and_then(|rows| rows.get(name))
            else {
                return;
            };
            let known = self.source().and_then(|source| {
                let relative = path.strip_prefix(&source.scan.folders[0].path).ok()?;
                let id = source.scan.folder_index(relative).ok()?;
                (current.folder && source.scan.folders[id].created == current.created)
                    .then_some(Item::Folder(id))
            });
            self.open_live_item(path, current.folder, known, None);
        } else {
            self.live_rows.restore(vec![path]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::*;
    use std::{fs, time::Instant};

    fn analyze(root: &std::path::Path) -> DiskWeaveApp {
        let mut app = DiskWeaveApp::new();
        app.scan.start(root.to_owned()).unwrap();
        let until = Instant::now() + Duration::from_secs(5);
        while app.scan.is_running() {
            app.sync_scan();
            assert!(Instant::now() < until);
            std::thread::sleep(Duration::from_millis(1));
        }
        app
    }

    fn draw(app: &mut DiskWeaveApp, ctx: &egui::Context, width: f32) -> Vec<String> {
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(width, 700.),
            )),
            ..Default::default()
        };
        let mut frame = ctx.run_ui(input, |ui| app.folder_workspace(ui));
        frame.textures_delta.clear();
        frame
            .shapes
            .into_iter()
            .filter_map(|shape| match shape.shape {
                egui::Shape::Text(text) => Some(text.galley.text().to_owned()),
                _ => None,
            })
            .collect()
    }

    fn observe(app: &mut DiskWeaveApp, ctx: &egui::Context, present: &[&str], absent: &[&str]) {
        let deadline = Instant::now() + Duration::from_secs(4);
        loop {
            let labels = draw(app, ctx, 1400.);
            let has = |text: &&str| labels.iter().any(|label| label == text);
            if present.iter().all(has) && !absent.iter().any(has) {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "{present:?}를 찾지 못함: {labels:?}"
            );
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    #[test]
    fn live_folder_changes_reach_the_display_without_a_reanalysis() {
        let fixture = crate::duplicates::tests::Fixture::new();
        let mut app = analyze(&fixture.0);
        let snapshot = app.source().unwrap().scan;
        let ctx = egui::Context::default();
        crate::ui_style::apply(&ctx);
        let mut wait = |present: &[&str], absent: &[&str]| observe(&mut app, &ctx, present, absent);
        wait(&["목록 자동 갱신"], &[]);
        let original = fixture.0.join("update-tracer.txt");
        let renamed = fixture.0.join("renamed-tracer.txt");
        let mut timing = [[Duration::ZERO; 5]; 4];
        for trial in 0..5 {
            fs::write(&original, b"123").unwrap();
            let start = Instant::now();
            wait(&["update-tracer.txt", "3 B"], &[]);
            timing[0][trial] = start.elapsed();
            fs::write(&original, vec![1; 4096]).unwrap();
            let start = Instant::now();
            wait(&["update-tracer.txt", "4.0 KiB"], &["3 B"]);
            timing[1][trial] = start.elapsed();
            fs::rename(&original, &renamed).unwrap();
            let start = Instant::now();
            wait(&["renamed-tracer.txt"], &["update-tracer.txt"]);
            timing[2][trial] = start.elapsed();
            fs::remove_file(&renamed).unwrap();
            let start = Instant::now();
            wait(&["이 폴더는 비어 있습니다."], &["renamed-tracer.txt"]);
            timing[3][trial] = start.elapsed();
        }
        assert!(Arc::ptr_eq(&snapshot, &app.source().unwrap().scan));
        for (action, mut samples) in ["생성", "수정", "이름 변경", "삭제"]
            .into_iter()
            .zip(timing)
        {
            samples.sort_unstable();
            let ms = |value: Duration| value.as_secs_f64() * 1000.0;
            eprintln!(
                "{action}: 중앙값 {:.2}ms · 범위 {:.2}–{:.2}ms (5회)",
                ms(samples[2]),
                ms(samples[0]),
                ms(samples[4]),
            );
        }
    }

    #[test]
    fn live_watch_follows_visible_panes_and_reconciles_returning_tabs() {
        let fixture = crate::duplicates::tests::Fixture::new();
        let left = fixture.0.join("left");
        let right = fixture.0.join("right");
        let hidden = fixture.0.join("hidden");
        for path in [&left, &right, &hidden] {
            fs::create_dir(path).unwrap();
        }
        fs::write(left.join("keep.txt"), [0; 20]).unwrap();
        fs::write(right.join("old-right.txt"), [0; 100]).unwrap();
        for n in 0..80 {
            fs::write(left.join(format!("z-list-{n:02}.txt")), b"z").unwrap();
        }
        let mut app = analyze(&fixture.0);
        let source = app.source().unwrap();
        let index = |name| {
            source
                .scan
                .folder_index(std::path::Path::new(name))
                .unwrap()
        };
        app.navigate(index("left"), true);
        app.open_folder_tab(&source, index("right"), true).unwrap();
        app.focus_folder_side(Side::Left);
        let ctx = egui::Context::default();
        crate::ui_style::apply(&ctx);
        observe(&mut app, &ctx, &["keep.txt", "old-right.txt"], &[]);
        let right_instance = app.visible_watches.get(&right).unwrap().version().0;
        app.live_rows.selection.click(0, false, false);

        draw(&mut app, &ctx, 550.);
        assert!(app.visible_watches.get(&left).is_some());
        assert!(app.visible_watches.get(&right).is_none());
        fs::rename(right.join("old-right.txt"), right.join("new-right.txt")).unwrap();
        fs::write(left.join("a-first.txt"), [0; 30]).unwrap();
        app.search.input.sort = crate::search::SortField::Name;
        app.search.input.descending = false;
        observe(
            &mut app,
            &ctx,
            &["a-first.txt", "keep.txt", "new-right.txt"],
            &["old-right.txt"],
        );
        assert_eq!(app.live_rows.rows[0].name, "a-first.txt");
        assert_eq!(
            app.live_selection_paths(),
            Some(vec![left.join("keep.txt")])
        );
        assert_ne!(
            app.visible_watches.get(&right).unwrap().version().0,
            right_instance
        );

        fs::write(left.join("late-left.txt"), b"late").unwrap();
        app.open_folder_tab(&source, index("hidden"), false)
            .unwrap();
        observe(
            &mut app,
            &ctx,
            &["목록 자동 갱신"],
            &["late-left.txt", "new-right.txt"],
        );
        assert!(app.visible_watches.get(&left).is_none());
        assert!(app.visible_watches.get(&right).is_none());
        app.switch_folder_tab(0);
        observe(&mut app, &ctx, &["late-left.txt", "new-right.txt"], &[]);
        assert_eq!(
            app.live_selection_paths(),
            Some(vec![left.join("keep.txt")])
        );

        app.search.vertical_offset = 240.0;
        draw(&mut app, &ctx, 1400.);
        let scrolled = app.search.vertical_offset;
        assert!(scrolled > 0.0);
        fs::write(left.join("zz-new.txt"), b"z").unwrap();
        let deadline = Instant::now() + Duration::from_secs(4);
        while app.live_rows.rows.len() != 84 {
            draw(&mut app, &ctx, 1400.);
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(app.search.vertical_offset, scrolled);

        app.folder_refresh_disabled = true;
        let labels = draw(&mut app, &ctx, 1400.);
        assert!(app.visible_watches.get(&left).is_none());
        assert!(app.visible_watches.get(&right).is_none());
        assert!(!labels.iter().any(|label| label == "목록 자동 갱신"));
        assert!(Arc::ptr_eq(&source.scan, &app.source().unwrap().scan));
    }

    #[test]
    fn opening_watched_analyzed_folders_stays_in_the_analysis() {
        let fixture = crate::duplicates::tests::Fixture::new();
        let left = fixture.0.join("left");
        let right = fixture.0.join("right");
        let child = right.join("right-child");
        fs::create_dir(&left).unwrap();
        fs::create_dir_all(&child).unwrap();

        let mut app = analyze(&fixture.0);
        let source = app.source().unwrap();
        let index = |name| {
            source
                .scan
                .folder_index(std::path::Path::new(name))
                .unwrap()
        };
        app.open_folder_tab(&source, index("right"), true).unwrap();
        app.focus_folder_side(Side::Left);
        let ctx = egui::Context::default();
        crate::ui_style::apply(&ctx);
        observe(&mut app, &ctx, &["left", "right-child"], &[]);

        app.focus_folder_side(Side::Right);
        app.pick_parked_live_row(child.clone(), true);
        assert_eq!(app.capture_navigation().unwrap().folder, child);

        app.focus_folder_side(Side::Left);
        observe(&mut app, &ctx, &["left", "right"], &[]);
        app.live_select_hit((source.id, source.generation, Item::Folder(index("left"))));
        assert!(app.live_keyboard_open());
        assert_eq!(app.capture_navigation().unwrap().folder, left);
    }

    #[test]
    fn parked_watch_reports_initial_failure_while_preserving_the_scanned_rows() {
        let fixture = crate::duplicates::tests::Fixture::new();
        let right = fixture.0.join("right");
        fs::create_dir(&right).unwrap();
        fs::write(right.join("cached-right.txt"), b"cached").unwrap();

        let mut app = analyze(&fixture.0);
        let source = app.source().unwrap();
        let folder = source
            .scan
            .folder_index(std::path::Path::new("right"))
            .unwrap();
        app.open_folder_tab(&source, folder, true).unwrap();
        app.focus_folder_side(Side::Left);
        fs::rename(&right, fixture.0.join("outside-view")).unwrap();

        let ctx = egui::Context::default();
        crate::ui_style::apply(&ctx);
        observe(&mut app, &ctx, &["cached-right.txt", "목록 갱신 불가"], &[]);
        assert!(app.other_folder_path().is_none());
    }

    #[test]
    fn tracked_results_and_visible_notifications_share_one_display_row() {
        let fixture = crate::duplicates::tests::Fixture::new();
        let mut app = analyze(&fixture.0);
        let original = app.source().unwrap();
        let ctx = egui::Context::default();
        crate::ui_style::apply(&ctx);
        observe(&mut app, &ctx, &["목록 자동 갱신"], &[]);

        fs::write(fixture.0.join("tracked.txt"), [0; 350]).unwrap();
        observe(&mut app, &ctx, &["tracked.txt", "350 B"], &[]);
        assert!(original.scan.files.is_empty());
        let now = std::time::SystemTime::now();
        let next = Arc::new(crate::scanner::scan(&fixture.0, || false).unwrap());
        let mut record = crate::snapshot::SavedScan::new(next.clone(), now, now, Duration::ZERO);
        record.tracking = Some(crate::usn::Checkpoint {
            version: 1,
            volume: "test".into(),
            root: 1,
            journal: 7,
            next: 100,
            folders: vec![1],
            files: vec![2],
            excluded: vec![],
            pending: vec![],
            unavailable: vec![],
            confirmed_after: None,
        });
        app.tracking
            .start_with(
                original.id,
                fixture.0.clone(),
                false,
                None,
                move |_, _, _, cancel, publish| {
                    publish(crate::usn::WatchUpdate::Record(Box::new(record)))?;
                    while !cancel() {
                        std::thread::sleep(Duration::from_millis(2));
                    }
                    Ok(())
                },
            )
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(4);
        loop {
            app.sync_tracking();
            if app.source().unwrap().generation != original.generation {
                break;
            }
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(1));
        }
        let labels = draw(&mut app, &ctx, 1400.);
        assert_eq!(
            labels
                .iter()
                .filter(|label| *label == "tracked.txt")
                .count(),
            1
        );
        assert_eq!(app.live_rows.rows.len(), 1);
        assert_eq!(app.source().unwrap().scan.folders[0].logical_bytes, 350);
        app.tracking.stop();
    }
}
