use crate::explorer::Item;
use crate::list_selection::ListSelection;
use crate::scanner::ScanResult;
use crate::search::{EntryKind, SearchInput, SearchMode, SortField};
use crate::search_job::{SearchController, SearchStatus};
use eframe::egui;
use std::sync::Arc;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Filter {
    Kind,
    Extensions,
    Size,
    Date,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SearchScope {
    Folder,
    Location,
    Registered,
}
impl SearchScope {
    pub fn label(self) -> &'static str {
        match self {
            Self::Folder => "이 폴더",
            Self::Location => "분석 대상 전체",
            Self::Registered => "모든 분석 위치",
        }
    }
}

type LiveSelection = (
    SearchInput,
    usize,
    bool,
    Arc<ScanResult>,
    std::collections::HashSet<std::path::PathBuf>,
);

#[derive(Default)]
pub struct SearchView {
    live_selection: Option<LiveSelection>,
    pub show_source: bool,
    presentation: Option<(bool, bool, bool)>,
    pub show_allocated: bool,
    pub show_path: bool,
    pub show_modified: bool,
    pub widths: crate::presentation::ColumnWidths,
    pub horizontal_offset: f32,
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
    pub fn rebase(&mut self, old: &ScanResult, new: &Arc<ScanResult>, folder: usize) {
        let paths = if let Some(pending) = self.live_selection.take() {
            pending.4
        } else {
            self.selection
                .rows()
                .filter_map(|row| {
                    self.job
                        .results()?
                        .items
                        .get(row)
                        .map(|&item| crate::search::item_path(old, item))
                })
                .collect()
        };
        self.invalidate();
        self.refresh(new, folder);
        let scope = if self.whole_scan { 0 } else { folder };
        self.live_selection = Some((
            self.input.clone(),
            scope,
            self.whole_scan,
            Arc::clone(new),
            paths,
        ));
    }
    pub fn selection_summary(&self) -> crate::list_selection::SelectionSummary {
        crate::list_selection::SelectionSummary::from_items(
            self.selection
                .rows()
                .filter_map(|row| self.job.results()?.items.get(row).copied()),
        )
    }
    pub fn sync_presentation(&mut self, map_collapsed: &mut bool) -> bool {
        self.widths.result_mode = self.input.is_active();
        match (self.input.is_active(), self.presentation) {
            (true, None) => {
                self.presentation = Some((*map_collapsed, self.show_path, self.show_source));
                *map_collapsed = true;
                self.show_path = true;
                self.show_source = true;
                true
            }
            (false, Some((map, path, source))) => {
                *map_collapsed = map;
                self.show_path = path;
                self.show_source = source;
                self.presentation = None;
                true
            }
            _ => false,
        }
    }
    pub fn scope(&self, global: bool) -> SearchScope {
        if global {
            SearchScope::Registered
        } else if self.whole_scan {
            SearchScope::Location
        } else {
            SearchScope::Folder
        }
    }
    pub fn clear_conditions(&mut self) {
        let (sort, descending) = (self.input.sort, self.input.descending);
        self.input = SearchInput {
            sort,
            descending,
            ..Default::default()
        };
        self.selection.clear();
    }
    pub fn clear_filter(&mut self, filter: Filter) {
        match filter {
            Filter::Kind => self.input.kind = EntryKind::All,
            Filter::Extensions => self.input.extensions.clear(),
            Filter::Size => {
                self.input.min_size.clear();
                self.input.max_size.clear();
            }
            Filter::Date => {
                self.input.from_date.clear();
                self.input.to_date.clear();
            }
        }
        self.selection.clear();
    }
    pub fn chips(&self) -> Vec<(Filter, String)> {
        let mut chips = Vec::new();
        if self.input.kind != EntryKind::All {
            chips.push((
                Filter::Kind,
                if self.input.kind == EntryKind::Files {
                    "파일만"
                } else {
                    "폴더만"
                }
                .into(),
            ));
        }
        if !self.input.extensions.is_empty() {
            chips.push((
                Filter::Extensions,
                format!("확장자: {}", self.input.extensions),
            ));
        }
        if !self.input.min_size.is_empty() || !self.input.max_size.is_empty() {
            chips.push((
                Filter::Size,
                match (
                    self.input.min_size.is_empty(),
                    self.input.max_size.is_empty(),
                ) {
                    (false, true) => format!("{} B 이상", self.input.min_size),
                    (true, false) => format!("{} B 이하", self.input.max_size),
                    _ => format!("{} ~ {} B", self.input.min_size, self.input.max_size),
                },
            ));
        }
        if !self.input.from_date.is_empty() || !self.input.to_date.is_empty() {
            chips.push((
                Filter::Date,
                match (
                    self.input.from_date.is_empty(),
                    self.input.to_date.is_empty(),
                ) {
                    (false, true) => format!("수정일: {}부터", self.input.from_date),
                    (true, false) => format!("수정일: {}까지", self.input.to_date),
                    _ => format!("수정일: {} ~ {}", self.input.from_date, self.input.to_date),
                },
            ));
        }
        chips
    }
    pub fn focus_item(&mut self, item: Item) {
        self.focus_after_analysis = Some(item);
    }
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
        self.invalidate();
        self.input = SearchInput::default();
        self.whole_scan = false;
    }
    pub fn invalidate(&mut self) {
        self.live_selection = None;
        self.job.cancel();
        self.selection.clear();
        self.last_request = None;
        self.focus_after_analysis = None;
        self.reveal_row = None;
    }

    pub fn poll(&mut self) {
        if self.job.poll() {
            if let Some((input, scope, whole, scan, paths)) = self.live_selection.take() {
                if self.last_request.as_ref() == Some(&(input, scope, whole)) {
                    if let Some(results) = self.job.results() {
                        for (row, &item) in results.items.iter().enumerate() {
                            if paths.contains(&crate::search::item_path(&scan, item)) {
                                self.selection.click(row, true, false);
                            }
                        }
                    }
                }
            }
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
        let scope = if self.whole_scan { 0 } else { folder };
        let key = (self.input.clone(), scope, self.whole_scan);
        if self.last_request.as_ref() != Some(&key) {
            self.live_selection = None;
            self.selection.clear();
            self.reveal_row = None;
            self.focus_after_analysis = None;
            self.job.submit(Arc::clone(scan), self.input.clone(), scope);
            self.last_request = Some(key);
        }
    }

    pub fn controls(&mut self, ui: &mut egui::Ui, global: bool) -> Option<SearchScope> {
        self.editing = false;
        let current = self.scope(global);
        let mut chosen = None;
        let width = crate::workspace::SearchLayout::new(ui.available_width() + 24.0);
        ui.spacing_mut().button_padding = egui::vec2(8.0, 5.0);
        crate::ui_style::slot(
            ui,
            egui::vec2(width.scope, 32.0),
            egui::Layout::left_to_right(egui::Align::Center),
            |ui| {
                ui.menu_button(format!("{} ▾", current.label()), |ui| {
                    for scope in [
                        SearchScope::Folder,
                        SearchScope::Location,
                        SearchScope::Registered,
                    ] {
                        if ui
                            .selectable_label(current == scope, scope.label())
                            .clicked()
                        {
                            chosen = Some(scope);
                            ui.close();
                        }
                    }
                    ui.small("스캔이 끝난 위치에서만 검색합니다.");
                });
            },
        );
        self.editing |= ui
            .add(
                egui::TextEdit::singleline(&mut self.input.text)
                    .id(egui::Id::unique("search_query"))
                    .hint_text("파일이나 폴더 검색")
                    .desired_width((ui.available_width() - width.filter - 26.0).max(48.0))
                    .margin(egui::vec2(8.0, 6.0)),
            )
            .has_focus();
        ui.menu_button(
            if self.input.is_active() {
                "필터 •"
            } else {
                "필터"
            },
            |ui| {
                ui.set_min_width(300.0);
                ui.spacing_mut().item_spacing.y = 7.0;
                self.options(ui);
                ui.separator();
                self.filter_fields(ui);
            },
        );
        chosen
    }
    pub fn filter_chips(&mut self, ui: &mut egui::Ui) {
        egui::ScrollArea::horizontal()
            .id_salt("active_search_filters")
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.spacing_mut().button_padding = egui::vec2(8.0, 3.0);
                    ui.spacing_mut().interact_size.y = 24.0;
                    if ui
                        .button(if self.input.mode == SearchMode::Name {
                            "이름으로 찾기"
                        } else {
                            "경로로 찾기"
                        })
                        .clicked()
                    {
                        self.input.mode = if self.input.mode == SearchMode::Name {
                            SearchMode::Path
                        } else {
                            SearchMode::Name
                        };
                    }
                    ui.checkbox(&mut self.input.recursive, "하위 폴더 포함");
                    for (filter, label) in self.chips() {
                        if ui
                            .button(format!("{label} ×"))
                            .on_hover_text("이 조건 해제")
                            .clicked()
                        {
                            self.clear_filter(filter);
                        }
                    }
                    if ui
                        .add_enabled(
                            self.input.is_active()
                                || self.input.mode != SearchMode::Name
                                || !self.input.recursive,
                            egui::Button::new("검색 초기화").frame(false),
                        )
                        .clicked()
                    {
                        self.clear_conditions();
                    }
                });
            });
    }
    fn options(&mut self, ui: &mut egui::Ui) {
        ui.horizontal_wrapped(|ui| {
            ui.selectable_value(&mut self.input.mode, SearchMode::Name, "이름");
            ui.selectable_value(&mut self.input.mode, SearchMode::Path, "전체 경로");
            ui.checkbox(&mut self.input.recursive, "하위 폴더 포함");
            if ui.button("검색 초기화").clicked() {
                self.clear_conditions();
            }
        });
    }
    fn filter_fields(&mut self, ui: &mut egui::Ui) {
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
            ui.label("최소 크기 (B)");
            self.editing |= ui
                .add(egui::TextEdit::singleline(&mut self.input.min_size).desired_width(100.0))
                .has_focus();
            ui.label("최대 크기 (B)");
            self.editing |= ui
                .add(egui::TextEdit::singleline(&mut self.input.max_size).desired_width(100.0))
                .has_focus();
        });
        ui.horizontal_wrapped(|ui| {
            ui.label("수정 날짜 · 시작일과 종료일 포함");
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
    }

    pub fn column_controls(&mut self, ui: &mut egui::Ui) {
        ui.menu_button("열 표시", |ui| {
            ui.checkbox(&mut self.show_allocated, "할당 크기");
            ui.checkbox(&mut self.show_path, "전체 경로");
            ui.add_enabled_ui(self.show_path, |ui| {
                ui.checkbox(&mut self.show_source, "분석 위치 표시");
            });
            ui.checkbox(&mut self.show_modified, "수정 날짜");
        });
    }
    pub fn headers(&mut self, ui: &mut egui::Ui) {
        self.widths.fit(
            ui.available_width(),
            self.show_path,
            self.show_modified,
            self.show_allocated,
        );
        let height = if ui.available_height() < 160.0 {
            30.0
        } else {
            crate::ui_style::HEADER
        };
        let scroll = egui::ScrollArea::horizontal()
            .id_salt("list_headers")
            .horizontal_scroll_offset(self.horizontal_offset)
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 0.0;
                    for (column, field, label) in [
                        (0, Some(SortField::Name), "이름"),
                        (2, Some(SortField::Size), "크기"),
                        (7, None, "할당 크기"),
                        (4, None, "상위 폴더 대비"),
                        (5, None, "파일"),
                        (6, None, "폴더"),
                        (
                            1,
                            Some(SortField::Path),
                            if self.show_source {
                                "전체 경로 · 분석 위치"
                            } else {
                                "전체 경로"
                            },
                        ),
                        (3, Some(SortField::Modified), "수정 날짜"),
                    ]
                    .into_iter()
                    {
                        if self.widths.result_mode && matches!(column, 4..=6) {
                            continue;
                        }
                        if column == 1 && !self.show_path
                            || column == 3 && !self.show_modified
                            || column == 7 && !self.show_allocated
                        {
                            continue;
                        }
                        let suffix = if Some(self.input.sort) == field {
                            if self.input.descending {
                                " ↓"
                            } else {
                                " ↑"
                            }
                        } else {
                            ""
                        };
                        let (rect, response) = ui.allocate_exact_size(
                            egui::vec2(self.widths.get(column), height),
                            egui::Sense::click(),
                        );
                        ui.painter()
                            .rect_filled(rect, 0.0, egui::Color32::from_rgb(21, 30, 40));
                        ui.painter().line_segment(
                            [rect.left_bottom(), rect.right_bottom()],
                            egui::Stroke::new(1.0, crate::ui_style::LINE),
                        );
                        crate::ui_style::text(
                            ui,
                            rect.shrink2(egui::vec2(12.0, 0.0)),
                            &format!("{label}{suffix}"),
                            !matches!(column, 0 | 1 | 3),
                            crate::ui_style::MUTED,
                            12.0,
                        );
                        let edge = egui::Rect::from_min_max(
                            egui::pos2(response.rect.right() - 6.0, response.rect.top()),
                            response.rect.right_bottom(),
                        );
                        let resize = ui
                            .interact(
                                edge,
                                ui.scope_id().with(("column_resize", column)),
                                egui::Sense::drag(),
                            )
                            .on_hover_cursor(egui::CursorIcon::ResizeHorizontal);
                        if resize.dragged() {
                            self.widths.set(
                                column,
                                self.widths.get(column) + ui.input(|i| i.pointer.delta().x),
                            );
                        }
                        if response.clicked() && !resize.dragged() && !resize.hovered() {
                            let Some(field) = field else {
                                continue;
                            };
                            if self.input.sort == field {
                                self.input.descending = !self.input.descending;
                            } else {
                                self.input.sort = field;
                                self.input.descending = false;
                            }
                        }
                    }
                });
            });
        self.horizontal_offset = scroll.state.clamped_offset().x;
    }

    pub fn status(&mut self, ui: &mut egui::Ui) {
        match self.job.status() {
            SearchStatus::Running => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label("검색 중…");
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
                    ui.small(format!(
                        "{}개 항목 · {}개 선택{}",
                        results.items.len(),
                        self.selection.count(),
                        if results.unknown_dates > 0 {
                            format!(" · 수정 날짜 확인 불가 {}개 제외", results.unknown_dates)
                        } else {
                            String::new()
                        }
                    ));
                }
            }
            SearchStatus::Idle => {}
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn live_search_keeps_conditions_and_surviving_selection_after_resorting() {
        let root =
            std::env::temp_dir().join(format!("diskweave-search-live-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("kept.bin"), b"123").unwrap();
        let old = Arc::new(crate::scanner::scan(&root, || false).unwrap());
        let mut view = SearchView::default();
        view.input.text = "bin".into();
        view.refresh(&old, 0);
        wait(&mut view);
        view.selection.click(0, false, false);
        std::fs::write(root.join("new.bin"), b"123456789").unwrap();
        let new = Arc::new(crate::scanner::scan(&root, || false).unwrap());
        view.rebase(&old, &new, 0);
        wait(&mut view);
        assert_eq!(view.input.text, "bin");
        let selected = view
            .selection
            .single(&view.job.results().unwrap().items)
            .unwrap();
        assert_eq!(
            crate::search::item_path(&new, selected),
            root.join("kept.bin")
        );
        std::fs::remove_file(root.join("kept.bin")).unwrap();
        let next = Arc::new(crate::scanner::scan(&root, || false).unwrap());
        view.rebase(&new, &next, 0);
        wait(&mut view);
        assert_eq!(view.selection.count(), 0);
        std::fs::remove_dir_all(root).unwrap();
    }
    use super::*;
    #[test]
    fn search_path_is_visible_by_default_without_horizontal_scroll_in_a_wide_list() {
        let mut view = SearchView::default();
        view.input.text = "model".into();
        let mut map = false;
        view.sync_presentation(&mut map);
        view.widths.fit(
            830.0,
            view.show_path,
            view.show_modified,
            view.show_allocated,
        );
        assert!(
            view.widths
                .total(view.show_path, view.show_modified, view.show_allocated)
                <= 831.0
        );
        assert!(view.widths.get(1) >= 280.0);
    }
    #[test]
    fn result_presentation_restores_map_and_analysis_columns_after_search() {
        let mut view = SearchView::default();
        let mut map = false;
        view.input.text = "model".into();
        assert!(view.sync_presentation(&mut map));
        assert!(map);
        assert!(view.show_path && view.show_source);
        map = false;
        assert!(!view.sync_presentation(&mut map));
        assert!(!map);
        view.clear_conditions();
        assert!(view.sync_presentation(&mut map));
        assert!(!map);
        assert!(!view.show_path && !view.show_source);
        map = true;
        view.input.kind = EntryKind::Files;
        view.sync_presentation(&mut map);
        view.clear_conditions();
        view.sync_presentation(&mut map);
        assert!(map);
    }
    #[test]
    fn clearing_filters_preserves_scope_sort_and_invalid_values_until_removed() {
        let mut view = SearchView {
            whole_scan: true,
            ..Default::default()
        };
        view.input.text = "model".into();
        view.input.extensions = "npy;nii.gz".into();
        view.input.min_size = "bad".into();
        view.input.max_size = "12".into();
        view.input.from_date = "2026-11-15".into();
        view.input.to_date = "2026-11-16".into();
        view.input.mode = SearchMode::Path;
        view.input.recursive = false;
        view.input.sort = SortField::Name;
        view.input.descending = false;
        view.selection.click(0, false, false);
        assert!(view.chips().iter().any(|(_, label)| label.contains("bad")));
        view.clear_filter(Filter::Size);
        assert!(view.input.min_size.is_empty() && view.input.max_size.is_empty());
        assert_eq!(view.input.extensions, "npy;nii.gz");
        assert_eq!(view.selection.count(), 0);
        view.clear_filter(Filter::Date);
        assert!(view.input.from_date.is_empty() && view.input.to_date.is_empty());
        view.clear_conditions();
        assert!(view.whole_scan);
        assert!(!view.input.is_active());
        assert_eq!(view.input.sort, SortField::Name);
        assert!(!view.input.descending);
        assert_eq!(view.input.mode, SearchMode::Name);
        assert!(view.input.recursive);
    }
    #[test]
    fn result_invalidation_preserves_query_during_background_location_refresh() {
        let mut view = SearchView::default();
        view.input.text = "model".into();
        view.input.extensions = "npy".into();
        view.selection.click(0, false, false);
        let expected = view.input.clone();
        view.invalidate();
        assert_eq!(view.input, expected);
        assert_eq!(view.selection.count(), 0);
        assert!(matches!(view.job.status(), SearchStatus::Cancelled));
    }
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
