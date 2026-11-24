mod folder_updates;
mod folders;
mod settings;

use crate::analysis::StatsScope;
use crate::analysis_panel::{AnalysisPanel, PanelKey};
use crate::explorer::{Explorer, Item};
use crate::file_open::{OpenAction, OpenController, OpenRequest, OpenStatus, TargetKind};
use crate::global_search::Hit;
use crate::locations::{Locations, Snapshot};
use crate::locations_ui::{LocationAction, LocationsView};
use crate::map_job::{MapController, Target};
use crate::presentation::{detail, SizeDisplay};
use crate::scan_job::ScanStatus;
use crate::scanner::ScanResult;
use crate::search::{item_name, SearchInput};
use crate::search_view::{ScopeChoice, SearchScope, SearchView};
use crate::target_selection::TargetSelection;
use crate::treemap::MapScope;
use crate::treemap_view::{MapEvent, MapView};
use crate::workspace::folders::{FolderTabs, PaneMemory, Side};
use eframe::egui;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

#[derive(Clone, Copy, PartialEq, Eq)]
enum SearchSurface {
    Quick,
    Inline,
}

struct QuickFolderScan {
    surface: SearchSurface,
    location: u64,
    generation: u64,
    hit: crate::quick_search::Hit,
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum SizeFreshness {
    LastScan,
    Refreshing,
    NeedsRefresh,
}
impl SizeFreshness {
    fn label(self) -> &'static str {
        match self {
            Self::LastScan => "마지막 스캔 기준",
            Self::Refreshing => "갱신 중 · 이전 값",
            Self::NeedsRefresh => "갱신 필요",
        }
    }
}

#[derive(Default)]
pub struct DiskWeaveApp {
    settings_path: Option<PathBuf>,
    settings_error: Option<String>,
    settings_draft: Option<settings::Draft>,
    desktop_settings_error: Option<String>,
    settings_saved: Option<crate::app_settings::UiSettings>,
    settings_attempt: Option<crate::app_settings::UiSettings>,
    settings_pending: Option<(crate::app_settings::UiSettings, std::time::Instant)>,
    window_state: Option<crate::app_settings::WindowState>,
    window_restore: Option<(crate::app_settings::WindowState, u8)>,
    history: crate::navigation::History,
    folder_refresh_disabled: bool,
    visible_watches: crate::visible_watch::VisibleWatches,
    live_rows: folder_updates::LiveRows,
    parked_live_rows: folder_updates::LiveRows,
    folder_tabs: FolderTabs,
    unavailable_pane: Option<crate::navigation::ViewState>,
    other_rows: crate::workspace::pane_rows::PaneRows,
    split_fraction: Option<f32>,
    duplicates: crate::duplicate_job::Controller,
    duplicate_view: crate::duplicate_ui::DuplicateUi,
    quick: crate::quick_search::QuickSearch,
    quick_ui: crate::quick_ui::QuickUi,
    inline_search: crate::quick_search::QuickSearch,
    inline_search_ui: crate::quick_ui::QuickUi,
    quick_folder_scan: Option<QuickFolderScan>,
    desktop: crate::quick_native::Desktop,
    link_details: crate::link_details::Controller,
    preview_view: crate::scanner::preview::View,
    preview_location: Option<u64>,
    file_close_requested: bool,
    file_job: crate::file_job::Controller,
    file_ui: crate::file_ui::FileUi,
    file_refresh: crate::file_refresh::Controller,
    tracking: crate::tracking::Controller,
    results: crate::results_ui::ResultsUi,
    saved_viewer: Option<Box<DiskWeaveApp>>,
    archive_record: Option<Arc<crate::snapshot::SavedScan>>,
    statistics: AnalysisPanel,
    scan: Locations,
    target: TargetSelection,
    root_replacement: Option<crate::locations::RootReplacement>,
    input_error: Option<String>,
    explorer: Option<Explorer>,
    file_open: OpenController,
    search: SearchView,
    locations_view: LocationsView,
    map: MapController,
    map_view: MapView,
    map_collapsed: bool,
    map_height: f32,
    detail_target: Option<Target>,
    details_open: bool,
    tab: usize,
    logo: Option<egui::TextureHandle>,
    detail_context: Option<(u64, Option<u64>, bool, usize, SearchInput, bool)>,
}

pub fn run() -> eframe::Result {
    let settings_path = crate::quick_settings::path();
    let settings = settings_path
        .as_ref()
        .map_err(|e| e.to_string())
        .and_then(|p| crate::app_settings::load(p).map_err(|e| e.to_string()));
    let icon =
        eframe::icon_data::from_png_bytes(include_bytes!("../assets/brand/diskweave-icon.png"))
            .expect("프로젝트 아이콘 PNG");
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("DiskWeave")
            .with_decorations(false)
            .with_inner_size([1380.0, 880.0])
            .with_min_inner_size([760.0, 500.0])
            .with_drag_and_drop(true)
            .with_icon(icon),
        ..Default::default()
    };
    eframe::run_native(
        "DiskWeave",
        options,
        Box::new(|creation| {
            crate::ui_style::apply(&creation.egui_ctx);
            let mut app = DiskWeaveApp::new();
            app.settings_path = settings_path.ok();
            match settings {
                Ok(settings) => app.apply_ui_settings(settings.ui),
                Err(e) => {
                    app.settings_error = Some(format!("화면 설정을 불러오지 못했습니다: {e}"))
                }
            }
            Ok(Box::new(app))
        }),
    )
}
fn bg() -> egui::Color32 {
    crate::ui_style::BG
}
fn surface() -> egui::Color32 {
    crate::ui_style::SIDEBAR
}
fn accent() -> egui::Color32 {
    crate::ui_style::ACCENT
}
fn type_chart(ui: &mut egui::Ui, stats: &crate::analysis::Statistics) {
    let mut groups: Vec<_> = stats
        .extensions
        .iter()
        .take(4)
        .enumerate()
        .map(|(i, e)| {
            (
                e.name.as_str(),
                e.bytes,
                [
                    egui::Color32::from_rgb(97, 185, 245),
                    egui::Color32::from_rgb(140, 145, 239),
                    egui::Color32::from_rgb(118, 211, 175),
                    egui::Color32::from_rgb(213, 164, 106),
                ][i],
            )
        })
        .collect();
    let other = stats.bytes - groups.iter().map(|(_, n, _)| n).sum::<u64>();
    if other > 0 {
        groups.push(("기타", other, egui::Color32::from_rgb(70, 93, 114)));
    }
    let stacked = ui.available_width() < 270.0;
    let height = if stacked {
        118.0 + groups.len() as f32 * 21.0
    } else {
        108.0
    };
    let (area, _) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), height),
        egui::Sense::hover(),
    );
    let diameter = 108.0;
    let center = if stacked {
        area.center_top() + egui::vec2(0.0, 54.0)
    } else {
        area.left_center() + egui::vec2(diameter / 2.0, 0.0)
    };
    let radius = 43.0;
    ui.painter().circle_stroke(
        center,
        radius,
        egui::Stroke::new(20.0, crate::ui_style::LINE),
    );
    let mut start = -std::f32::consts::FRAC_PI_2;
    if stats.bytes > 0 {
        for &(_, bytes, color) in &groups {
            let angle = bytes as f64 / stats.bytes as f64 * std::f64::consts::TAU;
            let steps = (angle * 20.0).ceil().max(1.0) as usize;
            let points = (0..=steps)
                .map(|i| {
                    let a = start + angle as f32 * i as f32 / steps as f32;
                    center + egui::vec2(a.cos(), a.sin()) * radius
                })
                .collect();
            ui.painter()
                .add(egui::Shape::line(points, egui::Stroke::new(20.0, color)));
            start += angle as f32;
        }
    }
    ui.painter().text(
        center,
        egui::Align2::CENTER_CENTER,
        SizeDisplay::new(stats.bytes).text,
        egui::FontId::proportional(14.0),
        crate::ui_style::TEXT,
    );
    let legend = egui::Rect::from_min_max(
        area.min
            + if stacked {
                egui::vec2(0.0, 118.0)
            } else {
                egui::vec2(122.0, 0.0)
            },
        area.max,
    );
    for (i, (name, bytes, color)) in groups.into_iter().enumerate() {
        let row = egui::Rect::from_min_size(
            legend.min + egui::vec2(0.0, i as f32 * 21.0),
            egui::vec2(legend.width(), 21.0),
        );
        ui.painter().rect_filled(
            egui::Rect::from_min_size(
                row.left_center() - egui::vec2(0.0, 3.5),
                egui::vec2(7.0, 7.0),
            ),
            2.0,
            color,
        );
        crate::ui_style::text(
            ui,
            egui::Rect::from_min_max(
                row.min + egui::vec2(14.0, 0.0),
                row.max - egui::vec2(43.0, 0.0),
            ),
            name,
            false,
            crate::ui_style::MUTED,
            11.0,
        );
        crate::ui_style::text(
            ui,
            row,
            &crate::presentation::percentage(bytes, stats.bytes),
            true,
            crate::ui_style::TEXT,
            11.0,
        );
    }
}
fn allocation_summary(ui: &mut egui::Ui, stats: &crate::analysis::Statistics) {
    ui.collapsing("디스크 사용량과 계산 기준", |ui| {
        for (label, total) in [
            ("디스크 할당 크기 합계", stats.allocation),
            ("Hard Link 중복 제외", stats.unique_allocation),
        ] {
            let shown = crate::presentation::allocation_display(total);
            ui.horizontal_wrapped(|ui| {
                ui.small(label);
                ui.label(shown.text).on_hover_text(shown.exact);
            });
        }
        if stats.identity_conflicts > 0 {
            ui.colored_label(
                egui::Color32::YELLOW,
                format!(
                    "같은 파일 ID의 할당 크기가 다른 {}개 항목 제외",
                    stats.identity_conflicts
                ),
            );
        }
        ui.small("기본 파일 데이터 기준입니다. 이 값이 삭제 후 확보되는 공간을 뜻하지는 않습니다.");
    });
}

fn open_menu(ui: &mut egui::Ui, kind: TargetKind) -> Option<OpenAction> {
    let mut action = None;
    match kind {
        TargetKind::Folder => {
            if crate::ui_style::menu_action(ui, "탐색기에서 열기", true).clicked() {
                action = Some(OpenAction::OpenFolder);
            }
        }
        TargetKind::File => {
            if crate::ui_style::menu_action(ui, "기본 앱으로 열기", true).clicked() {
                action = Some(OpenAction::OpenFile);
            }
            if crate::ui_style::menu_action(ui, "파일 위치 열기", true).clicked() {
                action = Some(OpenAction::RevealFile);
            }
        }
    }
    if action.is_some() {
        ui.close();
    }
    action
}

fn folder_open_menu(ui: &mut egui::Ui, available: bool) -> Option<bool> {
    if !available {
        return None;
    }
    for (right, label) in [(false, "새 탭에서 열기"), (true, "오른쪽에 열기")] {
        if crate::ui_style::menu_action(ui, label, true).clicked() {
            ui.close();
            return Some(right);
        }
    }
    None
}
impl DiskWeaveApp {
    /// 현재 실행의 분석 위치. 이전 자동 저장소 접근 없이 제공.
    pub fn active_locations(&self) -> &Locations {
        &self.scan
    }
    #[cfg(test)]
    pub fn selected_path(&self) -> Option<&std::path::Path> {
        self.target.path()
    }
    fn show_preview(&self) -> bool {
        self.archive_record.is_none()
            && !self.locations_view.global_mode
            && !self.preview_view.previous
            && self.scan.preview().is_some()
    }
    fn preview_banner(&mut self, ui: &mut egui::Ui) {
        if self.archive_record.is_some()
            || self.locations_view.global_mode
            || self.scan.preview().is_none()
        {
            return;
        }
        let width = ui.available_width();
        egui::Frame::new()
            .fill(crate::ui_style::RAISED)
            .inner_margin(10)
            .show(ui, |ui| {
                ui.set_min_width((width - 20.0).max(0.0));
                ui.horizontal_wrapped(|ui| {
                    crate::ui_style::activity_indicator(ui);
                    ui.strong(if self.preview_view.previous {
                        "이전 분석 결과"
                    } else {
                        "폴더를 찾고 있습니다"
                    });
                    if self.scan.completed().is_some()
                        && ui
                            .small_button(if self.preview_view.previous {
                                "진행 상황 보기"
                            } else {
                                "이전 결과 보기"
                            })
                            .clicked()
                    {
                        self.preview_view.previous = !self.preview_view.previous;
                    }
                });
                ui.add_space(6.0);
                ui.small("현재까지 확인한 용량입니다. 스캔이 끝나면 크기순으로 정렬됩니다.");
            });
    }
    fn preview_tree(&mut self, ui: &mut egui::Ui) {
        let model = self.scan.preview().expect("선택 위치의 진행 모델");
        let view = &mut self.preview_view;
        view.sync(model);
        let mut open = None;
        let mut toggle = None;
        egui::ScrollArea::both().id_salt("preview_tree").show_rows(
            ui,
            33.0,
            view.visible().len(),
            |ui, rows| {
                for row in rows {
                    let (id, depth) = view.visible()[row];
                    let folder = &model.folders[id];
                    let width = ui.available_width().max(depth as f32 * 14.0 + 180.0);
                    let (rect, _) =
                        ui.allocate_exact_size(egui::vec2(width, 33.0), egui::Sense::hover());
                    let rect = rect.shrink2(egui::vec2(8.0, 0.5));
                    let arrow = egui::Rect::from_min_size(
                        rect.min + egui::vec2(depth as f32 * 14.0, 0.0),
                        egui::vec2(21.0, rect.height()),
                    );
                    let pick =
                        egui::Rect::from_min_max(egui::pos2(arrow.right(), rect.top()), rect.max);
                    let response = ui.interact(
                        pick,
                        ui.scope_id().with(("preview_folder", id)),
                        egui::Sense::click(),
                    );
                    if view.selected == id || response.hovered() {
                        ui.painter().rect_filled(
                            rect,
                            6.0,
                            if view.selected == id {
                                egui::Color32::from_rgb(34, 53, 72)
                            } else {
                                crate::ui_style::RAISED
                            },
                        );
                    }
                    if !folder.children.is_empty() {
                        crate::ui_style::chevron(ui, arrow, view.expanded(id));
                        if ui
                            .interact(
                                arrow,
                                ui.scope_id().with(("preview_toggle", id)),
                                egui::Sense::click(),
                            )
                            .clicked()
                        {
                            toggle = Some(id);
                        }
                    }
                    crate::ui_style::icon(ui, pick.shrink2(egui::vec2(3.0, 0.0)), true);
                    let name = folder
                        .path
                        .file_name()
                        .unwrap_or(folder.path.as_os_str())
                        .to_string_lossy();
                    crate::ui_style::text(
                        ui,
                        egui::Rect::from_min_max(
                            pick.min + egui::vec2(25.0, 0.0),
                            pick.max - egui::vec2(70.0, 0.0),
                        ),
                        &name,
                        false,
                        crate::ui_style::TEXT,
                        13.0,
                    );
                    crate::ui_style::text(
                        ui,
                        egui::Rect::from_min_max(
                            egui::pos2(rect.right() - 68.0, rect.top()),
                            rect.max,
                        ),
                        &SizeDisplay::new(folder.bytes).text,
                        true,
                        crate::ui_style::SOFT,
                        11.0,
                    );
                    if response.clicked() {
                        open = Some(id);
                    }
                    response.on_hover_text(format!(
                        "{}\n{} · {} Byte",
                        folder.path.display(),
                        folder.state().label(),
                        folder.bytes
                    ));
                }
            },
        );
        if let Some(id) = toggle {
            view.toggle(id);
        }
        if let Some(id) = open {
            view.open(model, id);
        }
    }
    fn preview_table(&mut self, ui: &mut egui::Ui) {
        let model = self.scan.preview().expect("선택 위치의 진행 모델");
        let view = &mut self.preview_view;
        view.sync(model);
        if model.folders.is_empty() {
            ui.add_space(20.0);
            egui::Frame::new()
                .inner_margin(egui::Margin::symmetric(10, 0))
                .show(ui, |ui| ui.weak("발견한 폴더를 여기에 표시합니다."));
            return;
        }
        let folder = &model.folders[view.folder];
        let mut open = None;
        ui.horizontal_wrapped(|ui| {
            if ui
                .add_enabled(folder.parent.is_some(), egui::Button::new("↑ 상위 폴더"))
                .clicked()
            {
                open = folder.parent;
            }
            ui.add(egui::Label::new(folder.path.display().to_string()).truncate())
                .on_hover_text(folder.path.display().to_string());
        });
        ui.add_space(8.0);
        let width = ui.available_width();
        ui.horizontal(|ui| {
            ui.strong("하위 폴더");
            ui.weak(format!("{}개 발견 · 발견순", folder.children.len()));
        });
        ui.separator();
        egui::ScrollArea::vertical()
            .id_salt(("preview_list", view.folder))
            .show_rows(ui, 42.0, folder.children.len(), |ui, rows| {
                for row in rows {
                    let id = folder.children[row];
                    let item = &model.folders[id];
                    let (rect, response) =
                        ui.allocate_exact_size(egui::vec2(width, 42.0), egui::Sense::click());
                    if view.selected == id || response.hovered() {
                        ui.painter()
                            .rect_filled(rect, 0.0, egui::Color32::from_rgb(26, 47, 63));
                    }
                    ui.painter().hline(
                        rect.x_range(),
                        rect.bottom(),
                        egui::Stroke::new(1.0, crate::ui_style::LINE),
                    );
                    crate::ui_style::icon(ui, rect.shrink2(egui::vec2(12.0, 0.0)), true);
                    let name = item
                        .path
                        .file_name()
                        .unwrap_or(item.path.as_os_str())
                        .to_string_lossy();
                    crate::ui_style::text(
                        ui,
                        egui::Rect::from_min_max(
                            rect.min + egui::vec2(38.0, 0.0),
                            egui::pos2(rect.left() + width * 0.52, rect.bottom()),
                        ),
                        &name,
                        false,
                        crate::ui_style::TEXT,
                        13.0,
                    );
                    crate::ui_style::text(
                        ui,
                        egui::Rect::from_min_max(
                            egui::pos2(rect.left() + width * 0.52, rect.top()),
                            egui::pos2(rect.right() - 105.0, rect.bottom()),
                        ),
                        &SizeDisplay::new(item.bytes).text,
                        true,
                        crate::ui_style::TEXT,
                        12.0,
                    );
                    crate::ui_style::text(
                        ui,
                        egui::Rect::from_min_max(
                            egui::pos2(rect.right() - 94.0, rect.top()),
                            rect.max - egui::vec2(8.0, 0.0),
                        ),
                        item.state().label(),
                        true,
                        crate::ui_style::SOFT,
                        11.0,
                    );
                    if response.clicked() {
                        view.selected = id;
                    }
                    if response.double_clicked() {
                        open = Some(id);
                    }
                    response.on_hover_text(format!(
                        "{}\n{} Byte · 파일 {}개 · 하위 폴더 {}개",
                        item.path.display(),
                        item.bytes,
                        item.files,
                        item.folders
                    ));
                }
            });
        if folder.children.is_empty() {
            ui.add_space(14.0);
            ui.weak("아직 발견한 하위 폴더가 없습니다.");
        }
        if let Some(id) = open {
            view.open(model, id);
        }
    }
    fn preview_details(&self, ui: &mut egui::Ui) {
        ui.strong("진행 중인 폴더");
        if let Some(folder) = self
            .scan
            .preview()
            .and_then(|m| m.folders.get(self.preview_view.selected))
        {
            ui.add(egui::Label::new(folder.path.display().to_string()).wrap());
            ui.separator();
            crate::ui_style::property(ui, "상태", folder.state().label());
            let size = SizeDisplay::new(folder.bytes);
            ui.label(size.text).on_hover_text(size.exact);
            crate::ui_style::property(ui, "확인한 파일", &format!("{}개", folder.files));
            crate::ui_style::property(ui, "확인한 하위 폴더", &format!("{}개", folder.folders));
        }
        ui.add_space(12.0);
        ui.small("일부 누락에는 접근할 수 없거나 상세 정보를 읽지 못한 항목이 포함됩니다.");
        ui.small("파일 목록·통계·할당 크기·공간 지도는 스캔 완료 후 확인할 수 있습니다.");
    }
    fn file_blocked(&self) -> bool {
        self.file_ui.blocked(&self.file_job)
            || self.file_refresh.busy()
            || self.duplicates.running()
    }
    fn copy_selection_paths(&self) -> Vec<PathBuf> {
        if let Some(paths) = self.live_selection_paths() {
            return paths;
        }
        if self.locations_view.global_mode {
            return self
                .locations_view
                .selected_targets()
                .iter()
                .filter_map(|(id, generation, item)| {
                    self.scan
                        .get(*id)
                        .filter(|e| e.generation == *generation)
                        .and_then(|e| e.scan.completed())
                        .map(|c| crate::search::item_path(&c.result, *item))
                })
                .collect();
        }
        let Some(source) = self.source() else {
            return vec![];
        };
        let mut paths: Vec<_> = self
            .search
            .job
            .results()
            .map(|r| {
                self.search
                    .selection
                    .rows()
                    .filter_map(|row| r.items.get(row).copied())
                    .map(|item| crate::search::item_path(&source.scan, item))
                    .collect()
            })
            .unwrap_or_default();
        if paths.is_empty() {
            if let Some((_, _, item)) = self.selection_target() {
                paths.push(crate::search::item_path(&source.scan, item));
            }
        }
        paths
    }
    fn file_actions_available(&self) -> bool {
        self.shared_file_actions_available() && self.live_folder_writable() && self.tab != 4
    }

    fn shared_file_actions_available(&self) -> bool {
        self.archive_record.is_none()
            && !self.duplicates.running()
            && !self.scan.is_running()
            && self.root_replacement.is_none()
            && !self.file_job.busy()
            && !self.file_refresh.busy()
            && self.file_ui.dialog.is_none()
            && !self.results.busy()
    }

    fn registered_path_writable(&self, path: &std::path::Path) -> bool {
        self.scan.entries.iter().all(|entry| {
            crate::file_ops::contains_path(&entry.path, path).is_ok_and(|contained| {
                !contained || !entry.stale && entry.scan.completed().is_some()
            })
        })
    }

    fn quick_recycle_context(&self, path: PathBuf) -> std::io::Result<crate::file_ui::Context> {
        if !self.shared_file_actions_available() {
            return Err(std::io::Error::other("진행 중인 작업을 먼저 마쳐 주세요"));
        }
        if !self.registered_path_writable(&path) {
            return Err(std::io::Error::other(
                "등록 위치의 목록을 갱신한 뒤 다시 시도해 주세요",
            ));
        }
        Ok(crate::file_ui::Context {
            paths: vec![path],
            current: None,
            protected: self
                .scan
                .entries
                .iter()
                .map(|entry| entry.path.clone())
                .collect(),
            writable: true,
        })
    }

    fn file_context(&self, explicit: Option<Vec<PathBuf>>) -> crate::file_ui::Context {
        let mut writable = self.file_actions_available();
        let paths = if let Some(paths) = explicit {
            writable &= paths.iter().all(|path| self.registered_path_writable(path));
            paths
        } else {
            let mut selected = if self.locations_view.global_mode {
                self.locations_view.selected_targets()
            } else {
                self.source()
                    .zip(self.search.job.results())
                    .map(|(source, results)| {
                        self.search
                            .selection
                            .rows()
                            .filter_map(|row| results.items.get(row).copied())
                            .map(|item| (source.id, source.generation, item))
                            .collect()
                    })
                    .unwrap_or_default()
            };
            if selected.is_empty() {
                selected.extend(self.selection_target());
            }
            selected
                .iter()
                .filter_map(|(id, generation, item)| {
                    let Some(entry) = self.scan.get(*id) else {
                        writable = false;
                        return None;
                    };
                    if entry.generation != *generation || entry.stale {
                        writable = false;
                        return None;
                    }
                    let Some(completed) = entry.scan.completed() else {
                        writable = false;
                        return None;
                    };
                    Some(crate::search::item_path(&completed.result, *item))
                })
                .collect()
        };
        let current = if self.locations_view.global_mode {
            None
        } else {
            self.source()
                .zip(self.explorer.as_ref())
                .map(|(s, e)| s.scan.folders[e.folder()].path.clone())
        };
        if let Some(entry) = self.scan.selected_id().and_then(|id| self.scan.get(id)) {
            writable &= !entry.stale && entry.scan.completed().is_some();
        } else {
            writable = false;
        }
        crate::file_ui::Context {
            paths,
            current,
            protected: self.scan.entries.iter().map(|e| e.path.clone()).collect(),
            writable,
        }
    }
    fn begin_file_action(&mut self, kind: crate::file_ui::Kind, paths: Option<Vec<PathBuf>>) {
        let explicit = paths.is_some();
        let paths = paths.or_else(|| {
            (kind != crate::file_ui::Kind::Folder)
                .then(|| self.live_selection_paths())
                .flatten()
        });
        let mut context = self.file_context(paths);
        if kind == crate::file_ui::Kind::Folder && explicit {
            context.current = context.paths.first().cloned();
        }
        self.input_error = self
            .file_ui
            .begin(kind, context)
            .err()
            .map(|e| e.to_string());
    }

    fn prepare_folder_drop(
        &mut self,
        paths: Vec<PathBuf>,
        destination: PathBuf,
    ) -> std::io::Result<()> {
        let mut context = self.file_context(Some(paths));
        for entry in &self.scan.entries {
            if crate::file_ops::contains_path(&entry.path, &destination)? {
                context.writable &= !entry.stale && entry.scan.completed().is_some();
            }
        }
        self.file_ui.folder_drop(destination, context)
    }
    fn file_tick(&mut self) {
        if self.file_job.poll() {
            if let Some(report) = &self.file_job.report {
                if matches!(report.action, Some(crate::file_ops::Action::Recycle)) {
                    for item in report.items.iter().filter(|item| item.succeeded()) {
                        if let Some(path) = &item.source {
                            self.quick.remove_recycled(path);
                            self.inline_search.remove_recycled(path);
                        }
                    }
                }
            }
            let affected = self
                .file_job
                .report
                .as_ref()
                .map(|r| r.affected.clone())
                .unwrap_or_default();
            let tracked = self.tracking.location.filter(|_| self.tracking.running());
            if let Err(error) = self.file_refresh.begin(&self.scan, &affected, tracked) {
                self.input_error = Some(error.to_string());
            }
        }
        let previous_epoch = self.scan.epoch;
        let tracking = self
            .tracking
            .location
            .map(|id| (id, self.tracking.confirmed_after, self.tracking.running()));
        for update in self.file_refresh.poll(&mut self.scan, tracking) {
            if self.scan.selected_id() == Some(update.id) {
                self.replace_live_result(update.old.as_deref(), &update.new, None);
            }
        }
        if self.scan.epoch != previous_epoch {
            self.locations_view.preserve_live_selection(&self.scan);
        }
    }
    fn replace_live_result(
        &mut self,
        old: Option<&ScanResult>,
        new: &Arc<ScanResult>,
        entries: Option<(
            &crate::usn::entries::Projection,
            &crate::usn::entries::Projection,
        )>,
    ) {
        self.explorer = Some(
            self.search
                .replace_result(self.explorer.as_ref(), old, new, entries),
        );
        self.detail_target = None;
        self.detail_context = None;
    }

    fn reuse_local_panels(&mut self, source: &Snapshot) {
        if !self.locations_view.global_mode {
            self.statistics.reuse_local(source);
            self.map.reuse_local(source);
        }
    }

    fn sync_tracking(&mut self) {
        let Some(id) = self.tracking.location else {
            return;
        };
        if self.scan.get(id).is_none() || self.scan.is_running() {
            self.tracking.stop();
            return;
        }
        let was_running = self.tracking.running();
        let previous_epoch = self.scan.epoch;
        if let Some(update) = self.tracking.poll() {
            let accepted = match update {
                crate::tracking::Update::Record(record) => {
                    let old = self
                        .scan
                        .get(id)
                        .and_then(|e| e.scan.completed())
                        .map(|c| (Arc::clone(&c.result), c.entries.clone()));
                    self.scan.accept_published(id, &record).map(|changed| {
                        if changed && self.scan.selected_id() == Some(id) {
                            let identities = old
                                .as_ref()
                                .and_then(|(_, entries)| entries.as_deref())
                                .zip(record.entries.as_deref());
                            self.replace_live_result(
                                old.as_ref().map(|(scan, _)| scan.as_ref()),
                                &record.scan,
                                identities,
                            );
                            if let Some(source) = self.source() {
                                self.reuse_local_panels(&source);
                            }
                        }
                    })
                }
                crate::tracking::Update::Cursor(cursor) => self.scan.accept_cursor(id, cursor),
            };
            if let Err(error) = accepted {
                self.tracking.stop();
                self.input_error = Some(error.to_string());
                self.scan.tracking_failed(id);
            }
        }
        if was_running && matches!(self.tracking.status, crate::tracking::Status::Failed(_)) {
            self.scan.tracking_failed(id);
        }
        if self.scan.epoch != previous_epoch {
            self.locations_view.preserve_live_selection(&self.scan);
        }
    }
    #[allow(dead_code, reason = "개발용 NTFS 도우미 GUI 대조의 추적 진입점")]
    fn start_tracking(&mut self, fresh: bool) {
        if self.file_blocked() {
            self.input_error = Some("파일 작업과 목록 갱신을 마친 뒤 시작하세요".into());
            return;
        }
        let action = (|| {
            let path = self
                .target
                .path()
                .ok_or_else(|| std::io::Error::other("추적할 폴더를 선택하세요"))?
                .to_owned();
            let mut existing = None;
            for entry in &self.scan.entries {
                if crate::locations::contains_path(&entry.path, &path)?
                    && crate::locations::contains_path(&path, &entry.path)?
                {
                    existing = Some(entry.id);
                    break;
                }
            }
            let id = match existing {
                Some(id) => id,
                None => self.scan.add(&path)?,
            };
            let resume = self
                .scan
                .get(id)
                .and_then(|e| e.scan.completed())
                .filter(|c| !fresh && c.tracking.is_some() && c.result.detailed)
                .map(|c| Arc::new(c.saved_result()));
            let detailed = resume.as_ref().is_none_or(|r| r.scan.detailed);
            self.tracking.start(id, path, detailed, resume)
        })();
        self.input_error = action.err().map(|e| e.to_string());
    }
    pub fn new() -> Self {
        Self::default()
    }
    fn apply_ui_settings(&mut self, settings: crate::app_settings::UiSettings) {
        self.folder_refresh_disabled = !settings.folder_auto_refresh;
        self.search.widths.restore(settings.columns);
        self.search.input.sort = settings.sort;
        self.search.input.descending = settings.descending;
        self.map_height = settings.map_height;
        self.map_collapsed = settings.map_collapsed;
        let parallelism = match settings.workers {
            1 => crate::scanner::Parallelism::Sequential,
            2 => crate::scanner::Parallelism::Two,
            4 => crate::scanner::Parallelism::Four,
            8 => crate::scanner::Parallelism::Eight,
            _ => crate::scanner::Parallelism::Auto,
        };
        if let Err(e) = self.scan.configure_parallelism(parallelism) {
            self.settings_error = Some(e.to_string());
        }
        self.window_state = settings.window.clone();
        if let Some(window) = &settings.window {
            match crate::app_settings::screens().and_then(|s| window.fit(&s)) {
                Ok(w) => {
                    self.window_state = Some(w.clone());
                    self.window_restore = Some((w, 0));
                }
                Err(e) => self.settings_error = Some(format!("창 위치를 복원하지 못했습니다: {e}")),
            }
        }
        self.settings_saved = Some(settings);
    }
    pub fn ui_preferences(&self) -> crate::app_settings::UiSettings {
        use crate::scanner::Parallelism;
        crate::app_settings::UiSettings {
            window: self.window_state.clone(),
            columns: self.search.widths.saved(),
            sort: self.search.input.sort,
            descending: self.search.input.descending,
            map_height: self.map_height.clamp(64.0, 2000.0),
            map_collapsed: self.search.preferred_map_collapsed(self.map_collapsed),
            workers: match self.scan.parallelism() {
                Parallelism::Auto => 0,
                Parallelism::Sequential => 1,
                Parallelism::Two => 2,
                Parallelism::Four => 4,
                _ => 8,
            },
            folder_auto_refresh: !self.folder_refresh_disabled,
        }
    }
    fn save_ui_settings(&mut self) {
        let Some(path) = &self.settings_path else {
            return;
        };
        if self.window_restore.is_some() {
            return;
        }
        let settings = self.ui_preferences();
        if self.settings_saved.as_ref() == Some(&settings)
            || self.settings_attempt.as_ref() == Some(&settings)
        {
            return;
        }
        match crate::app_settings::save_ui(path, settings.clone()) {
            Ok(()) => {
                self.settings_saved = Some(settings.clone());
                self.settings_error = None;
            }
            Err(e) => self.settings_error = Some(format!("화면 설정을 저장하지 못했습니다: {e}")),
        }
        self.settings_attempt = Some(settings);
        self.settings_pending = None;
    }
    fn settings_frame(&mut self, ctx: &egui::Context) {
        if self.settings_path.is_none() {
            return;
        }
        if let Some((window, step)) = self.window_restore.take() {
            match step {
                0 => ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(
                    egui::pos2(window.position[0], window.position[1]) / ctx.pixels_per_point(),
                )),
                1 => ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(egui::vec2(
                    window.size[0],
                    window.size[1],
                ))),
                _ => ctx.send_viewport_cmd(egui::ViewportCommand::Maximized(window.maximized)),
            }
            if step < 2 {
                self.window_restore = Some((window, step + 1));
            }
            ctx.request_repaint();
            return;
        }
        let scale = ctx.pixels_per_point();
        ctx.input(|i| {
            let viewport = i.viewport();
            if viewport.minimized.unwrap_or(false) {
                return;
            }
            let maximized = viewport.maximized.unwrap_or(false);
            if maximized {
                if let Some(window) = &mut self.window_state {
                    window.maximized = true;
                }
            } else if let (Some(outer), Some(inner)) = (viewport.outer_rect, viewport.inner_rect) {
                self.window_state = Some(crate::app_settings::WindowState {
                    position: [outer.min.x * scale, outer.min.y * scale],
                    size: [inner.width(), inner.height()],
                    maximized: false,
                });
            }
        });
        let settings = self.ui_preferences();
        if self.settings_saved.as_ref() == Some(&settings)
            || self.settings_attempt.as_ref() == Some(&settings)
        {
            self.settings_pending = None;
            return;
        }
        if self
            .settings_pending
            .as_ref()
            .is_none_or(|(p, _)| p != &settings)
        {
            self.settings_pending = Some((settings, std::time::Instant::now()));
        }
        if self
            .settings_pending
            .as_ref()
            .is_some_and(|(_, t)| t.elapsed() > Duration::from_millis(750))
        {
            self.save_ui_settings();
        } else {
            ctx.request_repaint_after(Duration::from_millis(750));
        }
    }
    fn document_tick(&mut self) {
        if self.results.poll() {
            match &self.results.job.view {
                crate::documents::View::Saved(record) => {
                    let scan = Locations::from_saved(record);
                    let explorer = Some(Explorer::new(&record.scan));
                    self.saved_viewer = Some(Box::new(Self {
                        scan,
                        explorer,
                        archive_record: Some(Arc::clone(record)),
                        ..Default::default()
                    }));
                }
                crate::documents::View::Comparison(_) => self.saved_viewer = None,
                crate::documents::View::Live => {}
            }
        }
    }
    pub fn tick_background(&mut self, ctx: &egui::Context) {
        if ctx.input(|i| i.viewport().minimized.unwrap_or(false)) || self.desktop.life.hidden {
            self.visible_watches.clear();
        }
        self.settings_frame(ctx);
        if self.archive_record.is_none() {
            if self.scan.is_running() {
                self.tracking.stop();
                self.tracking.location = None;
            }
            self.sync_scan();
            self.finish_quick_folder_scan();
            if let Some(model) = self.scan.preview() {
                if self.preview_location != self.scan.selected_id() {
                    self.preview_view = Default::default();
                    self.preview_location = self.scan.selected_id();
                }
                self.preview_view.sync(model);
            }
            self.sync_tracking();
            self.file_tick();
            self.duplicates.poll(|id, generation| {
                self.scan.get(id).is_some_and(|location| {
                    !location.stale
                        && location.generation == generation
                        && location.scan.completed().is_some()
                })
            });
            self.close_windows(ctx);
            if self.scan.is_running()
                || self.duplicates.running()
                || self.tracking.running()
                || self.file_job.working()
                || self.file_refresh.busy()
            {
                ctx.request_repaint_after(Duration::from_millis(30));
            }
        }
        self.file_open.poll();
        if self.quick_ui.opening {
            match self.file_open.status() {
                OpenStatus::Failed(error) => {
                    self.quick_ui.error = Some(error.clone());
                    self.quick.open();
                    self.quick_ui.opening = false;
                }
                OpenStatus::Submitted => {
                    self.quick.hide();
                    self.quick_ui.opening = false;
                }
                _ => ctx.request_repaint_after(Duration::from_millis(30)),
            }
        }
        self.document_tick();
    }
    fn show_main(&mut self, ctx: &egui::Context) {
        self.desktop.life.show();
        ctx.send_viewport_cmd_to(
            egui::ViewportId::ROOT,
            egui::ViewportCommand::Minimized(false),
        );
        ctx.send_viewport_cmd_to(egui::ViewportId::ROOT, egui::ViewportCommand::Visible(true));
        ctx.send_viewport_cmd_to(egui::ViewportId::ROOT, egui::ViewportCommand::Focus);
    }
    fn quick_logic(&mut self, ctx: &egui::Context) {
        for event in self.desktop.poll(ctx) {
            match event {
                crate::quick_native::Event::Toggle => self.quick.toggle(self.quick_ui.focused),
                crate::quick_native::Event::Show => self.show_main(ctx),
                crate::quick_native::Event::Settings => self.open_settings(ctx),
                crate::quick_native::Event::Exit => {
                    self.desktop.life.exit = true;
                    self.file_close_requested = true;
                    self.show_main(ctx);
                }
                crate::quick_native::Event::Applied { error, .. } => {
                    self.desktop_settings_error = error;
                    if self.settings_draft.is_some() && self.desktop_settings_error.is_none() {
                        self.settings_draft = Some(settings::Draft::new(self.desktop.applied));
                    }
                    if self.desktop.life.hidden
                        && (!self.desktop.applied.stay || !self.desktop.tray_ready)
                    {
                        self.show_main(ctx);
                    }
                }
                crate::quick_native::Event::Tray { error, .. } => {
                    if let Some(error) = error {
                        self.quick_ui.error = Some(error);
                    }
                    if !self.desktop.tray_ready {
                        self.show_main(ctx);
                    }
                }
                crate::quick_native::Event::Notice(error) => {
                    self.desktop_settings_error = Some(error.clone());
                    self.input_error = Some(error.clone());
                    self.quick_ui.error = Some(error);
                }
                crate::quick_native::Event::Failed(error) => {
                    self.desktop_settings_error = Some(error.clone());
                    self.input_error = Some(error.clone());
                    self.quick_ui.error = Some(error);
                    self.show_main(ctx);
                }
            }
        }
        let now = std::time::Instant::now();
        self.quick.tick(&self.scan, now);
        self.inline_search.tick(&self.scan, now);
        if self.quick.live.running() || self.inline_search.live.running() {
            ctx.request_repaint_after(Duration::from_millis(50));
        }
        if self.quick.visible {
            if self.quick.focus {
                ctx.send_viewport_cmd(egui::ViewportCommand::RequestPaintWhileHidden);
            }
            if self.quick.pending() || self.quick_ui.opening || self.desktop.busy {
                ctx.request_repaint_after(Duration::from_millis(50));
            }
        }
        let busy = self.scan.is_running()
            || self.duplicates.running()
            || self.tracking.running()
            || self.file_job.working()
            || self.file_refresh.busy()
            || matches!(self.file_open.status(), OpenStatus::Running)
            || self.results.busy()
            || self.inline_search.live.running()
            || self.file_close_requested;
        if let Err(e) = self.desktop.heartbeat(self.desktop.life.hidden && busy) {
            self.quick_ui.error = Some(e.to_string());
            self.show_main(ctx);
        }
    }
    fn quick_window(&mut self, ctx: &egui::Context) {
        // eframe 0.37의 숨긴 루트 렌더링에서 새 즉시 창 생성 시 패닉 방지를 위해 상주 중 창 유지
        let action = self.quick_ui.show(
            ctx,
            &mut self.quick,
            &self.scan,
            self.desktop.applied.enabled || (self.desktop.applied.stay && self.desktop.tray_ready),
        );

        if let Some(action) = action {
            self.apply_search_action(ctx, SearchSurface::Quick, action);
        }
    }

    fn search_surface(
        &mut self,
        surface: SearchSurface,
    ) -> (
        &mut crate::quick_search::QuickSearch,
        &mut crate::quick_ui::QuickUi,
    ) {
        match surface {
            SearchSurface::Quick => (&mut self.quick, &mut self.quick_ui),
            SearchSurface::Inline => (&mut self.inline_search, &mut self.inline_search_ui),
        }
    }

    fn apply_search_action(
        &mut self,
        ctx: &egui::Context,
        surface: SearchSurface,
        action: crate::quick_ui::Action,
    ) {
        use crate::quick_ui::Action;
        let result = (|| -> std::io::Result<()> {
            match action {
                Action::ShowMain => {
                    self.show_main(ctx);
                    self.quick.hide();
                }
                Action::Settings => self.open_settings(ctx),
                Action::Analyze(target) => self.open_quick_analysis(ctx, surface, target)?,
                Action::AnalyzePath(hit) => self.analyze_quick_path(ctx, surface, hit)?,
                Action::ScanFolder(hit) => self.scan_quick_folder(ctx, surface, hit)?,
                Action::Open(request) => {
                    self.file_open.start(request)?;
                    if surface == SearchSurface::Quick {
                        self.quick_ui.opening = true;
                    }
                }
                Action::Copy(path) => crate::path_copy::copy_to_clipboard(ctx, &[path])?,
                Action::Recycle(hit) => {
                    let context = self.quick_recycle_context(hit.path)?;
                    let identity = hit.identity.ok_or_else(|| {
                        std::io::Error::other("검색 결과의 파일 또는 폴더 ID를 확인할 수 없습니다")
                    })?;
                    self.file_ui.begin_search_recycle(
                        context,
                        identity,
                        hit.kind == crate::file_open::TargetKind::Folder,
                    )?;
                    self.show_main(ctx);
                    if surface == SearchSurface::Quick {
                        self.quick.hide();
                    }
                }
            }
            Ok(())
        })();
        if let Err(error) = result {
            self.search_surface(surface).1.error = Some(error.to_string());
        }
    }

    fn check_analysis_documents(&self) -> std::io::Result<()> {
        if self.results.busy()
            || self
                .saved_viewer
                .as_ref()
                .is_some_and(|viewer| viewer.results.busy())
        {
            return Err(std::io::Error::other(
                "문서 작업이 끝난 뒤 분석을 열어 주세요",
            ));
        }
        Ok(())
    }

    fn select_quick_analysis(&mut self, (id, generation, item): Target) -> std::io::Result<()> {
        self.check_analysis_documents()?;
        self.try_analyze_at(id, generation, item)?;
        if let Some(entry) = self.scan.get(id) {
            self.target.choose_registered(entry.path.clone());
        }
        self.results.job.back();
        self.saved_viewer = None;
        self.input_error = None;
        Ok(())
    }

    fn open_quick_analysis(
        &mut self,
        ctx: &egui::Context,
        surface: SearchSurface,
        target: Target,
    ) -> std::io::Result<()> {
        self.select_quick_analysis(target)?;
        self.leave_search_for_analysis(ctx, surface);
        Ok(())
    }

    fn leave_search_for_analysis(&mut self, ctx: &egui::Context, surface: SearchSurface) {
        self.show_main(ctx);
        let (search, ui) = self.search_surface(surface);
        ui.error = None;
        search.leave_for_analysis();
    }
    fn analyze_quick_path(
        &mut self,
        ctx: &egui::Context,
        surface: SearchSurface,
        hit: crate::quick_search::Hit,
    ) -> std::io::Result<()> {
        let verified = hit.verify_for_analysis()?;
        let folder = std::fs::canonicalize(hit.analysis_folder().expect("확인한 분석 폴더"))?;
        if let Some(entry) = self.scan.overlapping(&folder)? {
            let relative = entry.relative_folder(&folder)?;
            if let (Some(relative), Some(completed)) =
                (relative, entry.scan.completed().filter(|_| !entry.stale))
            {
                if let Some(item) =
                    hit.analysis_item_at(&completed.result, relative, verified.created().ok())
                {
                    return self.open_quick_analysis(
                        ctx,
                        surface,
                        (entry.id, entry.generation, item),
                    );
                }
            }
            if !relative.is_some_and(|path| path.as_os_str().is_empty())
                || entry.scan.completed().is_some()
            {
                return Err(std::io::Error::other(
                    "기존 분석 위치와 범위가 겹칩니다. 기존 위치를 다시 스캔하거나 위치 관리를 확인하세요",
                ));
            }
        }
        let (_, ui) = self.search_surface(surface);
        ui.error = None;
        ui.confirm_folder_scan(hit);
        ctx.request_repaint();
        Ok(())
    }
    fn scan_quick_folder(
        &mut self,
        ctx: &egui::Context,
        surface: SearchSurface,
        hit: crate::quick_search::Hit,
    ) -> std::io::Result<()> {
        if self.scan.is_running() || self.file_blocked() || self.tracking.running() {
            return Err(std::io::Error::other(
                "스캔·파일 작업·실시간 갱신이 끝난 뒤 분석을 시작하세요",
            ));
        }
        self.check_analysis_documents()?;
        hit.verify_for_analysis()?;
        let path = hit
            .analysis_folder()
            .expect("확인한 분석 폴더")
            .to_path_buf();
        let canonical = std::fs::canonicalize(&path)?;
        let path = match self.scan.overlapping(&canonical)? {
            Some(entry)
                if entry
                    .relative_folder(&canonical)?
                    .is_some_and(|path| path.as_os_str().is_empty()) =>
            {
                entry.path.clone()
            }
            _ => path,
        };
        let location = self.scan.start_background(path).map_err(|error| {
            if error.kind() == std::io::ErrorKind::AlreadyExists {
                std::io::Error::other("기존 분석 위치와 범위가 겹칩니다. 기존 위치를 다시 스캔하거나 위치 관리를 확인하세요")
            } else {
                error
            }
        })?;
        let generation = self
            .scan
            .get(location)
            .expect("시작한 분석 위치")
            .generation;
        self.quick_folder_scan = Some(QuickFolderScan {
            surface,
            location,
            generation,
            hit,
        });
        self.input_error = None;
        self.leave_search_for_analysis(ctx, surface);
        Ok(())
    }
    fn finish_quick_folder_scan(&mut self) {
        if self.scan.is_running() {
            return;
        }
        let Some(pending) = self.quick_folder_scan.take() else {
            return;
        };
        let result = (|| -> std::io::Result<Target> {
            let entry = self
                .scan
                .get(pending.location)
                .ok_or_else(|| std::io::Error::other("분석 위치가 제거되었습니다"))?;
            match entry.scan.status() {
                ScanStatus::Cancelled => {
                    return Err(std::io::Error::other("폴더 분석을 취소했습니다"))
                }
                ScanStatus::Failed(error) => {
                    return Err(std::io::Error::other(format!("폴더 분석 실패: {error}")))
                }
                _ => {}
            }
            let completed = entry
                .scan
                .completed()
                .filter(|_| !entry.stale && entry.generation != pending.generation)
                .ok_or_else(|| std::io::Error::other("폴더 분석이 완료되지 않았습니다"))?;
            let verified = pending.hit.verify_for_analysis()?;
            let folder =
                std::fs::canonicalize(pending.hit.analysis_folder().expect("확인한 분석 폴더"))?;
            let relative = entry.relative_folder(&folder)?.ok_or_else(|| {
                std::io::Error::other("검색한 폴더가 새 분석 범위를 벗어났습니다")
            })?;
            let item = pending
                .hit
                .analysis_item_at(&completed.result, relative, verified.created().ok())
                .ok_or_else(|| std::io::Error::other("검색한 항목이 새 분석 결과에 없습니다"))?;
            Ok((entry.id, entry.generation, item))
        })();
        if let Err(error) = result.and_then(|target| self.select_quick_analysis(target)) {
            if pending.surface == SearchSurface::Inline {
                self.inline_search.open();
                self.inline_search_ui.error = Some(error.to_string());
            }
            self.input_error = Some(error.to_string());
        }
    }
    fn close_windows(&mut self, ctx: &egui::Context) {
        let requested = ctx.input(|i| i.viewport().close_requested()) || self.file_close_requested;
        if !requested {
            return;
        }
        self.visible_watches.clear();
        self.save_ui_settings();
        if !self.file_close_requested
            && self
                .desktop
                .life
                .close(self.desktop.applied.stay, self.desktop.tray_ready)
        {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
            self.quick.hide();
            return;
        }
        self.file_close_requested = true;
        self.inline_search_ui.close_inline(&mut self.inline_search);
        self.file_job.cancel();
        self.duplicates.cancel();
        self.file_ui.dialog = None;
        self.file_refresh.cancel(&mut self.scan);
        self.scan.cancel();
        self.tracking.stop();
        if self.results.busy() {
            self.results.job.cancel();
        }
        if self.file_job.working()
            || self.duplicates.running()
            || self.scan.is_running()
            || self.file_refresh.busy()
            || self.results.busy()
            || self.inline_search.live.running()
        {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            ctx.request_repaint_after(Duration::from_millis(30));
        } else {
            self.file_close_requested = false;
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }
    fn selection_target(&self) -> Option<Target> {
        if let Some(item) = self.live_selected_item() {
            let source = self.source()?;
            return item.map(|item| (source.id, source.generation, item));
        }
        self.detail_target.or_else(|| {
            if self.locations_view.global_mode {
                self.locations_view.selected_target()
            } else {
                if self.search.selection.count() > 1 {
                    return None;
                }
                let source = self.source()?;
                let item = self
                    .search
                    .job
                    .results()
                    .and_then(|r| self.search.selection.single(&r.items))
                    .unwrap_or_else(|| {
                        Item::Folder(self.explorer.as_ref().map_or(0, Explorer::folder))
                    });
                Some((source.id, source.generation, item))
            }
        })
    }
    fn source(&self) -> Option<Snapshot> {
        let locations = self.active_locations();
        locations.completed_snapshot(locations.selected_id()?)
    }
    fn export_context(&self) -> crate::results_ui::ExportContext {
        let sources = self.scan.snapshots();
        let selected = sources
            .iter()
            .position(|s| Some(s.id) == self.scan.selected_id());
        let matches = if !self.search.input.is_active() {
            None
        } else if self.locations_view.global_mode {
            self.locations_view
                .results()
                .map(|r| (r.sources.clone(), r.hits.clone()))
        } else {
            self.source().zip(self.search.job.results()).map(|(s, r)| {
                (
                    vec![s],
                    r.items
                        .iter()
                        .map(|&item| Hit { source: 0, item })
                        .collect(),
                )
            })
        };
        crate::results_ui::ExportContext {
            sources,
            selected: selected.filter(|_| !self.locations_view.global_mode),
            folder: self
                .explorer
                .as_ref()
                .map(Explorer::folder)
                .filter(|_| !self.locations_view.global_mode),
            matches,
        }
    }
    fn start_open(&mut self, request: OpenRequest) {
        self.input_error = self.file_open.start(request).err().map(|e| e.to_string());
    }
    fn sync_scan(&mut self) {
        let replacing = self.scan.replacing();
        let selected = self.scan.selected_id();
        let previous = if self.folder_tabs.len() > 1 || self.folder_tabs.is_split() {
            self.source()
        } else {
            None
        };
        if self.scan.poll() {
            if replacing && self.scan.selected_id() != selected {
                self.locations_view
                    .choose_scope(&mut self.search, SearchScope::Folder);
                self.tab = 0;
            }
            if self.scan.selected_id().is_some() {
                self.unavailable_pane = None;
            }
            if let (Some(old), Some(new)) = (previous, self.source()) {
                if old.id == new.id && !Arc::ptr_eq(&old.scan, &new.scan) {
                    self.scan.take_finished_preview();
                    self.replace_live_result(Some(&old.scan), &new.scan, None);
                    self.locations_view.preserve_live_selection(&self.scan);
                    return;
                }
            }
            if self.locations_view.global_mode {
                self.search.invalidate();
            } else {
                self.search.reset();
            }
            let preview = self.scan.take_finished_preview();
            let handoff = !self.locations_view.global_mode
                && self.preview_location == self.scan.selected_id()
                && !self.preview_view.previous
                && preview.is_some();
            self.explorer = self.scan.completed().map(|c| {
                if handoff {
                    if let Some(model) = &preview {
                        return self.preview_view.complete(model, &c.result);
                    }
                }
                Explorer::new(&c.result)
            });
            self.detail_target = None;
            if handoff {
                if let (Some(source), Some(view)) = (self.source(), self.explorer.as_ref()) {
                    self.search.refresh(&source.scan, view.folder());
                    self.search.focus_item(view.selected());
                    self.detail_target = Some((source.id, source.generation, view.selected()));
                }
            }
        }
    }
    fn begin_scan(&mut self) {
        if self.file_blocked() {
            self.input_error = Some("파일 작업과 목록 갱신을 마친 뒤 스캔하세요".into());
            return;
        }
        let Some(path) = self.target.path().map(PathBuf::from) else {
            self.input_error = Some("분석할 폴더를 선택하세요".into());
            return;
        };
        self.input_error = match self.scan.start(path.clone()) {
            Ok(()) => {
                self.tracking.stop();
                None
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                match self.scan.plan_root_replacement(&path) {
                    Ok(Some(plan)) => {
                        self.root_replacement = Some(plan);
                        None
                    }
                    Ok(None) => Some(error.to_string()),
                    Err(error) => Some(error.to_string()),
                }
            }
            Err(error) => Some(error.to_string()),
        };
    }

    fn confirm_root_replacement(&mut self, ctx: &egui::Context) {
        let Some(plan) = &self.root_replacement else {
            return;
        };
        let dialog = egui::Modal::new(egui::Id::unique("replace_scan_root")).show(ctx, |ui| {
            ui.set_width(360.0);
            ui.heading("새 최상위 폴더 스캔");
            ui.add(egui::Label::new(plan.path.display().to_string()).wrap());
            ui.add_space(8.0);
            ui.label(format!(
                "완료되면 겹치는 기존 분석 {}곳을 교체합니다.",
                plan.overlaps.len()
            ));
            for path in plan
                .overlaps
                .iter()
                .take(3)
                .filter_map(|id| self.scan.get(*id))
            {
                ui.add(egui::Label::new(path.path.display().to_string()).truncate());
            }
            ui.small("취소하거나 실패하면 기존 분석을 유지합니다.");
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                (
                    ui.button("취소").clicked(),
                    ui.button("새 최상위로 스캔").clicked(),
                )
            })
            .inner
        });
        let (cancel, start) = dialog.inner;
        if cancel || dialog.should_close() {
            self.root_replacement = None;
        } else if start {
            let plan = self
                .root_replacement
                .take()
                .expect("사용자가 확인한 새 최상위 폴더");
            if self.file_blocked() {
                self.input_error = Some("파일 작업과 목록 갱신을 마친 뒤 스캔하세요".into());
            } else {
                self.input_error = self
                    .scan
                    .start_root_replacement(plan)
                    .err()
                    .map(|error| error.to_string());
                if self.input_error.is_none() {
                    self.tracking.stop();
                }
            }
        }
    }
    fn accept_drop(&mut self, paths: Vec<PathBuf>) {
        if self.inline_search.visible
            && self.inline_search.scope() == crate::quick_search::Scope::Folder
        {
            self.inline_search_ui.error = self
                .inline_search
                .drop_folder(&paths)
                .err()
                .map(|error| error.to_string());
            return;
        }
        self.input_error = if self.scan.is_running() {
            Some("스캔이 끝난 뒤 새 폴더를 선택하세요".into())
        } else {
            self.target.accept_drop(&paths).err()
        };
    }
    pub fn analyze_at(&mut self, id: u64, generation: u64, item: Item) {
        if let Err(error) = self.try_analyze_at(id, generation, item) {
            self.input_error = Some(error.to_string());
        }
    }

    fn try_analyze_at(&mut self, id: u64, generation: u64, item: Item) -> std::io::Result<()> {
        self.sync_scan();
        let source = self
            .scan
            .get(id)
            .filter(|entry| !entry.stale && entry.generation == generation)
            .and_then(|_| self.scan.completed_snapshot(id))
            .ok_or_else(|| std::io::Error::other("결과가 변경되었습니다. 다시 스캔해 주세요."))?;
        let previous = self.capture_navigation();
        self.scan.show_completed_view(id);
        self.unavailable_pane = None;
        let explorer = source.analyze(item);
        self.locations_view
            .choose_scope(&mut self.search, SearchScope::Folder);
        self.search.reset();
        self.search.refresh(&source.scan, explorer.folder());
        self.search.focus_item(item);
        self.explorer = Some(explorer);
        self.locations_view.global_mode = false;
        self.tab = 0;
        self.detail_target = Some((id, generation, item));
        self.remember_navigation(previous);
        Ok(())
    }
    pub fn navigate(&mut self, folder: usize, clear_search: bool) {
        let previous = self.capture_navigation();
        if let (Some(source), Some(explorer)) = (self.source(), &mut self.explorer) {
            if source.scan.folders[explorer.folder()].parent == Some(folder) {
                explorer.up(&source.scan);
            } else {
                explorer.open_folder(&source.scan, folder);
            }
            if clear_search {
                self.search.clear_conditions();
            }
            self.search.whole_scan = false;
            self.search.vertical_offset = 0.0;
            self.search.refresh(&source.scan, folder);
            self.detail_target = Some((source.id, source.generation, Item::Folder(folder)));
        }
        self.remember_navigation(previous);
    }
    pub fn select_registered_location(&mut self, id: u64) {
        let Some(location) = self.scan.get(id) else {
            return;
        };
        let path = location.path.clone();
        let previous = self.capture_navigation();
        self.locations_view.set_global_mode(false);
        self.scan.choose(id);
        self.target.choose_registered(path);
        self.input_error = None;
        self.sync_scan();
        self.remember_navigation(previous);
    }
    pub fn capture_navigation(&self) -> Option<crate::navigation::ViewState> {
        if self.archive_record.is_some() {
            return None;
        }
        let Some(source) = self.source() else {
            return self.unavailable_pane.clone();
        };
        let folder = self.explorer.as_ref()?.folder();
        Some(crate::navigation::ViewState {
            location: source.id,
            folder: source.scan.folders[folder].path.clone(),
            query: self.search.input.clone(),
            whole_scan: self.search.whole_scan,
            global: self.locations_view.global_mode,
            selected: self
                .live_selection_paths()
                .or_else(|| {
                    if self.locations_view.global_mode {
                        self.locations_view.restoring_paths()
                    } else {
                        self.search.restoring_paths()
                    }
                })
                .unwrap_or_else(|| self.copy_selection_paths()),
            scroll: [self.search.horizontal_offset, self.search.vertical_offset],
            tab: self.tab,
        })
    }

    fn remember_navigation(&mut self, from: Option<crate::navigation::ViewState>) {
        if let (Some(from), Some(to)) = (from, self.capture_navigation()) {
            self.history.visit(from, &to);
        }
    }
    pub fn history_move(&mut self, forward: bool) {
        let Some(current) = self.capture_navigation() else {
            return;
        };
        let next = if forward {
            self.history.forward(current)
        } else {
            self.history.back(current)
        };
        let Some(next) = next else { return };
        let sources = self.scan.snapshots();
        let resolved = match next.resolve(&sources) {
            Ok(r) => r,
            Err(e) => {
                self.input_error = Some(e.to_string());
                return;
            }
        };
        self.restore_folder_view(next, resolved);
    }

    fn restore_folder_view(
        &mut self,
        next: crate::navigation::ViewState,
        resolved: crate::navigation::Resolved,
    ) {
        self.live_rows.restore(next.selected.clone());
        let reuse_tree = self.scan.selected_id() == Some(resolved.source.id);
        self.scan.show_completed_view(resolved.source.id);
        let mut explorer = match self.explorer.take().filter(|_| reuse_tree) {
            Some(mut explorer) => {
                explorer.open_folder(&resolved.source.scan, resolved.folder);
                explorer
            }
            None => {
                Explorer::with_expanded(&resolved.source.scan, resolved.folder, Default::default())
            }
        };
        if let Some(item) = resolved.selected.first() {
            explorer.select(*item);
        }
        self.search.restore(
            &resolved.source.scan,
            resolved.folder,
            next.query.clone(),
            next.whole_scan,
            resolved.selected,
            next.scroll,
        );
        self.locations_view.set_global_mode(next.global);
        if next.global {
            let mut paths = std::collections::HashSet::new();
            for path in &next.selected {
                if let Some(entry) = self.scan.entries.iter().find(|entry| {
                    !entry.stale
                        && entry.scan.completed().is_some()
                        && path.starts_with(&entry.path)
                }) {
                    paths.insert((entry.id, path.clone()));
                }
            }
            self.locations_view.restore_selection(next.query, paths);
        }
        self.explorer = Some(explorer);
        self.search.sync_presentation(&mut self.map_collapsed);
        self.tab = next.tab;
        self.detail_target = None;
        if resolved.fallback {
            self.input_error = Some("이전 폴더가 없어 남아 있는 상위 폴더로 이동했습니다".into());
        }
    }
    fn pick_map(&mut self, row: usize) {
        let selected = if self.locations_view.global_mode {
            let visible = self.locations_view.results().into_iter().flat_map(|r| {
                r.hits.iter().map(|hit| {
                    let s = &r.sources[hit.source];
                    (s.id, s.generation, hit.item)
                })
            });
            self.map.select(row, visible)
        } else {
            let Some(source) = self.source() else {
                return;
            };
            let visible = self.search.job.results().into_iter().flat_map(|r| {
                r.items
                    .iter()
                    .map(|&item| (source.id, source.generation, item))
            });
            self.map.select(row, visible)
        };
        let Some(selected) = selected else {
            return;
        };
        self.detail_target = Some(selected.target);
        self.live_select_hit(selected.target);
        self.statistics.select_target(selected.target);
        if self.locations_view.global_mode {
            self.locations_view.select_target(selected.target);
        } else if let (Some(source), Some(explorer)) = (self.source(), &mut self.explorer) {
            selected.apply_local(&source, explorer, &mut self.search);
        }
    }
    fn navigation_shortcuts(&mut self, ctx: &egui::Context) {
        if self.archive_record.is_some() {
            return;
        }
        let popup = egui::Popup::is_any_open(ctx)
            || self.settings_draft.is_some()
            || self.file_ui.dialog.is_some()
            || self.root_replacement.is_some();
        let editing = ctx.egui_wants_keyboard_input();
        let busy = self.scan.is_running() || self.file_blocked() || self.results.busy();
        let action = ctx.input_mut(|i| {
            let found = i.events.iter().find_map(|event| match event {
                egui::Event::Key {
                    key,
                    pressed: true,
                    modifiers,
                    ..
                } => crate::navigation::shortcut(*key, *modifiers, editing, popup, busy)
                    .map(|a| (*key, *modifiers, a)),
                _ => None,
            });
            found.and_then(|(key, m, a)| i.consume_key(m, key).then_some(a))
        });
        use crate::navigation::Action;
        if self.inline_search.visible && action == Some(Action::CopyPath) {
            let result = self
                .inline_search
                .selected_hit(&self.scan)
                .and_then(|hit| crate::path_copy::copy_to_clipboard(ctx, &[hit.path]));
            self.inline_search_ui.error = result.err().map(|error| error.to_string());
            return;
        }
        if self.inline_search.visible
            && !matches!(action, Some(Action::Search | Action::Path | Action::Rescan))
        {
            return;
        }
        match action {
            Some(Action::Search) => {
                ctx.memory_mut(|m| m.request_focus(egui::Id::unique("search_query")))
            }
            Some(Action::Path) => {
                ctx.memory_mut(|m| m.request_focus(egui::Id::unique("scan_path")))
            }
            Some(Action::Rescan) => {
                if let Some(path) = self
                    .scan
                    .selected_id()
                    .and_then(|id| self.scan.get(id))
                    .map(|s| s.path.clone())
                {
                    self.target.choose_registered(path);
                    self.begin_scan();
                }
            }
            Some(Action::Open) if matches!(self.tab, 0 | 3) => {
                if self.live_keyboard_open() {
                    return;
                }
                let target = if self.locations_view.global_mode {
                    self.locations_view.selected_target()
                } else {
                    self.source().and_then(|s| {
                        self.search
                            .job
                            .results()
                            .and_then(|r| self.search.selection.single(&r.items))
                            .map(|i| (s.id, s.generation, i))
                    })
                };
                if let Some((id, generation, item)) = target {
                    match item {
                        Item::Folder(_) => self.analyze_at(id, generation, item),
                        Item::File(_) => {
                            if let Some(s) = self
                                .scan
                                .snapshots()
                                .into_iter()
                                .find(|s| s.id == id && s.generation == generation)
                            {
                                self.start_open(OpenRequest::for_item(
                                    &s.scan,
                                    item,
                                    OpenAction::OpenFile,
                                ));
                            }
                        }
                    }
                }
            }
            Some(Action::CopyPath) => {
                let paths = self.copy_selection_paths();
                self.input_error = crate::path_copy::copy_to_clipboard(ctx, &paths)
                    .err()
                    .map(|e| e.to_string());
            }
            _ => {}
        }
    }
    fn derived_input(&self, matched: bool) -> Option<(PanelKey, Vec<Snapshot>)> {
        if self.locations_view.global_mode {
            let sources = if matched {
                self.locations_view.results()?.sources.clone()
            } else {
                self.scan.snapshots()
            };
            Some((
                PanelKey {
                    versions: sources.iter().map(|s| (s.id, s.generation)).collect(),
                    folder: None,
                    query: matched.then(|| self.search.input.clone()),
                },
                sources,
            ))
        } else {
            let source = self.source()?;
            let folder = if matched && self.search.whole_scan {
                0
            } else {
                self.explorer.as_ref()?.folder()
            };
            if matched && self.search.job.results().is_none() {
                return None;
            }
            Some((
                PanelKey {
                    versions: vec![(source.id, source.generation)],
                    folder: Some(folder),
                    query: matched.then(|| self.search.input.clone()),
                },
                vec![source],
            ))
        }
    }
    fn hits(&self) -> Vec<Hit> {
        if self.locations_view.global_mode {
            self.locations_view
                .results()
                .map(|r| r.hits.clone())
                .unwrap_or_default()
        } else {
            self.search
                .job
                .results()
                .map(|r| {
                    r.items
                        .iter()
                        .map(|&item| Hit { source: 0, item })
                        .collect()
                })
                .unwrap_or_default()
        }
    }
    fn refresh_derived(&mut self) {
        if self.search.sync_presentation(&mut self.map_collapsed) {
            self.tab = if self.search.input.is_active() { 3 } else { 0 };
        }
        self.search.poll();
        if self.locations_view.global_mode {
            self.locations_view.refresh(&self.scan, &self.search.input);
        } else if let (Some(source), Some(explorer)) = (self.source(), &self.explorer) {
            self.search.refresh(&source.scan, explorer.folder());
        }
        let context = (
            self.scan.epoch,
            self.scan.selected_id(),
            self.locations_view.global_mode,
            self.explorer.as_ref().map_or(0, Explorer::folder),
            self.search.input.clone(),
            self.search.whole_scan,
        );
        if self.detail_context.as_ref() != Some(&context) {
            self.detail_target = None;
            self.detail_context = Some(context);
        }
        let full = self.derived_input(false);
        let matched;
        let stats_input = if self.statistics.matched {
            matched = self.derived_input(true);
            matched.as_ref()
        } else {
            full.as_ref()
        };
        if let Some((key, sources)) = stats_input {
            if self.statistics.needs(key) {
                let scope = if self.statistics.matched {
                    StatsScope::Matches(self.hits())
                } else if let Some(folder) = key.folder {
                    StatsScope::Subtree(folder)
                } else {
                    StatsScope::All
                };
                self.statistics.submit(key.clone(), sources.clone(), scope);
            }
        } else {
            self.statistics.clear();
        }
        if let Some((key, sources)) = full {
            if self.map.needs(&key) {
                let scope = key.folder.map_or(MapScope::Locations, MapScope::Folder);
                self.map.submit(key, sources, scope);
            }
        } else {
            self.map.clear();
        }
        self.statistics.poll();
        self.map.poll();
    }
    fn status(&self) -> String {
        if self.duplicates.running() {
            let progress = self.duplicates.progress();
            return format!(
                "중복 파일 검사 · {} · 확인 {}개 · 읽은 용량 {} · {:.1}초",
                progress.stage,
                progress.files,
                SizeDisplay::new(progress.bytes).text,
                self.duplicates.elapsed()
            );
        }
        if self.file_job.working() {
            return format!(
                "{} · {:.1}초",
                self.file_job.phase.label(),
                self.file_job.elapsed()
            );
        }
        if self.file_refresh.busy() {
            return self.file_refresh.label().into();
        }
        if !self.scan.is_running()
            && self.tracking.location == self.scan.selected_id()
            && (self.tracking.running()
                || self.tracking.checked.is_some()
                || matches!(self.tracking.status, crate::tracking::Status::Failed(_)))
        {
            if let crate::tracking::Status::Failed(reason) = &self.tracking.status {
                return format!("{} · {reason}", self.tracking.label());
            }
            return format!(
                "{} · 마지막 확인 {}",
                self.tracking.label(),
                crate::local_date::display(self.tracking.checked)
            );
        }
        match self.scan.status() {
            ScanStatus::Idle => "대기 중".into(),
            ScanStatus::Running => {
                let counts = self.scan.progress().expect("진행 중 스캔");
                let seconds = self.scan.elapsed().unwrap_or_default().as_secs_f64();
                format!(
                    "파일·폴더 확인 중 · 파일 {} · 폴더 {} · {seconds:.1}초 · 동시 작업 {}개",
                    counts.files_scanned,
                    counts.folders_scanned,
                    self.scan.parallelism().workers()
                )
            }
            ScanStatus::Cancelling => format!(
                "취소 중 · {:.1}초",
                self.scan.elapsed().unwrap_or_default().as_secs_f64()
            ),
            ScanStatus::Completed => self
                .scan
                .completed()
                .and_then(|c| c.engine.as_ref())
                .map_or_else(
                    || "스캔 완료 · 방식 기록 없음".into(),
                    |r| {
                        format!(
                            "스캔 완료 · {}{}",
                            r.actual.label(),
                            if r.fallback.is_some() {
                                " (방식 전환)"
                            } else {
                                ""
                            }
                        )
                    },
                ),
            ScanStatus::Cancelled => "스캔 취소됨 · 이전 결과 유지".into(),
            ScanStatus::Failed(e) => format!("스캔 실패: {e}"),
        }
    }
    fn header(&mut self, ui: &mut egui::Ui, docked_details: bool) {
        if self.logo.is_none() {
            let data = eframe::icon_data::from_png_bytes(include_bytes!(
                "../assets/brand/diskweave-logo.png"
            ))
            .expect("프로젝트 로고");
            self.logo = Some(ui.ctx().load_texture(
                "diskweave_brand",
                egui::ColorImage::from_rgba_unmultiplied(
                    [data.width as usize, data.height as usize],
                    &data.rgba,
                ),
                egui::TextureOptions::LINEAR,
            ));
        }
        ui.horizontal(|ui| {
            let texture = self.logo.as_ref().unwrap();
            ui.add(egui::Image::new((texture.id(), egui::vec2(24.0, 24.0))).uv(
                egui::Rect::from_min_max(
                    egui::pos2(39.0 / 960.0, 37.0 / 240.0),
                    egui::pos2(205.0 / 960.0, 203.0 / 240.0),
                ),
            ));
            ui.add(
                egui::Image::new((texture.id(), egui::vec2(113.0, 25.0))).uv(
                    egui::Rect::from_min_max(
                        egui::pos2(270.0 / 960.0, 92.0 / 240.0),
                        egui::pos2(575.0 / 960.0, 157.0 / 240.0),
                    ),
                ),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.spacing_mut().item_spacing.x = 2.0;
                if crate::ui_style::title_control(ui, "닫기").clicked() {
                    ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                }
                let maximized = ui.input(|i| i.viewport().maximized.unwrap_or(false));
                if crate::ui_style::title_control(ui, if maximized { "복원" } else { "최대화" })
                    .clicked()
                {
                    ui.ctx()
                        .send_viewport_cmd(egui::ViewportCommand::Maximized(!maximized));
                }
                if crate::ui_style::title_control(ui, "최소화").clicked() {
                    ui.ctx()
                        .send_viewport_cmd(egui::ViewportCommand::Minimized(true));
                }
                if self.archive_record.is_some() {
                    ui.small("조회 전용");
                }
                if self.archive_record.is_none() {
                    let label =
                        if self.settings_error.is_some() || self.desktop_settings_error.is_some() {
                            "설정 !"
                        } else {
                            "설정"
                        };
                    if ui.button(label).clicked() {
                        self.open_settings(ui.ctx());
                    }
                }
                if !docked_details && ui.button("상세 정보").clicked() {
                    self.details_open = !self.details_open;
                }
                let response = ui.interact(
                    ui.available_rect_before_wrap(),
                    ui.scope_id().with("title_drag"),
                    egui::Sense::click_and_drag(),
                );
                if response.drag_started() {
                    ui.ctx().send_viewport_cmd(egui::ViewportCommand::StartDrag);
                }
                if response.double_clicked() {
                    ui.ctx()
                        .send_viewport_cmd(egui::ViewportCommand::Maximized(!maximized));
                }
            });
        });
    }

    fn scan_settings(&mut self, ui: &mut egui::Ui) {
        let mut parallelism = self.scan.parallelism();
        ui.add_enabled_ui(
            !self.scan.is_running() && !self.file_blocked() && self.archive_record.is_none(),
            |ui| {
                egui::ComboBox::from_id_salt("scan_parallelism")
                    .selected_text(format!("스캔 동시 작업: {}", parallelism.label()))
                    .show_ui(ui, |ui| {
                        for choice in [
                            crate::scanner::Parallelism::Auto,
                            crate::scanner::Parallelism::Sequential,
                            crate::scanner::Parallelism::Two,
                            crate::scanner::Parallelism::Four,
                            crate::scanner::Parallelism::Eight,
                        ] {
                            ui.selectable_value(&mut parallelism, choice, choice.label());
                        }
                    });
            },
        );
        if parallelism != self.scan.parallelism() {
            self.input_error = self
                .scan
                .configure_parallelism(parallelism)
                .err()
                .map(|e| e.to_string());
        }
        ui.small("자동은 최대 8개 작업을 사용합니다.");
    }
    fn toolbar(&mut self, ui: &mut egui::Ui) {
        let editable = !self.scan.is_running() && !self.file_blocked();
        let quick_scan_path = self
            .quick_folder_scan
            .as_ref()
            .and_then(|_| self.scan.active_path())
            .map(|path| path.to_string_lossy().into_owned());
        ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
            let [drive_width, browse_width, scan_width] = [68.0, 84.0, 114.0];
            let height = crate::ui_style::FIELD_HEIGHT;
            ui.spacing_mut().interact_size.y = height;
            ui.spacing_mut().button_padding = egui::vec2(10.0, 6.0);
            ui.label("스캔 대상");
            crate::ui_style::slot(
                ui,
                egui::vec2(drive_width, height),
                egui::Layout::left_to_right(egui::Align::Center),
                |ui| {
                    let label = quick_scan_path
                        .as_deref()
                        .unwrap_or_else(|| self.target.text())
                        .get(..2)
                        .filter(|drive| {
                            drive.as_bytes()[0].is_ascii_alphabetic() && drive.ends_with(':')
                        })
                        .unwrap_or("위치");
                    let drives = crate::ui_style::scope_button(ui, label, drive_width);
                    egui::Popup::menu(&drives).show(|ui| {
                        if let Some(path) = self.locations_view.drive_control(ui, !editable) {
                            self.input_error = self.target.choose_folder(path).err();
                        }
                        ui.separator();
                        ui.menu_button("분석 위치 관리", |ui| {
                            ui.set_max_width(420.0);
                            ui.add_enabled_ui(editable, |ui| {
                                let action = self.locations_view.controls(
                                    ui,
                                    &mut self.scan,
                                    self.target.path(),
                                );
                                match action {
                                    Some(LocationAction::Select(id)) => {
                                        self.select_registered_location(id)
                                    }
                                    Some(LocationAction::Remove(id)) => {
                                        if let Err(error) = self.remove_registered_location(id) {
                                            self.input_error = Some(error.to_string());
                                        }
                                    }
                                    None => {}
                                }
                            });
                        });
                        ui.separator();
                        ui.menu_button("결과 · 파일 작업", |ui| {
                            if ui.button("파일 작업 보기").clicked() {
                                self.file_ui.visible = true;
                                ui.close();
                            }
                            ui.separator();
                            let record = self
                                .scan
                                .completed()
                                .filter(|_| !self.locations_view.global_mode)
                                .map(|c| Arc::new(c.saved_result()));
                            let available = !self.scan.snapshots().is_empty();
                            self.results.menu(
                                ui,
                                self.scan.is_running() || self.file_blocked(),
                                record,
                                available,
                                self.archive_record.is_some(),
                            );
                        });
                    });
                },
            );
            let path_width = (ui.available_width()
                - browse_width
                - scan_width
                - 2.0 * ui.spacing().item_spacing.x)
                .max(80.0);
            let mut path = quick_scan_path
                .as_deref()
                .unwrap_or_else(|| self.target.text())
                .to_owned();
            if ui
                .add_enabled(
                    editable,
                    crate::ui_style::text_field(&mut path, path_width)
                        .id(egui::Id::unique("scan_path"))
                        .hint_text("분석할 폴더 경로"),
                )
                .changed()
            {
                self.target.set_text(path);
                self.input_error = None;
            }
            if ui
                .add_enabled(
                    editable,
                    egui::Button::new("폴더 선택").min_size(egui::vec2(browse_width, height)),
                )
                .clicked()
            {
                if let Some(path) = rfd::FileDialog::new().pick_folder() {
                    self.input_error = self.target.choose_folder(path).err();
                }
            }
            if ui
                .add_enabled(
                    editable || matches!(self.scan.status(), ScanStatus::Running),
                    egui::Button::new(
                        egui::RichText::new(if self.scan.is_running() {
                            "스캔 취소"
                        } else {
                            "스캔 시작"
                        })
                        .color(egui::Color32::from_rgb(6, 17, 27))
                        .strong(),
                    )
                    .fill(egui::Color32::from_rgb(56, 154, 236))
                    .stroke(egui::Stroke::new(
                        1.0,
                        egui::Color32::from_rgb(75, 166, 237),
                    ))
                    .min_size(egui::vec2(scan_width, height)),
                )
                .clicked()
            {
                if self.scan.is_running() {
                    self.scan.cancel();
                } else {
                    self.begin_scan();
                }
            }
        });
    }
    fn size_freshness(&self, id: u64, generation: u64) -> Option<SizeFreshness> {
        let entry = self.scan.get(id)?;
        let refreshing = self.file_refresh.pending_at(id) || entry.scan.is_running();
        if entry.stale && !refreshing {
            Some(SizeFreshness::NeedsRefresh)
        } else if refreshing
            || entry.generation != generation
            || (self.tracking.location == Some(id) && self.tracking.applying())
        {
            Some(SizeFreshness::Refreshing)
        } else if self.tracking.location == Some(id)
            && matches!(self.tracking.status, crate::tracking::Status::Following)
        {
            None
        } else {
            Some(SizeFreshness::LastScan)
        }
    }

    fn size_note(&self, sources: &[Snapshot]) -> Option<&'static str> {
        let mut highest = None;
        let mut following = false;
        for source in sources {
            let freshness = self.size_freshness(source.id, source.generation);
            highest = highest.max(freshness);
            following |= freshness.is_none();
        }
        match highest? {
            SizeFreshness::LastScan if following => Some("일부는 마지막 스캔 기준"),
            other => Some(other.label()),
        }
    }

    fn summary(&self, ui: &mut egui::Ui) {
        if self.scan.is_running() {
            crate::ui_style::activity_bar(ui);
            ui.add_space(2.0);
        }
        let sources = if self.show_preview() {
            Vec::new()
        } else if self.locations_view.global_mode {
            self.scan.snapshots()
        } else {
            self.source().into_iter().collect()
        };
        let mut bytes: u128 = sources
            .iter()
            .map(|s| s.scan.folders[0].logical_bytes as u128)
            .sum();
        let mut files: u64 = sources.iter().map(|s| s.scan.folders[0].file_count).sum();
        let mut folders: u64 = sources.iter().map(|s| s.scan.folders[0].folder_count).sum();
        let errors: u64 = sources.iter().map(|s| s.scan.error_count).sum();
        let elapsed = self
            .scan
            .elapsed()
            .map(|d| d.as_secs_f64())
            .unwrap_or_else(|| {
                self.scan
                    .completed()
                    .map_or(0.0, |c| c.elapsed.as_secs_f64())
            });
        let preview = self
            .show_preview()
            .then(|| self.scan.preview())
            .flatten()
            .and_then(|m| m.folders.first());
        if let Some(folder) = preview {
            bytes = folder.bytes as u128;
            files = folder.files;
            folders = folder.folders;
        }
        let has_results = !sources.is_empty() || preview.is_some();
        let live =
            self.tracking.running() && sources.iter().any(|s| Some(s.id) == self.tracking.location);
        let stale = sources
            .iter()
            .any(|s| self.scan.get(s.id).is_some_and(|e| e.stale));
        let size_note = self.size_note(&sources);
        let size = if !has_results {
            SizeDisplay {
                text: "—".into(),
                exact: "아직 스캔하지 않았습니다".into(),
            }
        } else if let Ok(bytes) = u64::try_from(bytes) {
            SizeDisplay::new(bytes)
        } else {
            SizeDisplay {
                text: "용량 합계 초과".into(),
                exact: format!("{bytes} B"),
            }
        };
        ui.horizontal_centered(|ui| {
            ui.spacing_mut().item_spacing.x = 22.0;
            ui.spacing_mut().interact_size.y = 22.0;
            ui.spacing_mut().button_padding = egui::vec2(8.0, 2.0);
            let state = if matches!(self.scan.status(), ScanStatus::Cancelling) {
                "취소 중"
            } else if self.scan.replacing() {
                "새 최상위 스캔 중 · 이전 결과"
            } else if self.scan.is_running() {
                "스캔 중"
            } else if stale {
                "갱신 필요"
            } else if live && matches!(self.tracking.status, crate::tracking::Status::Starting) {
                "갱신 준비 중"
            } else if live && self.tracking.excluded > 0 {
                "갱신 중 · 일부 제외"
            } else if live {
                "실시간 갱신 중"

            } else if has_results {
                "분석 완료"
            } else {
                "스캔 대기"
            };
            ui.small(state);
            let mut size_text = if preview.is_some() {
                    format!("{} · 계산 중", size.text)
                } else if errors > 0 {
                    format!("{} · 일부", size.text)
                } else {
                    size.text
                };
            if let Some(note) = size_note {
                size_text.push_str(" · ");
                size_text.push_str(note);
            }
            ui.label(egui::RichText::new(size_text).size(12.0)).on_hover_text(size.exact);
            ui.small(format!("파일 {}개", crate::ui_style::count(files)));
            ui.small(format!("폴더 {}개", crate::ui_style::count(folders)));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.small(format!("{elapsed:.2}초"));
                if errors > 0 {
                    ui.menu_button(egui::RichText::new(format!("수집 누락 {errors}건 · 상세")).color(egui::Color32::from_rgb(235,194,120)), |ui| {
                        ui.set_max_width(560.0);
                        ui.label("읽을 수 없는 항목은 검색에서 제외됩니다. 일부 누락된 폴더는 확인된 용량만 표시합니다.");
                        egui::ScrollArea::vertical().max_height(280.0).show(ui, |ui| {
                            for issue in sources.iter().flat_map(|s| &s.scan.issues).take(20) {
                                ui.label(format!("{}\n{}", issue.path.display(), issue.error));
                                ui.separator();
                            }
                        });
                        ui.small("상세는 최대 20건을 표시합니다. 폴더 내부에서 읽지 못한 항목 수는 알 수 없습니다.");
                    });
                }
            });
        });
    }
    fn tree(&mut self, ui: &mut egui::Ui) {
        crate::ui_style::panel_title(
            ui,
            "폴더 탐색",
            if self.show_preview() {
                "발견순"
            } else {
                "크기순"
            },
        );
        if self.show_preview() {
            self.preview_tree(ui);
            return;
        }
        let Some(source) = self.source() else {
            ui.add_space(15.0);
            ui.weak("  스캔 후 폴더가 표시됩니다");
            return;
        };
        if self.locations_view.global_mode {
            ui.weak("  스캔이 끝난 위치만 검색합니다.");
            return;
        }
        let Some(explorer) = &mut self.explorer else {
            return;
        };
        let mut destination = None;
        let mut open_folder = None;
        let mut toggle = None;
        let mut open = None;
        let height = (ui.available_height() - 26.0).max(30.0);
        egui::ScrollArea::both()
            .id_salt("folder_tree")
            .max_height(height)
            .show_rows(ui, 33.0, explorer.visible_tree().len(), |ui, rows| {
                // 보이는 행의 깊이만으로 가로 영역 확보. 큰 트리의 매 프레임 전체 순회 방지.
                let depth = rows
                    .clone()
                    .map(|row| explorer.visible_tree()[row].1)
                    .max()
                    .unwrap_or(0);
                let tree_width = ui.available_width().max(depth as f32 * 14.0 + 180.0);
                for row in rows {
                    let (folder, depth) = explorer.visible_tree()[row];
                    let (rect, _) =
                        ui.allocate_exact_size(egui::vec2(tree_width, 33.0), egui::Sense::hover());
                    let rect = rect.shrink2(egui::vec2(8.0, 0.5));
                    let selected = explorer.folder() == folder
                        || self.detail_target
                            == Some((source.id, source.generation, Item::Folder(folder)));
                    let arrow = egui::Rect::from_min_size(
                        rect.min + egui::vec2(depth as f32 * 14.0, 0.0),
                        egui::vec2(21.0, rect.height()),
                    );
                    let pick =
                        egui::Rect::from_min_max(egui::pos2(arrow.right(), rect.top()), rect.max);
                    let response = ui.interact(
                        pick,
                        ui.scope_id().with(("folder_select", folder)),
                        egui::Sense::click_and_drag(),
                    );
                    if self.archive_record.is_none()
                        && !self.file_job.busy()
                        && !self.file_refresh.busy()
                    {
                        response.dnd_set_drag_payload(crate::file_ui::DragPayload {
                            paths: vec![source.scan.folders[folder].path.clone()],
                        });
                        if let Some(payload) =
                            response.dnd_release_payload::<crate::file_ui::DragPayload>()
                        {
                            self.file_ui.drop_requested = Some((
                                payload.paths.clone(),
                                source.scan.folders[folder].path.clone(),
                            ));
                        }
                    }
                    if selected || response.hovered() {
                        ui.painter().rect_filled(
                            rect,
                            6.0,
                            if selected {
                                egui::Color32::from_rgb(34, 53, 72)
                            } else {
                                crate::ui_style::RAISED
                            },
                        );
                    }
                    if selected {
                        ui.painter().rect_stroke(
                            rect,
                            6.0,
                            egui::Stroke::new(1.0, egui::Color32::from_rgb(61, 102, 134)),
                            egui::StrokeKind::Inside,
                        );
                    }
                    if !source.scan.children[folder].is_empty() {
                        crate::ui_style::chevron(ui, arrow, explorer.is_expanded(folder));
                        if ui
                            .interact(
                                arrow,
                                ui.scope_id().with(("folder_toggle", folder)),
                                egui::Sense::click(),
                            )
                            .clicked()
                        {
                            toggle = Some(folder);
                        }
                    }
                    crate::ui_style::icon(ui, pick.shrink2(egui::vec2(3.0, 0.0)), true);
                    let name = egui::Rect::from_min_max(
                        pick.min + egui::vec2(25.0, 0.0),
                        pick.max - egui::vec2(68.0, 0.0),
                    );
                    crate::ui_style::text(
                        ui,
                        name,
                        &format!(
                            "{}{}",
                            item_name(&source.scan, Item::Folder(folder)).to_string_lossy(),
                            if source.scan.folders[folder].access
                                == crate::scanner::FolderAccess::Complete
                            {
                                String::new()
                            } else {
                                format!(" · {}", source.scan.folders[folder].access.label())
                            }
                        ),
                        false,
                        if source.scan.folders[folder].access.unreadable() {
                            crate::ui_style::SOFT
                        } else if selected {
                            crate::ui_style::TEXT
                        } else {
                            crate::ui_style::MUTED
                        },
                        13.0,
                    );
                    let size = egui::Rect::from_min_max(
                        egui::pos2(rect.right() - 68.0, rect.top()),
                        rect.max - egui::vec2(8.0, 0.0),
                    );
                    crate::ui_style::text(
                        ui,
                        size,
                        &if source.scan.folders[folder].access.unreadable() {
                            "확인 불가".into()
                        } else {
                            SizeDisplay::new(source.scan.folders[folder].logical_bytes).text
                        },
                        true,
                        crate::ui_style::SOFT,
                        11.0,
                    );
                    crate::file_ui::drop_hint(ui, &response);
                    if response.clicked() {
                        destination = Some(folder);
                    }
                    response
                        .on_hover_text(format!(
                            "{}\n{}",
                            source.scan.folders[folder].path.display(),
                            source.scan.folders[folder].access.label()
                        ))
                        .context_menu(|ui| {
                            crate::ui_style::menu_spacing(ui);
                            if let Some(right) = folder_open_menu(
                                ui,
                                self.archive_record.is_none()
                                    && !source.scan.folders[folder].access.unreadable(),
                            ) {
                                open_folder = Some((folder, right));
                            }
                            if crate::ui_style::menu_action(ui, "이 폴더에서 검색", true).clicked()
                            {
                                destination = Some(folder);
                                ui.close();
                            }
                            ui.separator();
                            self.file_ui.menu(
                                ui,
                                !self.file_job.busy()
                                    && !self.file_refresh.busy()
                                    && self.archive_record.is_none(),
                                Some(vec![source.scan.folders[folder].path.clone()]),
                                1,
                                true,
                            );
                            if let Some(action) = open_menu(ui, TargetKind::Folder) {
                                open = Some(OpenRequest::for_item(
                                    &source.scan,
                                    Item::Folder(folder),
                                    action,
                                ));
                            }
                        });
                }
            });
        ui.add_space(5.0);
        ui.small("   화살표를 눌러 펼쳐 보세요.");
        if let Some(folder) = toggle {
            explorer.toggle_folder(&source.scan, folder);
        }
        if let Some(folder) = destination {
            self.navigate(folder, false);
        }
        if let Some(open) = open {
            self.start_open(open);
        }
        if let Some((folder, right)) = open_folder {
            self.input_error = self
                .open_folder_tab(&source, folder, right)
                .err()
                .map(|error| error.to_string());
        }
    }

    fn choose_main_search_scope(&mut self, choice: ScopeChoice) {
        match choice {
            ScopeChoice::Scan(scope) => {
                if self.inline_search.visible {
                    self.search.input.text = std::mem::take(&mut self.inline_search.text);
                    self.search.input.mode = self.inline_search.mode;
                    self.inline_search_ui.close_inline(&mut self.inline_search);
                }
                self.locations_view.choose_scope(&mut self.search, scope);
                self.detail_target = None;
            }
            ScopeChoice::Direct(scope) => {
                if !self.inline_search.visible {
                    self.inline_search_ui.close_inline(&mut self.inline_search);
                    self.inline_search.text = self.search.input.text.clone();
                    self.inline_search.mode = self.search.input.mode;
                }
                self.inline_search.set_scope(scope);
                self.inline_search_ui.error = None;
                self.inline_search.open();
            }
        }
    }

    fn inline_search_panel(&mut self, ui: &mut egui::Ui) {
        use crate::quick_ui::InlineAction;
        let interactive = !self.file_ui.blocked(&self.file_job)
            && self.root_replacement.is_none()
            && self.settings_draft.is_none();
        match self.inline_search_ui.show_inline(
            ui,
            &mut self.inline_search,
            &self.scan,
            interactive,
        ) {
            Some(InlineAction::Scope(choice)) => self.choose_main_search_scope(choice),
            Some(InlineAction::Close) => {
                self.inline_search_ui.close_inline(&mut self.inline_search)
            }
            Some(InlineAction::Result(action)) => {
                self.apply_search_action(ui.ctx(), SearchSurface::Inline, action)
            }
            None => {}
        }
    }

    fn center(&mut self, ui: &mut egui::Ui) {
        self.preview_banner(ui);
        if self.show_preview() && !self.inline_search.visible {
            self.preview_table(ui);
            return;
        }
        let mut destination = None;
        let mut history_direction = None;
        let history_back = self.history.can_back();
        let history_forward = self.history.can_forward();
        let compact = ui.available_height() < 300.0;
        let available = ui.available_width();
        egui::Frame::new()
            .inner_margin(egui::Margin::symmetric(12, 0))
            .show(ui, |ui| {
                crate::ui_style::slot(
                    ui,
                    egui::vec2(available - 24.0, 32.0),
                    egui::Layout::left_to_right(egui::Align::Center),
                    |ui| {
                        ui.spacing_mut().button_padding = egui::vec2(6.0, 3.0);
                        ui.spacing_mut().interact_size.y = 24.0;
                        let (Some(source), Some(explorer)) = (self.source(), &self.explorer) else {
                            ui.weak("스캔 후 폴더가 표시됩니다.");
                            return;
                        };
                        let folder = explorer.folder();
                        if crate::ui_style::navigation_button(ui, history_back, -1).clicked() {
                            history_direction = Some(false);
                        }
                        if crate::ui_style::navigation_button(ui, history_forward, 1).clicked() {
                            history_direction = Some(true);
                        }
                        let global = self.locations_view.global_mode;
                        let can_go_up = folder != 0 && !global && !self.search.whole_scan;
                        if crate::ui_style::navigation_button(ui, can_go_up, 0).clicked() {
                            destination = source.scan.folders[folder].parent;
                        }
                        if !self.folder_tabs.is_split() {
                            ui.small("현재 폴더");
                        }
                        let path = &source.scan.folders[folder].path;
                        let display = path.display().to_string();
                        let (rect, response) = ui.allocate_exact_size(
                            egui::vec2(ui.available_width(), 30.0),
                            egui::Sense::click(),
                        );
                        let label = self
                            .folder_tabs
                            .is_split()
                            .then(|| folders::pane_path_label(ui, path, rect.width()));
                        crate::ui_style::text(
                            ui,
                            rect,
                            label.as_deref().unwrap_or(&display),
                            false,
                            crate::ui_style::TEXT,
                            13.0,
                        );
                        let response = response.on_hover_text(format!("현재 폴더\n{display}"));
                        if !global {
                            response.context_menu(|ui| {
                                crate::ui_style::menu_spacing(ui);
                                for parent in explorer.breadcrumbs(&source.scan) {
                                    if ui
                                        .button(
                                            source.scan.folders[parent].path.display().to_string(),
                                        )
                                        .clicked()
                                    {
                                        destination = Some(parent);
                                        ui.close();
                                    }
                                }
                            });
                        }
                    },
                );
            });
        if let Some(forward) = history_direction {
            self.history_move(forward);
            return;
        }
        if let Some(folder) = destination.take() {
            self.navigate(folder, false);
        }
        ui.painter().hline(
            ui.min_rect().x_range(),
            ui.cursor().top(),
            egui::Stroke::new(1.0, crate::ui_style::LINE),
        );
        egui::Frame::new()
            .inner_margin(egui::Margin::symmetric(5, if compact { 1 } else { 3 }))
            .show(ui, |ui| {
                let narrow = ui.available_width() < 420.0;
                if narrow {
                    ui.spacing_mut().button_padding = egui::vec2(8.0, 5.0);
                }
                egui::ScrollArea::horizontal()
                    .id_salt("center_tabs")
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            for (tab, label) in [
                                (0, "폴더·파일"),
                                (3, "검색 결과"),
                                (1, "대용량 파일"),
                                (2, "확장자"),
                                (4, "중복 파일"),
                            ] {
                                if tab == 3 && !self.search.input.is_active() {
                                    continue;
                                }
                                if ui
                                    .add(
                                        egui::Button::new(egui::RichText::new(label).color(
                                            if self.tab == tab {
                                                egui::Color32::from_rgb(165, 217, 255)
                                            } else {
                                                crate::ui_style::MUTED
                                            },
                                        ))
                                        .fill(if self.tab == tab {
                                            egui::Color32::from_rgb(33, 56, 76)
                                        } else {
                                            egui::Color32::TRANSPARENT
                                        })
                                        .frame(self.tab == tab)
                                        .min_size(
                                            egui::vec2(
                                                if narrow { 65.0 } else { 75.0 },
                                                if compact { 26.0 } else { 32.0 },
                                            ),
                                        ),
                                    )
                                    .clicked()
                                {
                                    if self.inline_search.visible {
                                        self.inline_search_ui.close_inline(&mut self.inline_search);
                                    }
                                    if tab == 0 {
                                        self.search.clear_conditions();
                                        self.detail_target = None;
                                    }
                                    self.tab = tab;
                                }
                            }
                            if matches!(self.tab, 1 | 2) {
                                if narrow {
                                    ui.menu_button("범위", |ui| {
                                        ui.checkbox(&mut self.statistics.matched, "검색된 파일만");
                                    });
                                } else {
                                    ui.checkbox(&mut self.statistics.matched, "검색된 파일만");
                                }
                            }
                        });
                    });
            });
        if self.inline_search.visible {
            egui::Frame::new()
                .inner_margin(egui::Margin::symmetric(8, 0))
                .show(ui, |ui| self.inline_search_panel(ui));
            return;
        }
        let chosen_scope = egui::Frame::new()
            .inner_margin(egui::Margin::symmetric(8, 0))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    self.search.controls(
                        ui,
                        self.locations_view.global_mode,
                        self.archive_record.is_none(),
                    )
                })
                .inner
            })
            .inner;
        if let Some(choice) = chosen_scope {
            self.choose_main_search_scope(choice);
            ui.ctx().request_repaint();
            if self.inline_search.visible {
                return;
            }
        }
        if self.inline_search.can_return_from_analysis() {
            if ui.button("검색 결과로 돌아가기").clicked() {
                self.inline_search.open();
                ui.ctx().request_repaint();
            }
        } else if self.quick.can_return_from_analysis()
            && ui.button("빠른 검색 결과로 돌아가기").clicked()
        {
            self.quick.open();
            ui.ctx().request_repaint();
        }
        if self.search.input.is_active()
            || self.search.input.mode != crate::search::SearchMode::Name
            || !self.search.input.recursive
        {
            egui::Frame::new()
                .fill(crate::ui_style::SIDEBAR)
                .inner_margin(egui::Margin::symmetric(8, 0))
                .show(ui, |ui| {
                    crate::ui_style::slot(
                        ui,
                        egui::vec2(available - 16.0, 30.0),
                        egui::Layout::left_to_right(egui::Align::Center),
                        |ui| self.search.filter_chips(ui),
                    );
                });
        }
        self.refresh_derived();
        if self.tab == 4 {
            let sources = self.scan.snapshots();
            let context = crate::duplicate_ui::Context {
                excluded: self.scan.entries.len().saturating_sub(sources.len()),
                sources,
                selected: self.scan.selected_id(),
                folder: self.explorer.as_ref().map(Explorer::folder),
                blocked: self.scan.is_running()
                    || self.file_ui.blocked(&self.file_job)
                    || self.file_refresh.busy()
                    || self.results.busy()
                    || self.tracking.running(),
                archived: self.archive_record.is_some(),
            };
            let (open, analyze) = egui::Frame::new()
                .inner_margin(12)
                .show(ui, |ui| {
                    self.duplicate_view.show(ui, &mut self.duplicates, &context)
                })
                .inner;
            if let Some(target) = self.duplicate_view.picked {
                self.detail_target = Some(target);
            }
            if let Some(open) = open {
                self.start_open(open);
            }
            if let Some((id, generation, item)) = analyze {
                self.analyze_at(id, generation, item);
            }
            return;
        }
        if matches!(self.tab, 1 | 2) {
            self.statistics.large_files = self.tab == 1;
            if self.tab == 1 {
                ui.horizontal(|ui| {
                    for n in [10, 50, 100] {
                        ui.selectable_value(
                            &mut self.statistics.top_count,
                            n,
                            format!("상위 {n}개"),
                        );
                    }
                });
            }
            let (open, analyze) = self.statistics.show(ui);
            if let Some(target) = self.statistics.picked {
                self.detail_target = Some(target);
            }
            if let Some(open) = open {
                self.start_open(open);
            }
            if let Some((id, generation, item)) = analyze {
                self.analyze_at(id, generation, item);
            }
            return;
        }
        if self.locations_view.global_mode {
            let (open, analyze) = self.locations_view.global(ui, &self.scan, &mut self.search);
            if self.locations_view.selection_changed {
                self.detail_target = self.locations_view.selected_target();
            }
            if let Some(open) = open {
                self.start_open(open);
            }
            if let Some((id, generation, item)) = analyze {
                self.analyze_at(id, generation, item);
            }
            return;
        }
        let Some(source) = self.source() else {
            egui::Frame::new().inner_margin(24).show(ui, |ui| {
                ui.weak("선택한 경로에서 ‘스캔 시작’을 누르면 파일과 폴더가 표시됩니다");
            });
            return;
        };
        if self.draw_live_folder(ui, &source) {
            return;
        }
        self.search.headers(ui);
        self.search.poll();
        let mut open = None;
        let mut enter = None;
        let mut analyze = false;
        let mut pick = None;
        let mut open_folder = None;
        if let Some(results) = self.search.job.results() {
            if results.items.is_empty() {
                ui.weak(if self.search.input.is_active() {
                    "검색 결과가 없습니다. 검색어나 범위를 바꿔 보세요."
                } else if source.scan.folders[self.explorer.as_ref().map_or(0, |e| e.folder())]
                    .access
                    .unreadable()
                {
                    "이 폴더의 목록을 읽을 수 없습니다. 접근 권한 또는 경로 상태를 확인하세요."
                } else {
                    "이 폴더는 비어 있습니다."
                });
            }
            if !self.search.editing
                && ui.input_mut(|i| i.consume_key(egui::Modifiers::CTRL, egui::Key::A))
            {
                self.search.selection.all(results.items.len());
                self.detail_target = None;
            }
            let mut scroll = egui::ScrollArea::both()
                .id_salt("entry_list")
                .horizontal_scroll_offset(self.search.horizontal_offset)
                .vertical_scroll_offset(self.search.vertical_offset)
                .max_height((ui.available_height() - if compact { 0.0 } else { 28.0 }).max(20.0));
            if let Some(row) = self.search.reveal_row.take() {
                scroll = scroll.vertical_scroll_offset(row as f32 * crate::ui_style::ROW);
            }
            let output =
                scroll.show_rows(ui, crate::ui_style::ROW, results.items.len(), |ui, rows| {
                    for row in rows {
                        let item = results.items[row];
                        let response = crate::ui_style::row(
                            ui,
                            row,
                            self.search.widths.total(
                                self.search.show_path,
                                self.search.show_modified,
                                self.search.show_allocated,
                            ),
                            self.search.selection.contains(row),
                            |ui| {
                                crate::ui_style::name_cell(
                                    ui,
                                    self.search.widths.get(0),
                                    &match item {
                                        Item::Folder(id)
                                            if source.scan.folders[id].access
                                                != crate::scanner::FolderAccess::Complete =>
                                        {
                                            format!(
                                                "{} · {}",
                                                item_name(&source.scan, item).to_string_lossy(),
                                                source.scan.folders[id].access.label()
                                            )
                                        }
                                        _ => item_name(&source.scan, item)
                                            .to_string_lossy()
                                            .into_owned(),
                                    },
                                    matches!(item, Item::Folder(_)),
                                );
                                self.search.widths.cells(
                                    ui,
                                    &source.scan,
                                    item,
                                    self.search.show_path.then_some(self.search.show_source),
                                    self.search.show_modified,
                                    self.search.show_allocated,
                                );
                            },
                        );
                        let response = response.interact(egui::Sense::click_and_drag());
                        if self.archive_record.is_none()
                            && !self.file_job.busy()
                            && !self.file_refresh.busy()
                        {
                            if response.drag_started() {
                                let paths = if self.search.selection.contains(row) {
                                    self.search
                                        .selection
                                        .rows()
                                        .filter_map(|r| results.items.get(r))
                                        .map(|i| crate::search::item_path(&source.scan, *i))
                                        .collect()
                                } else {
                                    vec![crate::search::item_path(&source.scan, item)]
                                };
                                response
                                    .dnd_set_drag_payload(crate::file_ui::DragPayload { paths });
                            }
                            if let Item::Folder(id) = item {
                                crate::file_ui::drop_hint(ui, &response);
                                if let Some(payload) =
                                    response.dnd_release_payload::<crate::file_ui::DragPayload>()
                                {
                                    self.file_ui.drop_requested = Some((
                                        payload.paths.clone(),
                                        source.scan.folders[id].path.clone(),
                                    ));
                                }
                            }
                        }
                        if response.clicked() {
                            let m = ui.input(|i| i.modifiers);
                            self.search.selection.click(row, m.ctrl, m.shift);
                            pick = self.search.selection.single(&results.items);
                            self.detail_target = None;
                        }
                        if response.secondary_clicked() && !self.search.selection.contains(row) {
                            self.search.selection.click(row, false, false);
                            pick = Some(item);
                        }
                        if response.double_clicked() && self.search.selection.count() <= 1 {
                            match item {
                                Item::Folder(id) => enter = Some(id),
                                Item::File(_) => {
                                    open = Some(OpenRequest::for_item(
                                        &source.scan,
                                        item,
                                        OpenAction::OpenFile,
                                    ))
                                }
                            }
                        }
                        response.context_menu(|ui| {
                            crate::ui_style::menu_spacing(ui);
                            self.file_ui.menu(
                                ui,
                                !self.file_job.busy()
                                    && !self.file_refresh.busy()
                                    && self.archive_record.is_none(),
                                None,
                                self.search.selection.count(),
                                true,
                            );
                            ui.separator();
                            ui.add_enabled_ui(self.search.selection.count() <= 1, |ui| {
                                if crate::ui_style::menu_action(ui, "용량 분석으로 이동", true)
                                    .clicked()
                                {
                                    analyze = true;
                                    ui.close();
                                }
                                if let Item::Folder(id) = item {
                                    if let Some(right) = folder_open_menu(
                                        ui,
                                        self.archive_record.is_none()
                                            && !source.scan.folders[id].access.unreadable(),
                                    ) {
                                        open_folder = Some((id, right));
                                    }
                                    if crate::ui_style::menu_action(ui, "이 폴더에서 검색", true)
                                        .clicked()
                                    {
                                        destination = Some(id);
                                        ui.close();
                                    }
                                }
                                if let Some(action) = open_menu(ui, item.into()) {
                                    open = Some(OpenRequest::for_item(&source.scan, item, action));
                                }
                            });
                        });
                    }
                });
            self.search.horizontal_offset = output.state.clamped_offset().x;
            self.search.vertical_offset = output.state.clamped_offset().y;
        }
        if !compact {
            egui::Frame::new()
                .fill(crate::ui_style::SIDEBAR)
                .inner_margin(egui::Margin::symmetric(12, 6))
                .show(ui, |ui| {
                    ui.set_min_width(ui.available_width());
                    ui.horizontal(|ui| {
                        self.search.status(ui);
                        if self.search.selection.count() > 0 {
                            ui.menu_button("선택 항목 작업", |ui| {
                                self.file_ui.menu(
                                    ui,
                                    !self.file_job.busy()
                                        && !self.file_refresh.busy()
                                        && self.archive_record.is_none(),
                                    None,
                                    self.search.selection.count(),
                                    true,
                                )
                            });
                        }
                    });
                    if let Some(explorer) = &self.explorer {
                        ui.response().on_hover_text(format!(
                            "바로 아래 항목 {}개 · {}",
                            explorer.entries().len(),
                            self.scan
                                .completed()
                                .map(|c| c.path.display().to_string())
                                .unwrap_or_default()
                        ));
                    }
                });
        }
        if let Some(item) = pick {
            self.detail_target = Some((source.id, source.generation, item));
            if let Some(explorer) = &mut self.explorer {
                explorer.select(item);
            }
        }
        if analyze {
            let previous = self.capture_navigation();
            if let Some(explorer) = &mut self.explorer {
                self.search.analyze(&source.scan, explorer);
                self.tab = 0;
            }
            self.remember_navigation(previous);
        }
        if let Some(folder) = destination {
            self.navigate(folder, false);
        }
        if let Some(folder) = enter {
            self.navigate(folder, true);
        }
        if let Some(open) = open {
            self.start_open(open);
        }
        if let Some((folder, right)) = open_folder {
            self.input_error = self
                .open_folder_tab(&source, folder, right)
                .err()
                .map(|error| error.to_string());
        }
    }
    fn details_title(&self) -> &'static str {
        if self.show_preview() {
            return "스캔 미리보기";
        }
        if let Some(count) = self.live_selection_count() {
            return if count == 0 {
                "현재 폴더"
            } else {
                "선택 항목"
            };
        }
        let selected = if self.locations_view.global_mode {
            self.locations_view.selection_summary().count() > 0
        } else {
            self.search.selection.count() > 0
        };
        if selected || self.detail_target.is_some() {
            "선택 항목"
        } else if !self.locations_view.global_mode && self.source().is_some() {
            "현재 폴더"
        } else {
            "상세 정보"
        }
    }

    fn details(&mut self, ui: &mut egui::Ui) {
        if self.show_preview() {
            self.preview_details(ui);
            return;
        }
        if self.live_details(ui) {
            return;
        }
        let summary = if self.locations_view.global_mode {
            self.locations_view.selection_summary()
        } else {
            self.search.selection_summary()
        };
        if summary.count() > 1 {
            ui.strong(format!("{}개 선택", summary.count()));
            crate::ui_style::property(ui, "파일", &format!("{}개", summary.files));
            crate::ui_style::property(ui, "폴더", &format!("{}개", summary.folders));
            ui.small("하나를 선택하면 파일 또는 폴더의 상세 정보를 볼 수 있습니다.");
            return;
        }

        let target = self.selection_target();
        let Some((id, generation, item)) = target else {
            ui.weak("파일이나 폴더를 선택하면 상세 정보가 표시됩니다.");
            if let Some(output) = &self.statistics.output {
                allocation_summary(ui, &output.stats);
            }
            return;
        };
        let Some(entry) = self.scan.get(id).filter(|e| e.generation == generation) else {
            return;
        };
        let Some(completed) = entry.scan.completed() else {
            return;
        };
        let scan = Arc::clone(&completed.result);
        let info = detail(&scan, item);
        let (card, response) =
            ui.allocate_exact_size(egui::vec2(ui.available_width(), 82.0), egui::Sense::hover());
        ui.painter().rect_filled(card, 7.0, crate::ui_style::RAISED);
        ui.painter().rect_stroke(
            card,
            7.0,
            egui::Stroke::new(1.0, crate::ui_style::LINE),
            egui::StrokeKind::Inside,
        );
        let inner = card.shrink2(egui::vec2(12.0, 9.0));
        let top = egui::Rect::from_min_size(inner.min, egui::vec2(inner.width(), 15.0));
        crate::ui_style::text(ui, top, info.kind, false, crate::ui_style::MUTED, 11.0);
        crate::ui_style::text(ui, top, &info.parent_percentage, true, accent(), 11.0);
        let name = egui::Rect::from_min_size(
            inner.min + egui::vec2(0.0, 20.0),
            egui::vec2(inner.width(), 20.0),
        );
        crate::ui_style::text(
            ui,
            name,
            &item_name(&scan, item).to_string_lossy(),
            false,
            crate::ui_style::TEXT,
            15.0,
        );
        let path = egui::Rect::from_min_size(
            inner.min + egui::vec2(0.0, 47.0),
            egui::vec2(inner.width(), 15.0),
        );
        crate::ui_style::text(
            ui,
            path,
            &info.path.display().to_string(),
            false,
            crate::ui_style::SOFT,
            10.0,
        );
        response.on_hover_text(info.path.display().to_string());
        ui.add_space(5.0);
        let freshness = if matches!(item, Item::Folder(_)) {
            self.size_freshness(id, generation)
        } else {
            None
        };
        if let Some(freshness) = freshness {
            let size = format!("{} · {}", info.size.text, freshness.label());
            ui.horizontal(|ui| {
                ui.add(egui::Label::new(
                    egui::RichText::new("크기")
                        .size(11.0)
                        .color(crate::ui_style::MUTED),
                ));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.add_space(4.0);
                    ui.add(egui::Label::new(egui::RichText::new(&size).size(12.0)).wrap())
                        .on_hover_text(format!("{}\n{}", info.size.exact, freshness.label()));
                });
            });
        } else {
            crate::ui_style::property(ui, "크기", &info.size.text).on_hover_text(info.size.exact);
        }
        crate::ui_style::property(ui, "할당 크기", &info.allocated.text)
            .on_hover_text(info.allocated.exact);
        if let (Some(files), Some(folders)) = (info.files, info.folders) {
            crate::ui_style::property(ui, "파일", &format!("{}개", crate::ui_style::count(files)));
            crate::ui_style::property(
                ui,
                "하위 폴더",
                &format!("{}개", crate::ui_style::count(folders)),
            );
        }
        crate::ui_style::property(ui, "수정 날짜", &info.modified);
        ui.collapsing("파일 상세 정보", |ui| {
            crate::ui_style::property(ui, "만든 날짜", &info.created);
            ui.small(format!("속성 {}", info.attributes));
            if let Item::File(file) = item {
                let identity = scan.files[file].extra.identity.value();
                self.link_details
                    .select(identity.map(|identity| crate::link_details::Key {
                        path: info.path.clone(),
                        identity,
                        source: id,
                        generation,
                    }));
                self.link_details.poll();
                let links = match &self.link_details.value {
                    Some(Ok(n)) => n.to_string(),
                    Some(Err(error)) => error.clone(),
                    None => info.links.clone(),
                };
                crate::ui_style::property(ui, "같은 파일의 연결 개수", &links);
                if self.archive_record.is_none() && identity.is_some() {
                    if self.link_details.busy() {
                        ui.horizontal(|ui| {
                            crate::ui_style::activity_indicator(ui);
                            ui.label("연결 개수 확인 중…");
                        });
                        ui.ctx().request_repaint_after(Duration::from_millis(30));
                    } else if ui.button("연결 개수 확인").clicked() {
                        self.input_error = self.link_details.start().err().map(|e| e.to_string());
                    }
                }
                ui.small(format!("파일 ID {}", info.identity));
                if scan.files[file].extra.links.value().is_some_and(|n| n > 1) {
                    ui.small("같은 파일에 여러 이름이 있습니다. 경로별 합계에 중복될 수 있습니다");
                }
            }
        });
        egui::CollapsingHeader::new("용량 통계")
            .default_open(true)
            .show(ui, |ui| {
                if let Some(output) = &self.statistics.output {
                    type_chart(ui, &output.stats);
                    allocation_summary(ui, &output.stats);
                }
                ui.small("크기 필터와 공간 지도는 파일 크기를 기준으로 표시합니다.");
            });
    }
    fn map_panel(&mut self, ui: &mut egui::Ui) {
        if self.show_preview() {
            ui.horizontal_wrapped(|ui| {
                ui.strong("공간 지도");
                ui.small("스캔을 마치면 확인할 수 있습니다.");
            });
            return;
        }
        if !self.map_collapsed {
            let (grip, response) =
                ui.allocate_exact_size(egui::vec2(ui.available_width(), 5.0), egui::Sense::drag());
            ui.painter().hline(
                (grip.center().x - 16.0)..=(grip.center().x + 16.0),
                grip.center().y,
                egui::Stroke::new(2.0, crate::ui_style::SOFT),
            );
            if response.dragged() {
                self.map_height = (self.map_height - response.drag_delta().y).clamp(96.0, 360.0);
            }
            response.on_hover_cursor(egui::CursorIcon::ResizeVertical);
        }
        ui.spacing_mut().interact_size.y = 24.0;
        ui.spacing_mut().button_padding = egui::vec2(8.0, 3.0);
        let width = ui.available_width();
        crate::ui_style::slot(
            ui,
            egui::vec2(
                width,
                if self.map_collapsed || ui.available_height() < 140.0 {
                    26.0
                } else {
                    34.0
                },
            ),
            egui::Layout::left_to_right(egui::Align::Center),
            |ui| {
                ui.strong(egui::RichText::new("공간 지도").size(12.0));
                if width > 1050.0 {
                    ui.small("사각형 크기는 파일 용량에 비례합니다");
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui
                        .button(if self.map_collapsed {
                            "펼치기 ↑"
                        } else {
                            "접기 ↓"
                        })
                        .clicked()
                    {
                        self.map_collapsed = !self.map_collapsed;
                    }
                });
            },
        );
        if self.map_collapsed {
            return;
        }
        let selected = self.selection_target();
        let size_note = self
            .map
            .output
            .as_ref()
            .and_then(|output| self.size_note(&output.sources));
        if let Some(event) = self.map_view.show(ui, &self.map, selected, size_note) {
            match event {
                MapEvent::Pick(row) => self.pick_map(row),
                MapEvent::Analyze(row) => {
                    if let Some((id, generation, item)) = self.map.route(row) {
                        self.analyze_at(id, generation, item);
                    }
                }
                MapEvent::Open(row, action) => {
                    if let Some(open) = self.map.open(row, action) {
                        self.start_open(open);
                    }
                }
            }
        }
    }
}

impl eframe::App for DiskWeaveApp {
    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.tracking.stop();
    }
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if self.archive_record.is_none() {
            self.quick_logic(ctx);
            self.tick_background(ctx);
        }
    }
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        if self.archive_record.is_none() {
            self.quick_window(ui.ctx());
        }
        let key = self
            .selection_target()
            .and_then(|(source, generation, item)| {
                let Item::File(file) = item else {
                    return None;
                };
                let entry = self
                    .scan
                    .get(source)
                    .filter(|e| e.generation == generation)?;
                let scan = &entry.scan.completed()?.result;
                Some(crate::link_details::Key {
                    path: scan.folders[scan.files[file].parent]
                        .path
                        .join(&scan.files[file].name),
                    identity: scan.files[file].extra.identity.value()?,
                    source,
                    generation,
                })
            });
        self.link_details.select(key);
        self.link_details.poll();
        if self.link_details.busy() {
            ui.ctx().request_repaint_after(Duration::from_millis(30));
        }

        if self.archive_record.is_some() {
            self.document_tick();
        }
        let document = self
            .quick_folder_scan
            .is_none()
            .then_some(&self.results.job.view);
        if let Some(crate::documents::View::Comparison(report)) = document {
            self.visible_watches.clear();
            let report = Arc::clone(report);
            egui::CentralPanel::default()
                .frame(egui::Frame::new().fill(bg()))
                .show(ui, |ui| {
                    if let Some(error) = &self.input_error {
                        ui.colored_label(egui::Color32::LIGHT_RED, error);
                    }
                    self.results.comparison(ui, &report);
                });
            return;
        }
        if let Some(crate::documents::View::Saved(record)) = document {
            let saved = crate::local_date::display(Some(record.saved));
            let busy = self
                .saved_viewer
                .as_ref()
                .is_some_and(|viewer| viewer.results.busy());
            let mut back = false;
            egui::Panel::top("saved_navigation")
                .exact_size(if self.input_error.is_some() {
                    60.0
                } else {
                    36.0
                })
                .frame(egui::Frame::new().fill(surface()).inner_margin(4))
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        back = ui
                            .add_enabled(!busy, egui::Button::new("← 현재 분석으로 돌아가기"))
                            .clicked();
                        ui.label(format!("저장된 결과 · {saved}"));
                    });
                    if let Some(error) = &self.input_error {
                        ui.colored_label(egui::Color32::LIGHT_RED, error);
                    }
                });
            if !back {
                self.visible_watches.clear();
                if let Some(viewer) = &mut self.saved_viewer {
                    viewer.ui(ui, _frame);
                }
                return;
            }
            self.results.job.back();
            self.saved_viewer = None;
            self.input_error = None;
        }
        if self.map_height == 0.0 {
            self.map_height = 192.0;
            if ui.ctx().content_rect().height() < 650.0 {
                self.map_collapsed = true;
            }
        }
        if self.archive_record.is_some() {
            self.file_open.poll();
            self.sync_scan();
        }
        let dropped: Vec<_> = ui.ctx().input(|i| {
            i.raw
                .dropped_files
                .iter()
                .map(|f| f.path().to_path_buf())
                .collect()
        });
        if !dropped.is_empty() && self.archive_record.is_none() {
            let context = self.file_context(None);
            if self.file_ui.importing || !self.file_blocked() {
                match self.file_ui.route_drop(dropped, context) {
                    Ok(Some(paths)) => self.accept_drop(paths),
                    Ok(None) => {}
                    Err(error) => self.input_error = Some(error.to_string()),
                }
            }
        }
        self.refresh_derived();
        let size = ui.available_size();
        let mut panels = crate::workspace::Panels::new(
            size.x,
            size.y,
            self.map_height,
            self.map_collapsed || self.show_preview(),
        );
        if self.inline_search.visible {
            panels.center += panels.right;
            panels.right = 0.0;
        }
        if self.folder_tabs.is_split()
            && panels.right > 0.0
            && crate::workspace::folders::split_widths(panels.center, 0.5).is_none()
        {
            panels.center += panels.right;
            panels.right = 0.0;
        }
        if self.tab == 4 || self.inline_search.visible {
            panels.body += panels.map;
            panels.map = 0.0;
        }
        if self.archive_record.is_some() {
            panels.body += panels.target - 48.0;
            panels.target = 48.0;
        }
        egui::Panel::top("brand")
            .exact_size(panels.header)
            .frame(
                egui::Frame::new()
                    .fill(egui::Color32::from_rgb(16, 23, 32))
                    .inner_margin(egui::Margin::symmetric(12, 4)),
            )
            .show(ui, |ui| {
                self.header(ui, panels.right > 0.0 || self.inline_search.visible)
            });
        egui::Panel::top("target")
            .exact_size(panels.target)
            .frame(
                egui::Frame::new()
                    .fill(crate::ui_style::SURFACE)
                    .inner_margin(egui::Margin::symmetric(14, 8)),
            )
            .show(ui, |ui| {
                if let Some(record) = &self.archive_record {
                    ui.add(
                        egui::Label::new(format!(
                            "저장된 분석 결과 · {}",
                            record.scan.folders[0].path.display()
                        ))
                        .truncate(),
                    )
                    .on_hover_text(record.scan.folders[0].path.display().to_string());
                    ui.small(format!(
                        "스캔 {} ~ {} · 자동으로 다시 스캔하지 않습니다.",
                        crate::local_date::display(Some(record.started)),
                        crate::local_date::display(Some(record.finished))
                    ));
                } else {
                    self.toolbar(ui);
                }
            });
        self.sync_scan();
        egui::Panel::top("summary")
            .exact_size(panels.summary)
            .frame(
                egui::Frame::new()
                    .fill(bg())
                    .inner_margin(egui::Margin::symmetric(
                        14,
                        if self.scan.is_running() { 2 } else { 4 },
                    )),
            )
            .show(ui, |ui| self.summary(ui));
        egui::Panel::bottom("status")
            .exact_size(panels.status)
            .frame(
                egui::Frame::new()
                    .fill(surface())
                    .inner_margin(egui::Margin::symmetric(14, 4)),
            )
            .show(ui, |ui| {
                ui.spacing_mut().interact_size.y = 18.0;
                ui.spacing_mut().item_spacing.x = 12.0;
                ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                    if self.scan.is_running() {
                        crate::ui_style::activity_indicator(ui);
                    }
                    let status = self.status();
                    let status_width = (ui.available_width() * 0.40).min(500.0);
                    ui.scope(|ui| {
                        ui.set_max_width(status_width);
                        ui.add(egui::Label::new(egui::RichText::new(&status).small()).truncate())
                    })
                    .inner
                    .on_hover_text(format!(
                        "{status}\n{}",
                        self.scan
                            .active_path()
                            .map(|p| p.display().to_string())
                            .unwrap_or_default()
                    ));
                    if let Some(report) = self
                        .scan
                        .completed()
                        .and_then(|c| c.engine.as_ref())
                        .filter(|r| r.fallback.is_some())
                    {
                        let button = ui.small_button("전환 이유");
                        egui::Popup::menu(&button).show(|ui| {
                            ui.set_max_width(320.0);
                            ui.label(report.label());
                        });
                    }
                    let sources = if self.locations_view.global_mode {
                        self.scan.snapshots()
                    } else {
                        self.source().into_iter().collect()
                    };
                    let errors: u64 = sources.iter().map(|s| s.scan.error_count).sum();
                    let skipped: u64 = sources.iter().map(|s| s.scan.skipped_reparse_points).sum();
                    ui.small(format!("수집 누락 {errors}건 · 특수 경로 제외 {skipped}"));
                    if !sources.is_empty() {
                        let detailed = sources.iter().filter(|s| s.scan.detailed).count();
                        ui.small(if detailed == sources.len() {
                            "할당 크기 확인"
                        } else if detailed == 0 {
                            "파일 크기만 확인"
                        } else {
                            "스캔 설정이 다른 결과 포함"
                        });
                    }
                    let extra_errors: u64 =
                        sources.iter().map(|s| s.scan.metadata_error_count).sum();
                    if extra_errors > 0 {
                        ui.colored_label(
                            egui::Color32::YELLOW,
                            format!("디스크 정보 확인 실패 {extra_errors}개"),
                        )
                        .on_hover_ui(|ui| {
                            for issue in sources
                                .iter()
                                .flat_map(|s| s.scan.metadata_issues.iter())
                                .take(5)
                            {
                                ui.label(format!("{}: {}", issue.path.display(), issue.error));
                            }
                        });
                    }
                    if let Some(results) = self
                        .search
                        .job
                        .results()
                        .filter(|_| !self.locations_view.global_mode)
                    {
                        if results.unknown_dates > 0 {
                            ui.small(format!(
                                "수정 날짜 확인 불가 {}개 제외",
                                results.unknown_dates
                            ));
                        }
                    }
                    if errors > 0 {
                        ui.colored_label(egui::Color32::YELLOW, "일부 누락");
                    }
                    let excluded = self
                        .scan
                        .entries
                        .iter()
                        .filter(|e| e.stale || e.scan.completed().is_none())
                        .count();
                    if self.locations_view.global_mode && excluded > 0 {
                        ui.small(format!("스캔 전/갱신이 필요한 위치 {excluded}개 제외"));
                    }
                    if !self.locations_view.global_mode
                        && self
                            .scan
                            .selected_id()
                            .and_then(|id| self.scan.get(id))
                            .is_some_and(|e| e.stale)
                    {
                        ui.add(egui::Label::new(egui::RichText::new("갱신 필요").small().color(egui::Color32::YELLOW)).truncate())
                            .on_hover_text("갱신 실패로 이 위치는 전체 검색에서 제외됩니다. 이전 완료 결과는 유지됩니다.");
                    }
                    if !self.locations_view.global_mode {
                        if let crate::search_job::SearchStatus::Failed(error) =
                            self.search.job.status()
                        {
                            ui.add(
                                egui::Label::new(
                                    egui::RichText::new(error).color(egui::Color32::LIGHT_RED),
                                )
                                .truncate(),
                            )
                            .on_hover_text(error);
                        }
                    }
                    if let Some(error) = &self.input_error {
                        ui.add(
                            egui::Label::new(
                                egui::RichText::new(error).color(egui::Color32::LIGHT_RED),
                            )
                            .truncate(),
                        );
                    }
                    match self.file_open.status() {
                        OpenStatus::Running => {
                            ui.small("파일 위치나 기본 프로그램을 여는 중…");
                        }
                        OpenStatus::Failed(error) => {
                            ui.colored_label(egui::Color32::LIGHT_RED, error);
                        }
                        OpenStatus::Submitted => {
                            ui.small("기본 프로그램에 열기를 요청했습니다");
                        }
                        OpenStatus::Idle => {}
                    }
                });
            });
        if self.tab != 4 && !self.inline_search.visible {
            egui::Panel::bottom("map")
                .exact_size(panels.map)
                .frame(egui::Frame::new().fill(surface()).inner_margin(
                    if self.map_collapsed || self.show_preview() {
                        4
                    } else {
                        8
                    },
                ))
                .show(ui, |ui| self.map_panel(ui));
        }
        egui::Panel::left("tree")
            .exact_size(panels.left)
            .resizable(false)
            .frame(egui::Frame::new().fill(surface()))
            .show(ui, |ui| self.tree(ui));
        if panels.right == 0.0 && self.details_open && !self.inline_search.visible {
            let mut open = self.details_open;
            egui::Window::new(self.details_title())
                .id(egui::Id::unique("floating_details"))
                .open(&mut open)
                .default_width(240.0)
                .show(ui.ctx(), |ui| {
                    egui::ScrollArea::vertical().show(ui, |ui| self.details(ui));
                });
            self.details_open = open;
        }
        if panels.right > 0.0 {
            egui::Panel::right("details")
                .exact_size(panels.right)
                .resizable(false)
                .frame(egui::Frame::new().fill(surface()))
                .show(ui, |ui| {
                    crate::ui_style::panel_title(ui, self.details_title(), "");
                    egui::ScrollArea::vertical()
                        .id_salt("detail_scroll")
                        .show(ui, |ui| {
                            egui::Frame::new()
                                .inner_margin(egui::Margin::symmetric(16, 14))
                                .show(ui, |ui| {
                                    ui.spacing_mut().item_spacing.y = 7.0;
                                    self.details(ui);
                                });
                        });
                });
        }
        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(bg()))
            .show(ui, |ui| {
                ui.set_max_width(panels.center.max(0.0));
                ui.set_max_height(panels.body.max(0.0));
                self.folder_workspace(ui);
            });
        if self.results.export_open && self.results.export_context.is_none() {
            let context = self.export_context();
            self.results.scope = if self.search.input.is_active() && context.matches.is_some() {
                2
            } else if self.locations_view.global_mode {
                3
            } else {
                0
            };
            self.results.export_context = Some(context);
        }
        let export_context = self.results.export_context.take().unwrap_or_default();
        self.results.windows(ui.ctx(), &export_context);
        if self.results.export_open {
            self.results.export_context = Some(export_context);
        }
        if let Some(paths) = self.file_ui.copy_requested.take() {
            let paths = paths.unwrap_or_else(|| self.copy_selection_paths());
            self.input_error = crate::path_copy::copy_to_clipboard(ui.ctx(), &paths)
                .err()
                .map(|e| e.to_string());
        }
        if let Some((kind, paths)) = self.file_ui.requested.take() {
            self.begin_file_action(kind, paths);
        }
        if let Some((paths, destination)) = self
            .file_ui
            .drop_requested
            .take()
            .or_else(|| self.locations_view.drop_requested.take())
        {
            self.input_error = self
                .prepare_folder_drop(paths, destination)
                .err()
                .map(|e| e.to_string());
        }
        if let Some(kind) = self.locations_view.file_requested.take() {
            self.begin_file_action(kind, None);
        }
        self.navigation_shortcuts(ui.ctx());
        let file_keys_enabled = self.settings_draft.is_none()
            && !self.inline_search.visible
            && !self.file_blocked()
            && !ui.ctx().egui_wants_keyboard_input()
            && !egui::Popup::is_any_open(ui.ctx())
            && self.file_ui.dialog.is_none()
            && self.root_replacement.is_none()
            && !self.scan.is_running();
        if file_keys_enabled
            && ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::F2))
        {
            self.begin_file_action(crate::file_ui::Kind::Rename, None);
        }
        if file_keys_enabled
            && ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Delete))
        {
            self.begin_file_action(crate::file_ui::Kind::Recycle, None);
        }
        self.file_ui.show(ui.ctx(), &mut self.file_job);
        self.confirm_root_replacement(ui.ctx());
        self.settings_window(ui.ctx());
        if self.file_refresh.busy() || !self.file_refresh.errors.is_empty() {
            egui::Window::new("분석 목록 갱신")
                .id(egui::Id::unique("file_refresh_status"))
                .collapsible(false)
                .show(ui.ctx(), |ui| {
                    ui.label(self.file_refresh.label());
                    for error in &self.file_refresh.errors {
                        ui.add(egui::Label::new(error).wrap());
                    }
                    if !self.file_refresh.busy() && ui.button("확인").clicked() {
                        self.file_refresh.errors.clear();
                    }
                    if self.file_refresh.busy() {
                        ui.add_space(8.0);
                        ui.horizontal(|ui| {
                            crate::ui_style::activity_indicator(ui);
                            if ui.button("목록 갱신 취소").clicked() {
                                self.file_refresh.cancel(&mut self.scan);
                            }
                        });
                    }
                });
        }
        self.refresh_derived();
        if ui.input(|i| !i.events.is_empty()) {
            ui.ctx().request_repaint();
        }
        if self.scan.is_running()
            || self.map.running
            || self.statistics.running
            || matches!(
                self.search.job.status(),
                crate::search_job::SearchStatus::Running
            )
            || matches!(self.file_open.status(), OpenStatus::Running)
            || self.results.busy()
            || self.file_job.working()
            || self.file_refresh.busy()
        {
            ui.ctx().request_repaint_after(Duration::from_millis(30));
        }
    }
}

#[cfg(test)]
mod session_tests {
    use super::*;

    fn visible_text(frame: &egui::FullOutput, label: &str) -> Option<egui::Rect> {
        frame.shapes.iter().find_map(|shape| match &shape.shape {
            egui::Shape::Text(text) if text.galley.text() == label => {
                let rect = text.visual_bounding_rect();
                shape.clip_rect.contains_rect(rect).then_some(rect)
            }
            _ => None,
        })
    }

    fn click_at(
        draw: &mut impl FnMut(Vec<egui::Event>) -> egui::FullOutput,
        pos: egui::Pos2,
    ) -> egui::FullOutput {
        let input = |pressed| {
            vec![
                egui::Event::PointerMoved(pos),
                egui::Event::PointerButton {
                    pos,
                    button: egui::PointerButton::Primary,
                    pressed,
                    modifiers: egui::Modifiers::NONE,
                },
            ]
        };
        draw(input(true));
        draw(input(false))
    }

    #[test]
    fn starting_a_child_scan_offers_a_new_root_without_discarding_the_parent() {
        let fixture = crate::duplicates::tests::Fixture::new();
        let child = fixture.0.join("child");
        std::fs::create_dir(&child).unwrap();
        let mut app = DiskWeaveApp::new();
        let finish = |app: &mut DiskWeaveApp| {
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            while app.scan.is_running() {
                app.sync_scan();
                assert!(std::time::Instant::now() < deadline);
                std::thread::yield_now();
            }
        };
        app.scan.start(fixture.0.clone()).unwrap();
        finish(&mut app);
        app.target.choose_folder(child.clone()).unwrap();
        app.begin_scan();
        assert!(app.input_error.is_none(), "{:?}", app.input_error);
        assert_eq!(app.scan.snapshots()[0].scan.folders[0].path, fixture.0);
        assert!(!app.scan.is_running());

        let ctx = egui::Context::default();
        crate::ui_style::apply(&ctx);
        let mut draw = |events| {
            let mut frame = ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(760.0, 500.0),
                    )),
                    events,
                    ..Default::default()
                },
                |ui| app.confirm_root_replacement(ui.ctx()),
            );
            frame.textures_delta.clear();
            frame
        };
        draw(Vec::new());
        let frame = draw(Vec::new());
        let button = visible_text(&frame, "새 최상위로 스캔").unwrap();
        click_at(&mut draw, button.center());
        assert!(app.scan.is_running());
        assert_eq!(app.scan.snapshots()[0].scan.folders[0].path, fixture.0);
        finish(&mut app);
        assert_eq!(app.scan.snapshots().len(), 1);
        assert_eq!(app.source().unwrap().scan.folders[0].path, child);
    }

    #[test]
    fn scan_summary_shows_a_moving_bar_until_the_scan_finishes() {
        let fixture = crate::duplicates::tests::Fixture::new();
        std::fs::write(fixture.0.join("sample.txt"), b"test").unwrap();
        for width in [760.0, 1380.0] {
            let ctx = egui::Context::default();
            crate::ui_style::apply(&ctx);
            let mut app = DiskWeaveApp::new();
            let draw = |app: &DiskWeaveApp, time| {
                let mut frame = ctx.run_ui(
                    egui::RawInput {
                        screen_rect: Some(egui::Rect::from_min_size(
                            egui::Pos2::ZERO,
                            egui::vec2(width, 500.0),
                        )),
                        time: Some(time),
                        ..Default::default()
                    },
                    |ui| app.summary(ui),
                );
                frame.textures_delta.clear();
                frame
            };
            let marker = |frame: &egui::FullOutput| {
                frame.shapes.iter().find_map(|shape| match &shape.shape {
                    egui::Shape::Rect(rect)
                        if rect.fill == crate::ui_style::ACCENT
                            && rect.rect.height() <= 5.0
                            && rect.rect.width() > 20.0 =>
                    {
                        assert!(shape.clip_rect.contains_rect(rect.rect));
                        Some(rect.rect)
                    }
                    _ => None,
                })
            };

            assert!(marker(&draw(&app, 0.0)).is_none());
            app.scan.start(fixture.0.clone()).unwrap();
            let first = draw(&app, 0.25);
            let first_bar = marker(&first).expect("스캔 중 가로 진행 막대");
            assert!(first_bar.bottom() < visible_text(&first, "스캔 중").unwrap().top());
            let second_bar = marker(&draw(&app, 0.75)).expect("계속 진행 중인 스캔");
            assert!(second_bar.left() > first_bar.left());

            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            while app.scan.is_running() {
                app.sync_scan();
                assert!(std::time::Instant::now() < deadline);
                std::thread::yield_now();
            }
            let completed = draw(&app, 1.0);
            assert!(visible_text(&completed, "분석 완료").is_some());
            assert!(marker(&completed).is_none());

            app.scan.start(fixture.0.clone()).unwrap();
            app.scan.cancel();
            let cancelling = draw(&app, 1.25);
            assert!(visible_text(&cancelling, "취소 중").is_some());
            assert!(marker(&cancelling).is_some());
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            while app.scan.is_running() {
                app.sync_scan();
                assert!(std::time::Instant::now() < deadline);
                std::thread::yield_now();
            }
            assert!(marker(&draw(&app, 1.5)).is_none());
        }
    }

    #[test]
    fn target_menu_keeps_location_management_without_a_volume_tracking_action() {
        let mut app = DiskWeaveApp::new();
        let ctx = egui::Context::default();
        crate::ui_style::apply(&ctx);
        let mut draw = |events| {
            let mut frame = ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(1000.0, 600.0),
                    )),
                    events,
                    ..Default::default()
                },
                |ui| app.toolbar(ui),
            );
            frame.textures_delta.clear();
            frame
        };
        let frame = draw(Vec::new());
        let target = visible_text(&frame, "C:").expect("드라이브 선택 메뉴");
        click_at(&mut draw, target.center());
        let frame = draw(Vec::new());
        assert!(visible_text(&frame, "분석 위치 관리").is_some());
        assert!(visible_text(&frame, "결과 · 파일 작업").is_some());
        assert!(visible_text(&frame, "실시간 갱신").is_none());
    }

    #[test]
    fn main_search_scope_opens_direct_search_in_the_list() {
        let ctx = egui::Context::default();
        crate::ui_style::apply(&ctx);
        let mut app = DiskWeaveApp::new();
        let mut draw = |events| {
            let mut frame = ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(760.0, 500.0),
                    )),
                    events,
                    ..Default::default()
                },
                |ui| app.center(ui),
            );
            frame.textures_delta.clear();
            frame
        };
        let frame = draw(Vec::new());
        let scope = visible_text(&frame, "현재 폴더 안에서").unwrap();
        click_at(&mut draw, scope.center());
        let frame = draw(Vec::new());
        for label in [
            "현재 폴더 안에서",
            "현재 스캔 위치 전체",
            "스캔한 위치 모두",
            "다른 폴더 지정",
            "내 PC 전체",
        ] {
            assert!(visible_text(&frame, label).is_some(), "{label}");
        }
        let choice = visible_text(&frame, "내 PC 전체").expect("메인 검색의 직접 검색 범위");
        click_at(&mut draw, choice.center());
        let frame = draw(Vec::new());
        assert!(visible_text(&frame, "검색 닫기").is_some());
        assert!(!app.quick.visible);
    }

    #[test]
    fn settings_are_at_top_right_at_supported_scales() {
        for (width, scale) in [(760.0, 1.0), (760.0, 2.0), (1380.0, 1.5)] {
            let ctx = egui::Context::default();
            crate::ui_style::apply(&ctx);
            let mut app = DiskWeaveApp::new();
            let mut raw = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(width, 500.0),
                )),
                ..Default::default()
            };
            raw.viewports
                .get_mut(&egui::ViewportId::ROOT)
                .unwrap()
                .native_pixels_per_point = Some(scale);
            let mut draw = |events| {
                let mut raw = raw.clone();
                raw.events = events;
                let mut frame = ctx.run_ui(raw, |ui| {
                    egui::Panel::top("brand").exact_size(36.0).show(ui, |ui| {
                        app.header(ui, width >= 1050.0);
                    });
                });
                frame.textures_delta.clear();
                frame
            };
            let frame = draw(Vec::new());
            let button = visible_text(&frame, "설정").expect("제목줄 설정");
            assert!(
                button.left() > width - 220.0 && button.center().y < 36.0,
                "{button:?}, 창 너비 {width}, 배율 {scale}"
            );
            assert!(visible_text(&frame, "빠른 검색 · 별도 창").is_none());
        }
    }

    #[test]
    fn main_settings_presents_editable_controls_and_restores_folder_refresh() {
        let fixture = crate::duplicates::tests::Fixture::new();
        let path = fixture.0.join("preferences.json");
        let ctx = egui::Context::default();
        crate::ui_style::apply(&ctx);
        let mut app = DiskWeaveApp::new();
        app.settings_path = Some(path.clone());
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(760.0, 500.0),
            )),
            ..Default::default()
        };
        let frame_for = |app: &mut DiskWeaveApp, events| {
            let mut frame = ctx.run_ui(
                egui::RawInput {
                    events,
                    ..raw.clone()
                },
                |ui| {
                    egui::Panel::top("test_header")
                        .exact_size(36.0)
                        .show(ui, |ui| app.header(ui, false));
                    app.settings_window(&ctx);
                },
            );
            frame.textures_delta.clear();
            frame
        };
        let mut draw = |events| frame_for(&mut app, events);
        let settings = visible_text(&draw(Vec::new()), "설정").expect("제목줄 설정");
        click_at(&mut draw, settings.center());
        let frame = draw(Vec::new());
        for label in [
            "스캔 및 갱신",
            "검색 및 단축키",
            "다른 앱에서도 검색 열기",
            "창을 닫아도 백그라운드에서 실행",
        ] {
            assert!(visible_text(&frame, label).is_some(), "{label}");
        }
        assert!(visible_text(&frame, "일반·표시").is_none());
        let toggle = visible_text(&frame, "보고 있는 폴더 자동 갱신").unwrap();
        click_at(&mut draw, toggle.center());
        let close = visible_text(&draw(Vec::new()), "닫기").expect("설정 닫기");
        click_at(&mut draw, close.center());
        assert!(app.settings_draft.is_none());
        assert!(
            !crate::app_settings::load(&path)
                .unwrap()
                .ui
                .folder_auto_refresh
        );

        let mut restored = DiskWeaveApp::new();
        restored.apply_ui_settings(crate::app_settings::load(&path).unwrap().ui);
        assert!(restored.folder_refresh_disabled);
        app.quick.open();
        app.apply_search_action(
            &ctx,
            SearchSurface::Quick,
            crate::quick_ui::Action::Settings,
        );
        assert!(app.settings_draft.is_some());
        assert!(!app.quick.visible);
    }

    #[test]
    fn direct_search_discards_stale_rows_and_preserves_the_analysis_on_close() {
        let fixture = crate::duplicates::tests::Fixture::new();
        let external = crate::duplicates::tests::Fixture::new();
        let entry = external.0.join("outside-needle.txt");
        std::fs::write(&entry, [1]).unwrap();
        let mut app = DiskWeaveApp::new();
        app.scan.start(fixture.0.clone()).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while app.scan.is_running() {
            app.sync_scan();
            assert!(std::time::Instant::now() < deadline);
            std::thread::yield_now();
        }
        let old = app.capture_navigation().unwrap();

        use crate::quick_search::Scope;
        app.search.input.text = "needle".into();
        app.choose_main_search_scope(ScopeChoice::Direct(Scope::Folder));
        app.inline_search.set_folder(external.0.clone());
        assert_eq!(app.inline_search.text, "needle");
        assert_eq!(app.inline_search.count(), 0);
        assert!(!app.quick.visible && app.quick.text.is_empty());

        let ctx = egui::Context::default();
        crate::ui_style::apply(&ctx);
        let mut draw = |events| {
            let mut frame = ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(520.0, 350.0),
                    )),
                    events,
                    ..Default::default()
                },
                |ui| app.inline_search_panel(ui),
            );
            frame.textures_delta.clear();
            frame
        };
        let frame = draw(Vec::new());
        assert!(visible_text(&frame, "검색").is_some());
        assert!(visible_text(&frame, "검색 닫기").is_some());
        draw(vec![egui::Event::Key {
            key: egui::Key::Enter,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        }]);
        assert!(app.inline_search.live.input.is_some());
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while app.inline_search.pending() {
            app.inline_search.tick(&app.scan, std::time::Instant::now());
            assert!(std::time::Instant::now() < deadline);
            std::thread::yield_now();
        }
        assert_eq!(app.inline_search.row(0).unwrap().path, entry);
        app.inline_search.start_live().unwrap();
        app.choose_main_search_scope(ScopeChoice::Direct(Scope::Computer));
        assert_eq!(app.inline_search.count(), 0);
        app.inline_search_ui.close_inline(&mut app.inline_search);
        while app.inline_search.live.running() {
            app.inline_search.tick(&app.scan, std::time::Instant::now());
            assert!(std::time::Instant::now() < deadline);
            std::thread::yield_now();
        }
        assert!(!app.inline_search.visible);
        assert_eq!(app.inline_search.count(), 0);
        let restored = app.capture_navigation().unwrap();
        assert_eq!(restored.folder, old.folder);
        assert_eq!(restored.selected, old.selected);
        assert_eq!(app.scan.snapshots().len(), 1);
    }

    #[test]
    fn compact_list_keeps_navigation_scope_and_important_columns_visible() {
        let fixture = crate::duplicates::tests::Fixture::new();
        let child = fixture.0.join("child");
        std::fs::create_dir(&child).unwrap();
        std::fs::write(child.join("sample.txt"), b"test").unwrap();
        let mut app = DiskWeaveApp::new();
        app.folder_refresh_disabled = true;
        assert_eq!(app.details_title(), "상세 정보");
        app.scan.start(fixture.0.clone()).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while app.scan.is_running() {
            app.sync_scan();
            assert!(std::time::Instant::now() < deadline);
            std::thread::yield_now();
        }
        let source = app.source().unwrap();
        app.navigate(
            source
                .scan
                .folder_index(std::path::Path::new("child"))
                .unwrap(),
            true,
        );
        app.refresh_derived();
        assert_eq!(app.details_title(), "현재 폴더");
        app.detail_target = Some((source.id, source.generation, Item::File(0)));
        assert_eq!(app.details_title(), "선택 항목");
        app.detail_target = None;

        let ctx = egui::Context::default();
        crate::ui_style::apply(&ctx);
        let panels = crate::workspace::Panels::new(760.0, 500.0, 192.0, false);
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(panels.center, panels.body),
            )),
            ..Default::default()
        };
        for scope in [
            crate::search_view::SearchScope::Folder,
            crate::search_view::SearchScope::Location,
            crate::search_view::SearchScope::Registered,
        ] {
            app.locations_view.choose_scope(&mut app.search, scope);
            let mut frame = ctx.run_ui(raw.clone(), |ui| app.center(ui));
            frame.textures_delta.clear();
            let search = visible_text(&frame, "파일·폴더 이름 검색").expect("목록 검색 입력");
            let scope_button = visible_text(&frame, scope.label()).expect("현재 검색 범위");
            let tabs = visible_text(&frame, "폴더·파일").expect("목록 탭");
            assert!(tabs.bottom() < search.top());
            assert!(search.right() < scope_button.left());
            assert!(visible_text(&frame, &child.display().to_string()).is_some());
            if scope != crate::search_view::SearchScope::Registered {
                let date = visible_text(&frame, "수정 날짜").expect("창 안의 수정 날짜 열");
                let ratio = visible_text(&frame, "부모 대비").expect("창 안의 비중 열");
                assert!(ratio.right() < date.left());
                assert!(search.bottom() < date.top());
            }
        }
    }

    #[test]
    fn folder_tabs_open_only_on_request_and_restore_the_original_view() {
        let fixture = crate::duplicates::tests::Fixture::new();
        std::fs::create_dir_all(fixture.0.join("first/deep")).unwrap();
        std::fs::create_dir(fixture.0.join("second")).unwrap();
        let mut app = DiskWeaveApp::new();
        app.scan.start(fixture.0.clone()).unwrap();
        let limit = std::time::Instant::now() + Duration::from_secs(5);
        while app.scan.is_running() {
            app.sync_scan();
            assert!(std::time::Instant::now() < limit);
            std::thread::yield_now();
        }
        let source = app.source().unwrap();
        let first = source
            .scan
            .folder_index(std::path::Path::new("first/deep"))
            .unwrap();
        let second = source
            .scan
            .folder_index(std::path::Path::new("second"))
            .unwrap();
        app.navigate(first, true);
        assert_eq!(app.folder_tabs.len(), 1);
        assert!(!app.folder_tabs.is_split());
        app.search.input.text = "saved query".into();
        app.search.vertical_offset = 36.0;
        app.open_folder_tab(&source, second, false).unwrap();
        assert_eq!(app.folder_tabs.len(), 2);
        assert_eq!(
            app.capture_navigation().unwrap().folder,
            fixture.0.join("second")
        );
        assert!(Arc::ptr_eq(&source.scan, &app.source().unwrap().scan));
        app.switch_folder_tab(0);
        let original = app.capture_navigation().unwrap();
        assert_eq!(original.folder, fixture.0.join("first/deep"));
        assert_eq!(original.query.text, "saved query");
        assert_eq!(original.scroll[1], 36.0);
        app.history_move(false);
        assert_eq!(app.capture_navigation().unwrap().folder, fixture.0);
        app.close_folder_tab(1);
        assert_eq!(app.folder_tabs.len(), 1);
    }

    #[test]
    fn split_keeps_left_view_and_updates_both_sides_after_copy_and_rescan() {
        let fixture = crate::duplicates::tests::Fixture::new();
        let left = fixture.0.join("left");
        let right = fixture.0.join("right");
        let input = left.join("report.txt");
        std::fs::create_dir(&left).unwrap();
        std::fs::create_dir_all(right.join("deep")).unwrap();
        std::fs::write(&input, b"report").unwrap();
        let mut app = DiskWeaveApp::new();
        app.scan.start(fixture.0.clone()).unwrap();
        let until = std::time::Instant::now() + Duration::from_secs(5);
        while app.scan.is_running() {
            app.sync_scan();
            assert!(std::time::Instant::now() < until);
            std::thread::yield_now();
        }
        let source = app.source().unwrap();
        let folder = |name: &str| {
            source
                .scan
                .folder_index(std::path::Path::new(name))
                .unwrap()
        };
        app.navigate(folder("left"), true);
        app.open_folder_tab(&source, folder("right"), true).unwrap();
        assert_eq!(app.folder_tabs.len(), 1);
        assert_eq!(app.folder_tabs.side(), Side::Right);
        assert_eq!(app.folder_tabs.other().unwrap().folder, left);
        app.focus_folder_side(Side::Left);
        let target = app.other_folder_path().unwrap();
        assert_eq!(target, right);
        let ctx = egui::Context::default();
        crate::ui_style::apply(&ctx);
        for (pressed, pos) in [
            (true, egui::pos2(180.0, 160.0)),
            (false, egui::pos2(850.0, 340.0)),
        ] {
            let raw = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(1100.0, 600.0),
                )),
                events: vec![
                    egui::Event::PointerMoved(pos),
                    egui::Event::PointerButton {
                        pos,
                        button: egui::PointerButton::Primary,
                        pressed,
                        modifiers: egui::Modifiers::NONE,
                    },
                ],
                ..egui::RawInput::default()
            };
            let mut frame = ctx.run_ui(raw, |ui| {
                if pressed {
                    egui::DragAndDrop::set_payload(
                        ui.ctx(),
                        crate::file_ui::DragPayload {
                            paths: vec![input.clone()],
                        },
                    );
                }
                app.folder_workspace(ui);
            });
            frame.textures_delta.clear();
        }
        let (dropped, destination) = app
            .file_ui
            .drop_requested
            .take()
            .expect("반대편 폴더에서 드롭 수신");
        app.prepare_folder_drop(dropped, destination).unwrap();
        let request = app.file_ui.request().unwrap();
        assert_eq!(request.paths, [input]);
        assert!(
            matches!(request.action, crate::file_ops::Action::Copy { destination } if destination == right)
        );
        assert!(!right.join("report.txt").exists());
        app.file_ui.dialog = None;
        app.prepare_folder_drop(vec![left.join("report.txt")], target)
            .unwrap();
        app.file_job
            .prepare(app.file_ui.request().unwrap(), None)
            .unwrap();
        app.file_ui.dialog = None;
        while app.file_job.working() {
            app.file_tick();
            assert!(std::time::Instant::now() < until);
            std::thread::yield_now();
        }
        app.file_job.confirm().unwrap();
        while app.file_job.working() || app.file_refresh.busy() {
            app.file_tick();
            assert!(std::time::Instant::now() < until);
            std::thread::yield_now();
        }
        assert_eq!(std::fs::read(right.join("report.txt")).unwrap(), b"report");
        assert_eq!(app.capture_navigation().unwrap().folder, left);
        app.focus_folder_side(Side::Right);
        assert_eq!(app.capture_navigation().unwrap().folder, right);
        let latest = app.source().unwrap();
        assert!(!Arc::ptr_eq(&source.scan, &latest.scan));
        assert!(latest
            .scan
            .file_named(
                latest
                    .scan
                    .folder_index(std::path::Path::new("right"))
                    .unwrap(),
                std::ffi::OsStr::new("report.txt")
            )
            .is_some());
        let deep = latest
            .scan
            .folder_index(std::path::Path::new("right/deep"))
            .unwrap();
        app.navigate(deep, true);
        std::fs::remove_dir(right.join("deep")).unwrap();
        app.scan.start_selected(&[source.id]).unwrap();
        while app.scan.is_running() {
            app.sync_scan();
            assert!(std::time::Instant::now() < until);
            std::thread::yield_now();
        }
        assert_eq!(app.capture_navigation().unwrap().folder, right);
        app.close_folder_split();
        assert_eq!(app.capture_navigation().unwrap().folder, left);
        assert!(!app.folder_tabs.is_split());
    }

    #[test]
    fn removing_a_tab_location_does_not_show_another_location_under_its_name() {
        let fixture = crate::duplicates::tests::Fixture::new();
        let first = fixture.0.join("first");
        let second = fixture.0.join("second");
        std::fs::create_dir(&first).unwrap();
        std::fs::create_dir(&second).unwrap();
        let mut app = DiskWeaveApp::new();
        let a = app.scan.add(&first).unwrap();
        let b = app.scan.add(&second).unwrap();
        app.scan.start_selected(&[a, b]).unwrap();
        let limit = std::time::Instant::now() + Duration::from_secs(5);
        while app.scan.is_running() {
            app.sync_scan();
            assert!(std::time::Instant::now() < limit);
            std::thread::yield_now();
        }
        let source = app.source().unwrap();
        app.open_folder_tab(&source, 0, false).unwrap();
        app.select_registered_location(b);
        app.switch_folder_tab(0);
        app.remove_registered_location(a).unwrap();
        assert!(app.source().is_none());
        assert_eq!(app.capture_navigation().unwrap().folder, first);
        assert!(app.input_error.as_deref().unwrap().contains("제거"));
        app.switch_folder_tab(1);
        assert_eq!(app.capture_navigation().unwrap().folder, second);
        app.switch_folder_tab(0);
        assert!(app.source().is_none());
        app.close_folder_tab(0);
        assert_eq!(app.capture_navigation().unwrap().folder, second);
    }

    #[test]
    fn quick_recycle_policy_uses_the_hit_location_and_keeps_registered_roots_protected() {
        let registered = crate::duplicates::tests::Fixture::new();
        let external = crate::duplicates::tests::Fixture::new();
        let in_file = registered.0.join("registered.txt");
        let out_file = external.0.join("external.txt");
        std::fs::write(&in_file, []).unwrap();
        std::fs::write(&out_file, []).unwrap();
        let mut app = DiskWeaveApp::new();
        let id = app.scan.add(&registered.0).unwrap();
        app.tab = 4;

        assert!(app.quick_recycle_context(in_file.clone()).is_err());
        let context = app.quick_recycle_context(out_file).unwrap();
        assert_eq!(context.protected, std::slice::from_ref(&registered.0));
        assert!(context.writable);

        app.scan.start_selected(&[id]).unwrap();
        assert!(app.quick_recycle_context(in_file.clone()).is_err());
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while app.scan.is_running() {
            app.sync_scan();
            assert!(std::time::Instant::now() < deadline);
            std::thread::yield_now();
        }
        assert!(app.quick_recycle_context(in_file.clone()).is_ok());
        app.scan.entries[0].stale = true;
        assert!(app.quick_recycle_context(in_file).is_err());
    }

    #[test]
    fn explicit_file_actions_use_their_own_paths_and_keep_stale_locations_read_only() {
        let fixture = crate::duplicates::tests::Fixture::new();
        let marked = fixture.0.join("선택한 파일.txt");
        let dropped = fixture.0.join("내려놓을 파일.txt");
        std::fs::write(&marked, b"selected").unwrap();
        std::fs::write(&dropped, b"dropped").unwrap();
        let mut app = DiskWeaveApp::new();
        let id = app.scan.add(&fixture.0).unwrap();
        app.scan.start_selected(&[id]).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while app.scan.is_running() {
            app.sync_scan();
            assert!(std::time::Instant::now() < deadline);
            std::thread::yield_now();
        }
        let source = app.source().unwrap();
        let marked_id = source
            .scan
            .files
            .iter()
            .position(|file| file.name == "선택한 파일.txt")
            .unwrap();
        app.detail_target = Some((source.id, source.generation, Item::File(marked_id)));
        assert_eq!(app.copy_selection_paths(), [marked]);

        let context = app.file_context(Some(vec![dropped.clone()]));
        assert!(context.writable);
        assert_eq!(context.paths, std::slice::from_ref(&dropped));
        assert_eq!(context.protected, std::slice::from_ref(&fixture.0));
        app.scan.entries[0].stale = true;
        let context = app.file_context(Some(vec![dropped]));
        assert!(!context.writable);
        assert!(app
            .file_ui
            .begin(crate::file_ui::Kind::Copy, context)
            .is_err());
    }

    #[test]
    fn search_match_statistics_and_whole_folder_map_keep_distinct_totals() {
        let fixture = crate::duplicates::tests::Fixture::new();
        std::fs::write(fixture.0.join("match.bin"), [0; 3]).unwrap();
        std::fs::write(fixture.0.join("other.bin"), [0; 9]).unwrap();
        let mut app = DiskWeaveApp::new();
        app.scan.start(fixture.0.clone()).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while app.scan.is_running() {
            app.sync_scan();
            assert!(std::time::Instant::now() < deadline);
            std::thread::yield_now();
        }
        app.search.input.text = "match".into();
        app.statistics.matched = true;
        while app.statistics.output.is_none() || app.map.output.is_none() {
            app.refresh_derived();
            assert!(std::time::Instant::now() < deadline);
            std::thread::yield_now();
        }
        assert_eq!(app.statistics.output.as_ref().unwrap().stats.bytes, 3);
        assert_eq!(app.map.output.as_ref().unwrap().data.total, 12);
    }

    #[test]
    fn unrelated_live_edit_preserves_completed_local_search_statistics_and_map() {
        let fixture = crate::duplicates::tests::Fixture::new();
        std::fs::write(fixture.0.join("sample.bin"), [1, 2, 3]).unwrap();
        let mut app = DiskWeaveApp::new();
        let id = app.scan.add(&fixture.0).unwrap();
        app.scan.start_selected(&[id]).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while app.scan.is_running() {
            app.sync_scan();
            assert!(std::time::Instant::now() < deadline);
            std::thread::yield_now();
        }
        app.search.input.text = "sample".into();
        while app.search.job.results().is_none()
            || app.statistics.output.is_none()
            || app.map.output.is_none()
        {
            app.refresh_derived();
            assert!(std::time::Instant::now() < deadline);
            std::thread::yield_now();
        }
        let old = app.source().unwrap();
        let revision = app.map.revision;
        let mut next = (*old.scan).clone();
        next.partial = Some(Arc::new(crate::scanner::PartialUpdate {
            previous: Arc::downgrade(&old.scan),
            affected: Default::default(),
            search: None,
        }));
        let next = Arc::new(next);
        app.replace_live_result(Some(&old.scan), &next, None);
        let current = Snapshot {
            generation: old.generation + 1,
            scan: next,
            ..old
        };
        app.reuse_local_panels(&current);
        assert!(app.search.job.results().is_some());
        assert_eq!(app.map.revision, revision);
        assert_eq!(
            app.map.output.as_ref().unwrap().sources[0].generation,
            current.generation
        );
        assert_eq!(
            app.statistics.output.as_ref().unwrap().sources[0].generation,
            current.generation
        );
        let mut resized = (*current.scan).clone();
        resized.files[0].logical_bytes = 9;
        resized.folders[0].logical_bytes = 9;
        resized.partial = Some(Arc::new(crate::scanner::PartialUpdate {
            previous: Arc::downgrade(&current.scan),
            affected: [0].into(),
            search: Some(crate::scanner::SearchDelta {
                before: vec![Item::File(0)],
                after: vec![Item::File(0)],
                file_rows: None,
                folder_rows: None,
            }),
        }));
        let resized = Snapshot {
            generation: current.generation + 1,
            scan: Arc::new(resized),
            ..current
        };
        app.reuse_local_panels(&resized);
        let stats = app.statistics.output.as_ref().unwrap();
        let map = app.map.output.as_ref().unwrap();
        assert_eq!(stats.stats.bytes, 9);
        assert_eq!(stats.sources[0].generation, resized.generation);
        assert_eq!(map.data.total, 9);
        assert_eq!(map.sources[0].generation, resized.generation);
    }

    #[test]
    fn navigation_api_restores_search_selection_and_scroll_without_persisting_paths() {
        let fixture = crate::duplicates::tests::Fixture::new();
        std::fs::create_dir_all(fixture.0.join("nested/deep")).unwrap();
        std::fs::create_dir(fixture.0.join("other")).unwrap();
        std::fs::write(fixture.0.join("nested/deep/find-me"), b"data").unwrap();
        let mut app = DiskWeaveApp::new();
        app.scan.start(fixture.0.clone()).unwrap();
        let ctx = egui::Context::default();
        let limit = std::time::Instant::now() + Duration::from_secs(5);
        while app.scan.is_running() {
            app.tick_background(&ctx);
            assert!(std::time::Instant::now() < limit);
            std::thread::yield_now();
        }
        let source = app.source().unwrap();
        app.search.input.text = "find-me".into();
        app.search.whole_scan = true;
        app.tab = 3;
        app.search.refresh(&source.scan, 0);
        while app.search.job.results().is_none() {
            app.search.poll();
            assert!(std::time::Instant::now() < limit);
            std::thread::yield_now();
        }
        app.search.selection.click(0, false, false);
        app.search.horizontal_offset = 12.0;
        app.search.vertical_offset = 44.0;
        app.analyze_at(source.id, source.generation, Item::File(0));
        assert_eq!(
            app.capture_navigation().unwrap().folder,
            fixture.0.join("nested/deep")
        );
        app.history_move(false);
        while app.search.job.results().is_none() {
            app.search.poll();
            assert!(std::time::Instant::now() < limit);
            std::thread::yield_now();
        }
        let restored = app.capture_navigation().unwrap();
        assert_eq!(restored.query.text, "find-me");
        assert_eq!(restored.scroll, [12.0, 44.0]);
        assert_eq!(restored.selected.len(), 1);
        app.history_move(true);
        assert_eq!(
            app.capture_navigation().unwrap().folder,
            fixture.0.join("nested/deep")
        );
        app.history_move(false);
        let other = source
            .scan
            .folder_index(std::path::Path::new("other"))
            .unwrap();
        app.navigate(other, true);
        app.history_move(true);
        assert_eq!(
            app.capture_navigation().unwrap().folder,
            fixture.0.join("other")
        );
        let serialized = serde_json::to_string(&app.ui_preferences()).unwrap();
        assert!(!serialized.contains("find-me"));
        assert!(!serialized.contains(&fixture.0.display().to_string()));
    }
    #[test]
    fn selecting_an_offline_location_preserves_global_search_in_history() {
        let fixture = crate::duplicates::tests::Fixture::new();
        let first = fixture.0.join("first");
        let offline = fixture.0.join("offline");
        std::fs::create_dir(&first).unwrap();
        std::fs::create_dir(&offline).unwrap();
        let hit = first.join("자료.bin");
        std::fs::write(&hit, b"data").unwrap();

        let mut app = DiskWeaveApp::new();
        let first_id = app.scan.add(&first).unwrap();
        let offline_id = app.scan.add(&offline).unwrap();
        app.scan.start_selected(&[first_id, offline_id]).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while app.scan.is_running() {
            app.sync_scan();
            assert!(std::time::Instant::now() < deadline);
            std::thread::yield_now();
        }
        std::fs::remove_dir(&offline).unwrap();

        app.search.input.text = "자료".into();
        app.locations_view.set_global_mode(true);
        app.locations_view.refresh(&app.scan, &app.search.input);
        while app.locations_view.results().is_none() {
            app.locations_view.refresh(&app.scan, &app.search.input);
            assert!(std::time::Instant::now() < deadline);
            std::thread::yield_now();
        }
        app.locations_view.select_result(0, false, false);
        app.search.vertical_offset = 27.0;
        app.select_registered_location(offline_id);
        assert_eq!(app.target.path(), Some(offline.as_path()));
        assert_eq!(app.scan.selected_id(), Some(offline_id));
        assert!(!app.locations_view.global_mode);

        app.history_move(false);
        assert_eq!(app.scan.selected_id(), Some(first_id));
        assert!(app.locations_view.global_mode);
        assert_eq!(app.search.input.text, "자료");
        assert_eq!(app.search.vertical_offset, 27.0);
        while app.locations_view.selected_targets().is_empty() {
            app.locations_view.refresh(&app.scan, &app.search.input);
            assert!(std::time::Instant::now() < deadline);
            std::thread::yield_now();
        }
        assert_eq!(app.copy_selection_paths(), vec![hit]);
        app.history_move(true);
        assert_eq!(app.scan.selected_id(), Some(offline_id));
        assert!(!app.locations_view.global_mode);
    }

    #[test]
    fn a_new_app_starts_an_empty_general_session_and_does_not_restore_previous_results() {
        let app = DiskWeaveApp::new();
        assert!(app.active_locations().entries.is_empty());
        assert!(app.active_locations().snapshots().is_empty());
        assert!(!app.active_locations().is_running());
        assert_eq!(app.selected_path(), Some(std::path::Path::new("C:\\")));
        drop(app);
        let reopened = DiskWeaveApp::new();
        assert!(reopened.active_locations().entries.is_empty());
    }

    #[test]
    fn viewport_frames_record_physical_position_and_keep_normal_bounds_when_maximized() {
        let fixture = crate::duplicates::tests::Fixture::new();
        let path = fixture.0.join("preferences.json");
        let mut app = DiskWeaveApp::new();
        app.settings_path = Some(path.clone());
        let ctx = egui::Context::default();
        let mut raw = egui::RawInput::default();
        let viewport = raw.viewports.get_mut(&egui::ViewportId::ROOT).unwrap();
        viewport.native_pixels_per_point = Some(1.5);
        viewport.outer_rect = Some(egui::Rect::from_min_size(
            egui::pos2(20.0, 30.0),
            egui::vec2(900.0, 600.0),
        ));
        viewport.inner_rect = viewport.outer_rect;
        let mut output = ctx.run_ui(raw.clone(), |ui| app.settings_frame(ui.ctx()));
        output.textures_delta.clear();
        let normal = app.window_state.clone().unwrap();
        assert_eq!(normal.position, [30.0, 45.0]);
        assert_eq!(normal.size, [900.0, 600.0]);
        raw.viewports
            .get_mut(&egui::ViewportId::ROOT)
            .unwrap()
            .maximized = Some(true);
        let mut output = ctx.run_ui(raw, |ui| app.settings_frame(ui.ctx()));
        output.textures_delta.clear();
        assert_eq!(app.window_state.as_ref().unwrap().position, normal.position);
        assert!(app.window_state.as_ref().unwrap().maximized);
        app.save_ui_settings();
        assert_eq!(
            crate::app_settings::load(&path).unwrap().ui.window,
            app.window_state
        );
        assert!(app.scan.entries.is_empty());
    }

    #[test]
    fn scan_status_shows_counts_and_worker_limit_until_cancelled() {
        let fixture = crate::duplicates::tests::Fixture::new();
        let mut app = DiskWeaveApp::new();
        app.scan
            .configure_parallelism(crate::scanner::Parallelism::Two)
            .unwrap();
        app.scan.start(fixture.0.clone()).unwrap();
        let message = app.status();
        assert!(message.starts_with("파일·폴더 확인 중 · 파일 "));
        assert!(message.contains(" · 폴더 "));
        assert!(message.ends_with("초 · 동시 작업 2개"));
        app.scan.cancel();
        assert!(app.status().starts_with("취소 중 · "));

        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while app.scan.is_running() {
            app.sync_scan();
            assert!(std::time::Instant::now() < deadline);
            std::thread::yield_now();
        }
    }

    #[test]
    fn keyboard_dispatch_keeps_approved_keys_and_preserves_button_navigation() {
        fn press(
            app: &mut DiskWeaveApp,
            ctx: &egui::Context,
            key: egui::Key,
            modifiers: egui::Modifiers,
        ) {
            let mut raw = egui::RawInput::default();
            raw.events.push(egui::Event::Key {
                key,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers,
            });
            let mut output = ctx.run_ui(raw, |ui| {
                let mut text = String::new();
                ui.add(egui::TextEdit::singleline(&mut text).id(egui::Id::unique("scan_path")));
                ui.add(egui::TextEdit::singleline(&mut text).id(egui::Id::unique("search_query")));
                app.navigation_shortcuts(ui.ctx());
            });
            output.textures_delta.clear();
        }
        let fixture = crate::duplicates::tests::Fixture::new();
        std::fs::create_dir(fixture.0.join("child")).unwrap();
        let mut app = DiskWeaveApp::new();
        let ctx = egui::Context::default();
        let until = std::time::Instant::now() + Duration::from_secs(5);
        app.scan.start(fixture.0.clone()).unwrap();
        while app.scan.is_running() {
            app.sync_scan();
            assert!(std::time::Instant::now() < until);
            std::thread::yield_now();
        }
        app.sync_scan();
        let source = app.source().unwrap();
        let child = source
            .scan
            .folder_index(std::path::Path::new("child"))
            .unwrap();
        app.navigate(child, true);
        press(&mut app, &ctx, egui::Key::ArrowUp, egui::Modifiers::ALT);
        assert_eq!(app.explorer.as_ref().unwrap().folder(), child);
        press(&mut app, &ctx, egui::Key::ArrowLeft, egui::Modifiers::ALT);
        assert_eq!(app.explorer.as_ref().unwrap().folder(), child);
        press(&mut app, &ctx, egui::Key::ArrowRight, egui::Modifiers::ALT);
        assert_eq!(app.explorer.as_ref().unwrap().folder(), child);
        let selected = app.capture_navigation().unwrap().selected;
        for key in [egui::Key::ArrowUp, egui::Key::ArrowDown] {
            press(&mut app, &ctx, key, egui::Modifiers::NONE);
            assert_eq!(app.capture_navigation().unwrap().selected, selected);
        }
        app.history_move(false);
        assert_eq!(app.explorer.as_ref().unwrap().folder(), 0);
        app.history_move(true);
        assert_eq!(app.explorer.as_ref().unwrap().folder(), child);
        app.navigate(0, true);
        assert_eq!(app.explorer.as_ref().unwrap().folder(), 0);
        while matches!(
            app.search.job.status(),
            crate::search_job::SearchStatus::Running
        ) {
            app.search.poll();
            assert!(std::time::Instant::now() < until);
            std::thread::yield_now();
        }
        let row = app
            .search
            .job
            .results()
            .unwrap()
            .items
            .iter()
            .position(|&item| item == Item::Folder(child))
            .unwrap();
        app.search.selection.click(row, false, false);
        press(&mut app, &ctx, egui::Key::Enter, egui::Modifiers::NONE);
        assert_eq!(app.explorer.as_ref().unwrap().folder(), child);
        press(&mut app, &ctx, egui::Key::L, egui::Modifiers::CTRL);
        assert_eq!(
            ctx.memory(|m| m.focused()),
            Some(egui::Id::unique("scan_path"))
        );
        press(&mut app, &ctx, egui::Key::F5, egui::Modifiers::NONE);
        assert!(!app.scan.is_running());
        press(&mut app, &ctx, egui::Key::F, egui::Modifiers::CTRL);
        assert_eq!(
            ctx.memory(|m| m.focused()),
            Some(egui::Id::unique("search_query"))
        );
        ctx.memory_mut(|m| m.surrender_focus(egui::Id::unique("search_query")));
        press(&mut app, &ctx, egui::Key::F5, egui::Modifiers::NONE);
        assert!(app.scan.is_running());
        assert_eq!(app.target.path(), Some(fixture.0.as_path()));
        while app.scan.is_running() {
            app.sync_scan();
            assert!(std::time::Instant::now() < until);
            std::thread::yield_now();
        }
    }

    #[test]
    fn quick_search_reports_shell_failure_and_routes_analysis_to_its_source() {
        let root =
            std::env::temp_dir().join(format!("diskweave-quick-routing-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("sample.bin");
        std::fs::write(&path, [1, 2, 3]).unwrap();
        let mut app = DiskWeaveApp::new();
        let id = app.scan.add(&root).unwrap();
        app.scan.start_selected(&[id]).unwrap();
        let ctx = egui::Context::default();
        let until = std::time::Instant::now() + Duration::from_secs(5);
        while app.scan.is_running() {
            app.tick_background(&ctx);
            assert!(std::time::Instant::now() < until);
            std::thread::yield_now();
        }
        let source = app.scan.snapshots().pop().unwrap();
        app.analyze_at(id, source.generation, Item::File(0));
        assert_eq!(app.scan.selected_id(), Some(id));
        assert_eq!(
            app.detail_target,
            Some((id, source.generation, Item::File(0)))
        );
        let request = OpenRequest::for_item(&source.scan, Item::File(0), OpenAction::OpenFile);
        app.file_open
            .start_with(request, move |r| {
                assert_eq!(r.path(), path);
                Err(std::io::Error::other("시험 Shell 실패"))
            })
            .unwrap();
        app.quick_ui.opening = true;
        while matches!(app.file_open.status(), OpenStatus::Running) {
            app.tick_background(&ctx);
            assert!(std::time::Instant::now() < until);
            std::thread::yield_now();
        }
        assert!(app.quick.visible);
        assert!(app.quick_ui.error.is_some());
        assert!(!app.quick_ui.opening);
        drop(app);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn confirmed_quick_folder_scan_opens_the_searched_file_and_preserves_the_prior_analysis() {
        let fixture = crate::duplicates::tests::Fixture::new();
        let first = fixture.0.join("first");
        let other = fixture.0.join("other");
        std::fs::create_dir(&first).unwrap();
        std::fs::create_dir(&other).unwrap();
        let file = other.join("backup.bin");
        std::fs::write(&file, b"test").unwrap();
        let hit = crate::quick_search::Hit {
            path: file.clone(),
            kind: crate::file_open::TargetKind::File,
            bytes: Some(4),
            identity: crate::metadata::read(&file).values.identity.value(),
        };
        let mut app = DiskWeaveApp::new();
        let ctx = egui::Context::default();
        app.scan.start(first.clone()).unwrap();
        finish_quick_test_scan(&mut app, &ctx);
        let previous = app.source().unwrap();

        app.analyze_quick_path(&ctx, SearchSurface::Quick, hit.clone())
            .unwrap();
        assert!(!app.scan.is_running());
        assert_eq!(app.scan.snapshots().len(), 1);
        app.scan_quick_folder(&ctx, SearchSurface::Quick, hit.clone())
            .unwrap();
        assert_eq!(app.scan.active_path(), Some(other.as_path()));
        assert_eq!(app.source().unwrap().id, previous.id);
        finish_quick_test_scan(&mut app, &ctx);
        assert_eq!(app.scan.snapshots().len(), 2);
        let result = app.source().unwrap();
        assert_eq!(result.scan.folders[0].path, other);
        assert_eq!(result.scan.folders[0].logical_bytes, 4);
        let (id, generation, item) = app.detail_target.unwrap();
        assert_eq!((id, generation), (result.id, result.generation));
        assert_eq!(crate::search::item_path(&result.scan, item), file);

        app.analyze_quick_path(&ctx, SearchSurface::Quick, hit)
            .unwrap();
        assert!(!app.scan.is_running());
        assert_eq!(app.detail_target, Some((id, generation, item)));
        app.history_move(false);
        assert_eq!(app.source().unwrap().id, previous.id);
    }

    fn finish_quick_test_scan(app: &mut DiskWeaveApp, ctx: &egui::Context) {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while app.scan.is_running() {
            app.tick_background(ctx);
            assert!(std::time::Instant::now() < deadline);
            std::thread::yield_now();
        }
    }

    #[test]
    fn quick_analysis_returns_to_the_direct_search_result_and_selection() {
        let fixture = crate::duplicates::tests::Fixture::new();
        let path = fixture.0.join("selected.bin");
        std::fs::write(&path, b"test").unwrap();
        let ctx = egui::Context::default();
        let mut app = DiskWeaveApp::new();
        app.scan.start(fixture.0.clone()).unwrap();
        finish_quick_test_scan(&mut app, &ctx);
        app.quick.set_folder(fixture.0.clone());
        app.quick.text = "selected".into();
        app.quick.open();
        app.quick.start_live().unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while app.quick.pending() {
            app.quick.tick(&app.scan, std::time::Instant::now());
            assert!(std::time::Instant::now() < deadline);
            std::thread::yield_now();
        }
        app.quick.step(1);
        let hit = app.quick.selected_hit(&app.scan).unwrap();
        app.analyze_quick_path(&ctx, SearchSurface::Quick, hit)
            .unwrap();
        assert!(!app.scan.is_running());
        assert!(!app.quick.visible);
        assert!(app.quick.can_return_from_analysis());
        app.quick.open();
        app.quick.tick(&app.scan, std::time::Instant::now());
        assert_eq!(app.quick.text, "selected");
        assert_eq!(app.quick.folder(), fixture.0);
        assert_eq!(app.quick.selected_hit(&app.scan).unwrap().path, path);
    }

    #[test]
    fn cancelled_or_changed_quick_analysis_keeps_the_previous_view() {
        let fixture = crate::duplicates::tests::Fixture::new();
        let first = fixture.0.join("first");
        let other = fixture.0.join("other");
        std::fs::create_dir(&first).unwrap();
        std::fs::create_dir(&other).unwrap();
        let file = other.join("backup.bin");
        std::fs::write(&file, b"test").unwrap();
        let hit = crate::quick_search::Hit {
            identity: crate::metadata::read(&file).values.identity.value(),
            path: file.clone(),
            kind: crate::file_open::TargetKind::File,
            bytes: Some(4),
        };
        let mut app = DiskWeaveApp::new();
        let ctx = egui::Context::default();
        app.scan.start(first).unwrap();
        finish_quick_test_scan(&mut app, &ctx);
        let previous = app.source().unwrap();

        app.scan_quick_folder(&ctx, SearchSurface::Quick, hit.clone())
            .unwrap();
        app.scan.cancel();
        finish_quick_test_scan(&mut app, &ctx);
        assert_eq!(app.source().unwrap().id, previous.id);
        assert_eq!(app.scan.snapshots().len(), 1);
        assert!(app.input_error.as_deref().unwrap().contains("취소"));

        app.scan_quick_folder(&ctx, SearchSurface::Quick, hit.clone())
            .unwrap();
        std::fs::rename(&file, other.join("moved.bin")).unwrap();
        std::fs::write(&file, b"other").unwrap();
        finish_quick_test_scan(&mut app, &ctx);
        assert_eq!(app.source().unwrap().id, previous.id);
        assert!(Arc::ptr_eq(&app.source().unwrap().scan, &previous.scan));
        assert!(app.input_error.as_deref().unwrap().contains("바뀌었습니다"));
        assert!(app
            .analyze_quick_path(&ctx, SearchSurface::Quick, hit)
            .is_err());
    }

    #[test]
    fn quick_analysis_restores_the_scanned_hit_after_another_scan_changes_result_order() {
        let fixture = crate::duplicates::tests::Fixture::new();
        let first = fixture.0.join("first");
        let other = fixture.0.join("other");
        std::fs::create_dir(&first).unwrap();
        std::fs::create_dir(&other).unwrap();
        let file = first.join("backup.bin");
        std::fs::write(&file, b"small").unwrap();
        std::fs::write(other.join("backup.bin"), [0; 100]).unwrap();
        let mut app = DiskWeaveApp::new();
        let ctx = egui::Context::default();
        app.scan.start(first).unwrap();
        finish_quick_test_scan(&mut app, &ctx);
        app.quick.text = "backup".into();
        let search = |app: &mut DiskWeaveApp| {
            app.quick.open();
            let now = std::time::Instant::now();
            app.quick.tick(&app.scan, now);
            while app.quick.pending() {
                let elapsed = std::time::Instant::now();
                assert!(elapsed - now < Duration::from_secs(5));
                app.quick
                    .tick(&app.scan, elapsed + Duration::from_millis(200));
                std::thread::yield_now();
            }
        };
        search(&mut app);
        app.quick.step(1);
        let target = app.quick.selected(&app.scan).unwrap();
        app.open_quick_analysis(&ctx, SearchSurface::Quick, target)
            .unwrap();
        assert!(app.quick.can_return_from_analysis());
        app.scan.start_background(other).unwrap();
        finish_quick_test_scan(&mut app, &ctx);
        search(&mut app);
        assert_eq!(app.quick.count(), 2);
        assert_eq!(app.quick.selected_hit(&app.scan).unwrap().path, file);
    }

    #[test]
    fn hidden_main_keeps_scanning_and_explicit_exit_preserves_last_complete_result() {
        let root = std::env::temp_dir().join(format!("diskweave-hidden-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("sample.bin"), [1; 16]).unwrap();
        let mut app = DiskWeaveApp::new();
        let id = app.scan.add(&root).unwrap();
        app.scan.start_selected(&[id]).unwrap();
        app.desktop.initialized = true;
        app.desktop.life.hidden = true;
        let ctx = egui::Context::default();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while app.scan.is_running() {
            app.tick_background(&ctx);
            assert!(std::time::Instant::now() < deadline);
            std::thread::yield_now();
        }
        let result = app.scan.snapshots().pop().unwrap();
        assert_eq!(result.scan.files.len(), 1);
        app.scan.start_selected(&[id]).unwrap();
        app.desktop.life.exit = true;
        app.file_close_requested = true;
        while app.scan.is_running() {
            app.tick_background(&ctx);
            assert!(std::time::Instant::now() < deadline);
            std::thread::yield_now();
        }
        let retained = app.scan.snapshots().pop().unwrap();
        assert_eq!(retained.scan.files.len(), 1);
        assert!(app.desktop.life.exit);
        drop(app);
        std::fs::remove_dir_all(root).unwrap();
    }
}

#[cfg(test)]
mod analysis_regressions {
    use super::*;

    fn finish_scan(app: &mut DiskWeaveApp) {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while app.scan.is_running() {
            app.sync_scan();
            assert!(std::time::Instant::now() < deadline);
            std::thread::yield_now();
        }
    }

    #[test]
    fn an_old_analysis_item_is_rejected_when_the_pending_scan_finishes() {
        let fixture = crate::duplicates::tests::Fixture::new();
        let file = fixture.0.join("removed.bin");
        std::fs::write(&file, b"x").unwrap();
        let mut app = DiskWeaveApp::new();
        app.scan.start(fixture.0.clone()).unwrap();
        finish_scan(&mut app);
        let old = app.source().unwrap();
        assert_eq!(old.scan.files.len(), 1);
        std::fs::remove_file(&file).unwrap();
        app.scan.start(fixture.0.clone()).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while app.source().unwrap().generation == old.generation {
            app.analyze_at(old.id, old.generation, Item::File(0));
            assert!(std::time::Instant::now() < deadline);
            std::thread::yield_now();
        }
        let new = app.source().unwrap();
        assert!(new.generation > old.generation, "새 스캔 결과 수신 필요");
        assert!(app.input_error.is_some(), "이전 세대 요청 거부 필요");
    }

    #[test]
    fn an_unchanged_ntfs_folder_from_direct_search_can_open_its_analysis() {
        let fixture = crate::duplicates::tests::Fixture::new();
        let path = fixture.0.join("picked");
        std::fs::create_dir(&path).unwrap();
        std::fs::write(path.join("old.dat"), [0; 256]).unwrap();
        let mut app = DiskWeaveApp::new();
        app.scan.start(fixture.0.clone()).unwrap();
        finish_scan(&mut app);
        app.quick.set_folder(fixture.0.clone());
        app.quick.text = "picked".into();
        app.quick.open();
        app.quick.start_live().unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while app.quick.pending() {
            app.quick.tick(&app.scan, std::time::Instant::now());
            assert!(std::time::Instant::now() < deadline);
            std::thread::yield_now();
        }
        app.quick.step(1);
        let hit = app.quick.selected_hit(&app.scan).unwrap();
        assert_eq!(hit.path, path);
        assert!(hit.identity.is_some(), "직접 검색에서 확인한 파일 ID 필요");
        app.analyze_quick_path(&egui::Context::default(), SearchSurface::Quick, hit)
            .unwrap();
        assert!(!app.quick.visible, "기존 분석 화면으로 이동 필요");
    }
    #[test]
    fn cancelling_quick_analysis_keeps_the_open_saved_view() {
        let previous = crate::duplicates::tests::Fixture::new();
        let captured = Arc::new(crate::scanner::scan(&previous.0, || false).unwrap());
        let now = std::time::SystemTime::now();
        let saved = Arc::new(crate::snapshot::SavedScan::new(
            captured,
            now,
            now,
            Duration::ZERO,
        ));
        let fixture = crate::duplicates::tests::Fixture::new();
        let file = fixture.0.join("find.bin");
        std::fs::write(&file, [0; 4]).unwrap();
        let hit = crate::quick_search::Hit {
            identity: crate::metadata::read(&file).values.identity.value(),
            path: file,
            kind: crate::file_open::TargetKind::File,
            bytes: Some(4),
        };
        let mut app = DiskWeaveApp::new();
        let ctx = egui::Context::default();
        app.results.job.view = crate::documents::View::Saved(saved.clone());
        assert!(matches!(
            &app.results.job.view,
            crate::documents::View::Saved(_)
        ));
        app.scan_quick_folder(&ctx, SearchSurface::Quick, hit.clone())
            .unwrap();
        app.scan.cancel();
        finish_scan(&mut app);
        app.finish_quick_folder_scan();
        assert!(app.input_error.as_deref().unwrap().contains("취소"));
        assert!(
            matches!(&app.results.job.view, crate::documents::View::Saved(current)
            if Arc::ptr_eq(current, &saved)),
            "취소 시 열어 둔 Snapshot 화면을 복원해야 함"
        );
        app.analyze_quick_path(&ctx, SearchSurface::Quick, hit.clone())
            .unwrap();
        app.scan_quick_folder(&ctx, SearchSurface::Quick, hit)
            .unwrap();
        finish_scan(&mut app);
        app.finish_quick_folder_scan();
        assert!(matches!(app.results.job.view, crate::documents::View::Live));
        assert_eq!(app.source().unwrap().scan.folders[0].path, fixture.0);
    }

    #[test]
    fn drive_and_extended_paths_reuse_the_same_scan() {
        let fixture = crate::duplicates::tests::Fixture::new();
        let ordinary = fixture.0.join("find.bin");
        std::fs::write(&ordinary, [0; 4]).unwrap();
        let extended = std::fs::canonicalize(&ordinary).unwrap();
        assert_ne!(ordinary, extended);
        let mut app = DiskWeaveApp::new();
        app.scan.start(fixture.0.clone()).unwrap();
        finish_scan(&mut app);
        let hit = crate::quick_search::Hit {
            identity: crate::metadata::read(&extended).values.identity.value(),
            path: extended,
            kind: crate::file_open::TargetKind::File,
            bytes: Some(4),
        };
        let ctx = egui::Context::default();
        app.quick.open();
        app.analyze_quick_path(&ctx, SearchSurface::Quick, hit)
            .unwrap();
        assert!(!app.quick.visible);
        assert_eq!(app.scan.entries.len(), 1);
    }

    #[test]
    fn a_new_file_in_an_existing_scan_requests_a_rescan() {
        let fixture = crate::duplicates::tests::Fixture::new();
        let folder = fixture.0.join("child");
        std::fs::create_dir(&folder).unwrap();
        let mut app = DiskWeaveApp::new();
        app.scan.start(fixture.0.clone()).unwrap();
        finish_scan(&mut app);
        let file = folder.join("new.bin");
        std::fs::write(&file, [0; 4]).unwrap();
        let hit = crate::quick_search::Hit {
            identity: crate::metadata::read(&file).values.identity.value(),
            path: file,
            kind: TargetKind::File,
            bytes: Some(4),
        };
        app.quick.open();
        let error = app
            .analyze_quick_path(&egui::Context::default(), SearchSurface::Quick, hit)
            .unwrap_err();
        assert!(error.to_string().contains("다시 스캔"));
        assert!(app.quick.visible);
        assert_eq!(app.scan.entries.len(), 1);
    }

    #[test]
    fn a_replaced_search_folder_cannot_reuse_the_previous_analysis() {
        use std::os::windows::fs::{FileTimesExt, OpenOptionsExt};
        let fixture = crate::duplicates::tests::Fixture::new();
        let folder = fixture.0.join("child");
        std::fs::create_dir(&folder).unwrap();
        let older = std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000_000);
        let set_created = |time| {
            std::fs::File::options()
                .access_mode(windows_sys::Win32::Storage::FileSystem::FILE_WRITE_ATTRIBUTES)
                .custom_flags(windows_sys::Win32::Storage::FileSystem::FILE_FLAG_BACKUP_SEMANTICS)
                .open(&folder)
                .unwrap()
                .set_times(std::fs::FileTimes::new().set_created(time))
                .unwrap();
        };
        set_created(older);
        let mut app = DiskWeaveApp::new();
        app.scan.start(fixture.0.clone()).unwrap();
        finish_scan(&mut app);
        std::fs::rename(&folder, fixture.0.join("old")).unwrap();
        std::fs::create_dir(&folder).unwrap();
        set_created(older + Duration::from_secs(60));
        let hit = crate::quick_search::Hit {
            identity: Some(
                crate::file_ops::Target::capture(&folder)
                    .unwrap()
                    .identity(),
            ),
            path: folder.clone(),
            kind: TargetKind::Folder,
            bytes: None,
        };
        app.quick.open();
        let error = app
            .analyze_quick_path(&egui::Context::default(), SearchSurface::Quick, hit)
            .unwrap_err();
        assert!(error.to_string().contains("다시 스캔"));
        assert!(app.quick.visible);
    }
}
