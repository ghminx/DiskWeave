use crate::analysis::StatsScope;
use crate::analysis_panel::{AnalysisPanel, PanelKey};
use crate::explorer::{Explorer, Item};
use crate::file_open::{OpenAction, OpenController, OpenRequest, OpenStatus};
use crate::global_search::Hit;
use crate::locations::{Locations, Snapshot};
use crate::locations_ui::LocationsView;
use crate::map_job::{MapController, Target};
use crate::presentation::{detail, SizeDisplay};
use crate::scan_job::ScanStatus;
use crate::scanner::ScanResult;
use crate::search::{item_name, SearchInput};
use crate::search_view::{SearchScope, SearchView};
use crate::target_selection::TargetSelection;
use crate::treemap::MapScope;
use crate::treemap_view::{MapEvent, MapView};
use eframe::egui;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

#[derive(Default)]
pub struct DiskWeaveApp {
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
    input_error: Option<String>,
    explorer: Option<Explorer>,
    file_open: OpenController,
    search: SearchView,
    locations_view: LocationsView,
    map: MapController,
    map_view: MapView,
    map_matched: bool,
    map_collapsed: bool,
    map_height: f32,
    detail_target: Option<Target>,
    details_open: bool,
    tab: usize,
    logo: Option<egui::TextureHandle>,
    detail_context: Option<(u64, Option<u64>, bool, usize, SearchInput, bool)>,
}

pub fn run() -> eframe::Result {
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
            Ok(Box::new(DiskWeaveApp::new()))
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

fn open_menu(ui: &mut egui::Ui, item: Item) -> Option<OpenAction> {
    let mut action = None;
    match item {
        Item::Folder(_) => {
            if crate::ui_style::menu_action(ui, "탐색기에서 열기", true).clicked() {
                action = Some(OpenAction::OpenFolder);
            }
        }
        Item::File(_) => {
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
impl DiskWeaveApp {
    /// 현재 실행의 분석 위치. 이전 자동 저장소 접근 없이 제공.
    pub fn active_locations(&self) -> &Locations {
        &self.scan
    }
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
                    ui.spinner();
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
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label("폴더 구조를 확인하고 있습니다");
            });
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
        self.file_ui.blocked(&self.file_job) || self.file_refresh.busy()
    }
    fn copy_selection_paths(&self) -> Vec<PathBuf> {
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
    fn file_context(&self, explicit: Option<Vec<PathBuf>>) -> crate::file_ui::Context {
        let mut selected = if self.locations_view.global_mode {
            self.locations_view.selected_targets()
        } else {
            self.source()
                .map(|s| {
                    self.search
                        .job
                        .results()
                        .map(|r| {
                            self.search
                                .selection
                                .rows()
                                .filter_map(|row| r.items.get(row).copied())
                                .map(|item| (s.id, s.generation, item))
                                .collect()
                        })
                        .unwrap_or_default()
                })
                .unwrap_or_default()
        };
        if selected.is_empty() {
            if let Some(target) = self.selection_target() {
                selected.push(target);
            }
        }
        let mut writable = self.archive_record.is_none()
            && !self.scan.is_running()
            && !self.file_job.busy()
            && !self.file_refresh.busy()
            && self.file_ui.dialog.is_none()
            && !self.results.busy();
        let paths = if let Some(paths) = explicit {
            for path in &paths {
                for entry in &self.scan.entries {
                    if crate::file_ops::contains_path(&entry.path, path).unwrap_or(true) {
                        writable &= !entry.stale && entry.scan.completed().is_some();
                    }
                }
            }
            paths
        } else {
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
    fn file_tick(&mut self) {
        if self.file_job.poll() {
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
        if self.file_refresh.busy() {
            self.locations_view.preserve_live_selection();
        }
        let tracking = self
            .tracking
            .location
            .map(|id| (id, self.tracking.confirmed_after, self.tracking.running()));
        for update in self.file_refresh.poll(&mut self.scan, tracking) {
            if self.scan.selected_id() == Some(update.id) {
                self.replace_live_result(update.old.as_deref(), &update.new);
            }
        }
    }
    fn replace_live_result(&mut self, old: Option<&ScanResult>, new: &Arc<ScanResult>) {
        self.explorer = Some(self.search.replace_result(self.explorer.as_ref(), old, new));
        self.detail_target = None;
        self.detail_context = None;
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
        if let Some(record) = self.tracking.poll() {
            let old = self
                .scan
                .get(id)
                .and_then(|e| e.scan.completed())
                .map(|c| Arc::clone(&c.result));
            self.locations_view.preserve_live_selection();
            match self.scan.accept_tracking(id, &record) {
                Ok(true) if self.scan.selected_id() == Some(id) => {
                    self.replace_live_result(old.as_deref(), &record.scan);
                }
                Ok(_) => {}
                Err(error) => {
                    self.tracking.stop();
                    self.input_error = Some(error.to_string());
                    self.scan.tracking_failed(id);
                }
            }
        }
        if was_running && matches!(self.tracking.status, crate::tracking::Status::Failed(_)) {
            self.locations_view.preserve_live_selection();
            self.scan.tracking_failed(id);
        }
    }
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
    fn tracking_controls(&mut self, ui: &mut egui::Ui) {
        if self.archive_record.is_some() {
            return;
        }
        ui.separator();
        ui.strong("실시간 갱신");
        ui.add(
            egui::Label::new(format!(
                "추적 대상: {}",
                (if self.tracking.running() {
                    self.tracking
                        .location
                        .and_then(|id| self.scan.get(id))
                        .map(|e| e.path.as_path())
                } else {
                    self.selected_path()
                })
                .map(|p| p.display().to_string())
                .unwrap_or_default()
            ))
            .wrap(),
        );
        ui.small("로컬 NTFS 드라이브 또는 폴더 한 곳의 변경을 확인합니다.");
        if self.tracking.running() {
            ui.label(self.tracking.label());
            if matches!(self.tracking.status, crate::tracking::Status::Starting)
                || self.tracking.applying()
            {
                let progress = self.tracking.progress();
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.small(format!(
                        "조회 파일 {}개 · 폴더 {}개",
                        progress.files_scanned, progress.folders_scanned
                    ));
                });
            }
            if ui.button("실시간 갱신 중지").clicked() {
                self.tracking.stop();
            }
        } else {
            let resumable = self
                .scan
                .entries
                .iter()
                .find(|e| self.target.path() == Some(e.path.as_path()))
                .and_then(|e| e.scan.completed())
                .is_some_and(|c| c.tracking.is_some());
            if ui
                .add_enabled(
                    !self.scan.is_running() && !self.file_blocked() && self.target.path().is_some(),
                    egui::Button::new(if resumable {
                        "갱신 다시 시작"
                    } else {
                        "실시간 갱신 시작"
                    }),
                )
                .clicked()
            {
                self.start_tracking(false);
            }
            if resumable
                && ui
                    .add_enabled(
                        !self.scan.is_running() && !self.file_blocked(),
                        egui::Button::new("기준 다시 수집"),
                    )
                    .clicked()
            {
                self.start_tracking(true);
            }
        }
        if let crate::tracking::Status::Failed(error) = &self.tracking.status {
            ui.colored_label(
                egui::Color32::YELLOW,
                "실시간 갱신을 시작하거나 계속할 수 없습니다",
            );
            ui.add(egui::Label::new(error).wrap());
            ui.small("마지막 스캔 결과는 유지되지만 이후 변경 사항은 반영되지 않습니다.");
            if error.contains("os error 32") {
                ui.small("Windows가 사용 중인 파일·시스템 영역과의 공유 충돌입니다. 일반 작업 폴더로 범위를 줄여 시도할 수 있습니다.");
            }
        }
        if let Some((root, state)) = self
            .tracking
            .location
            .and_then(|id| self.scan.get(id))
            .and_then(|entry| {
                entry
                    .scan
                    .completed()
                    .and_then(|c| c.tracking.as_ref().map(|s| (&entry.path, s)))
            })
        {
            if !state.unavailable.is_empty() {
                ui.colored_label(
                    egui::Color32::from_rgb(235, 194, 120),
                    format!(
                        "확인 불가 {}개 · 검색과 용량 합계에서 제외",
                        state.unavailable.len()
                    ),
                );
                ui.small("제외된 폴더 내부는 미확인입니다. 항목별 최소 30초 간격으로 최대 64개씩 다시 확인합니다.");
                egui::CollapsingHeader::new("제외된 항목 · 이유 · 마지막 확인")
                    .default_open(true)
                    .show(ui, |ui| {
                        egui::ScrollArea::vertical().max_height(220.0).show_rows(
                            ui,
                            64.0,
                            state.unavailable.len(),
                            |ui, rows| {
                                for row in rows {
                                    let e = &state.unavailable[row];
                                    let path = root.join(e.relative());
                                    ui.add(egui::Label::new(path.display().to_string()).truncate())
                                        .on_hover_text(path.display().to_string());
                                    let error = e.reason.error().to_string();
                                    ui.add(
                                        egui::Label::new(
                                            egui::RichText::new(format!(
                                                "{} · {}",
                                                if e.listing {
                                                    "하위 목록 미확인"
                                                } else {
                                                    "항목 정보 미확인"
                                                },
                                                e.reason.label()
                                            ))
                                            .small(),
                                        )
                                        .truncate(),
                                    )
                                    .on_hover_text(error);
                                    ui.small(format!(
                                        "확인 {}",
                                        crate::local_date::display(Some(e.checked))
                                    ));
                                    ui.separator();
                                }
                            },
                        );
                    });
            }
        }
        if let Some(checked) = self.tracking.checked {
            ui.small(format!(
                "마지막 확인 {}",
                crate::local_date::display(Some(checked))
            ));
        }
        ui.small("처음에는 기준 스캔을 수행하며 Windows 권한 승인이 필요합니다.");
        ui.small("큰 폴더나 쓰기 중인 파일은 반영이 늦어질 수 있습니다.");
        ui.small("앱을 닫으면 자동 추적은 종료됩니다. 다음 실행에서는 다시 시작하세요.");
    }
    pub fn new() -> Self {
        Self::default()
    }
    fn close_windows(&mut self, ctx: &egui::Context) {
        let requested = ctx.input(|i| i.viewport().close_requested()) || self.file_close_requested;
        if !requested {
            return;
        }
        self.file_close_requested = true;
        self.file_job.cancel();
        self.file_ui.dialog = None;
        self.file_refresh.cancel(&mut self.scan);
        self.scan.cancel();
        self.tracking.stop();
        if self.results.busy() {
            self.results.job.cancel();
        }
        if self.file_job.working()
            || self.scan.is_running()
            || self.file_refresh.busy()
            || self.results.busy()
        {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            ctx.request_repaint_after(Duration::from_millis(30));
        } else {
            self.file_close_requested = false;
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }
    fn selection_target(&self) -> Option<Target> {
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
        let entry = locations.get(locations.selected_id()?)?;
        Some(Snapshot {
            id: entry.id,
            generation: entry.generation,
            scan: Arc::clone(&entry.scan.completed()?.result),
        })
    }
    fn export_context(&self) -> crate::results_ui::ExportContext {
        let sources = self.scan.snapshots();
        let selected = sources
            .iter()
            .position(|s| Some(s.id) == self.scan.selected_id());
        let matches = if self.locations_view.global_mode {
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
            matches: matches.filter(|_| self.search.input.is_active()),
        }
    }
    fn start_open(&mut self, request: OpenRequest) {
        self.input_error = self.file_open.start(request).err().map(|e| e.to_string());
    }
    fn sync_scan(&mut self) {
        if self.scan.poll() {
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
        self.tracking.stop();
        let Some(path) = self.target.path().map(PathBuf::from) else {
            self.input_error = Some("분석할 폴더를 선택하세요".into());
            return;
        };
        self.input_error = self.scan.start(path).err().map(|e| e.to_string());
    }
    fn accept_drop(&mut self, paths: Vec<PathBuf>) {
        self.input_error = if self.scan.is_running() {
            Some("스캔이 끝난 뒤 새 폴더를 선택하세요".into())
        } else {
            self.target.accept_drop(&paths).err()
        };
    }
    fn analyze_at(&mut self, id: u64, generation: u64, item: Item) {
        self.scan.choose(id);
        self.sync_scan();
        if !self
            .scan
            .get(id)
            .is_some_and(|e| e.generation == generation && !e.stale)
        {
            self.input_error = Some("결과가 변경되었습니다. 다시 스캔해 주세요.".into());
            return;
        }
        if let Some(source) = self.source() {
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
        }
    }
    fn navigate(&mut self, folder: usize, clear_search: bool) {
        if let (Some(source), Some(explorer)) = (self.source(), &mut self.explorer) {
            if source.scan.folders[explorer.folder()].parent == Some(folder) {
                explorer.up(&source.scan);
            } else {
                explorer.open_folder(&source.scan, folder);
            }
            if clear_search {
                self.search.input = SearchInput::default();
            }
            self.search.whole_scan = false;
            self.search.refresh(&source.scan, folder);
            self.detail_target = Some((source.id, source.generation, Item::Folder(folder)));
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
        self.statistics.select_target(selected.target);
        if self.locations_view.global_mode {
            self.locations_view.select_target(selected.target);
        } else if let (Some(source), Some(explorer)) = (self.source(), &mut self.explorer) {
            selected.apply_local(&source, explorer, &mut self.search);
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
        if let Some((key, sources)) = self.derived_input(self.statistics.matched) {
            if self.statistics.needs(&key) {
                let scope = if self.statistics.matched {
                    StatsScope::Matches(self.hits())
                } else if let Some(folder) = key.folder {
                    StatsScope::Subtree(folder)
                } else {
                    StatsScope::All
                };
                self.statistics.submit(key, sources, scope);
            }
        } else {
            self.statistics.clear();
        }
        if let Some((key, sources)) = self.derived_input(self.map_matched) {
            if self.map.needs(&key) {
                let scope = if self.map_matched {
                    MapScope::Matches(self.hits())
                } else if let Some(folder) = key.folder {
                    MapScope::Folder(folder)
                } else {
                    MapScope::Locations
                };
                self.map.submit(key, sources, scope);
            }
        } else {
            self.map.clear();
        }
        self.statistics.poll();
        self.map.poll();
    }
    fn status(&self) -> String {
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
            return format!(
                "{} · 마지막 확인 {}",
                self.tracking.label(),
                crate::local_date::display(self.tracking.checked)
            );
        }
        match self.scan.status() {
            ScanStatus::Idle => "대기 중".into(),
            ScanStatus::Running | ScanStatus::Cancelling => {
                let progress = crate::engine::Progress {
                    stage: self.scan.stage().unwrap_or_default(),
                    counts: self.scan.progress().expect("진행 중 스캔"),
                };
                if matches!(self.scan.status(), ScanStatus::Cancelling) {
                    format!(
                        "취소 중 · {:.1}초",
                        self.scan.elapsed().unwrap_or_default().as_secs_f64()
                    )
                } else {
                    let text =
                        progress.label(self.scan.elapsed().unwrap_or_default().as_secs_f64());
                    if matches!(
                        progress.stage,
                        crate::engine::Stage::Basic | crate::engine::Stage::Fallback
                    ) {
                        format!("{text} · 동시 작업 {}개", self.scan.parallelism().workers())
                    } else {
                        text
                    }
                }
            }
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
    fn header(&mut self, ui: &mut egui::Ui) {
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
                if ui.ctx().content_rect().width() < 1050.0 && ui.button("선택 항목").clicked()
                {
                    self.details_open = !self.details_open;
                }
                let rect = ui.available_rect_before_wrap();
                let response = ui.allocate_rect(rect, egui::Sense::click_and_drag());
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
    fn toolbar(&mut self, ui: &mut egui::Ui) {
        let editable = !self.scan.is_running() && !self.file_blocked();
        let available = ui.available_width();
        ui.with_layout(egui::Layout::left_to_right(egui::Align::Min), |ui| {
            ui.spacing_mut().interact_size.y = 32.0;
            ui.spacing_mut().button_padding = egui::vec2(10.0, 6.0);
            crate::ui_style::slot(
                ui,
                egui::vec2(80.0, 32.0),
                egui::Layout::left_to_right(egui::Align::Center),
                |ui| {
                    let label = self.target.text().chars().take(2).collect::<String>();
                    ui.menu_button(format!("{label}  ▼"), |ui| {
                        if let Some(path) =
                            self.locations_view
                                .drive_control(ui, !editable, self.target.text())
                        {
                            self.input_error = self.target.choose_folder(path).err();
                        }
                        ui.separator();
                        ui.menu_button("분석 위치 관리", |ui| {
                            ui.set_max_width(420.0);
                            ui.add_enabled_ui(editable, |ui| {
                                if let Some(path) = self.locations_view.controls(
                                    ui,
                                    &mut self.scan,
                                    self.target.path(),
                                ) {
                                    if self.scan.entries.iter().any(|e| e.path == path) {
                                        self.target.choose_registered(path);
                                        self.input_error = None;
                                    } else {
                                        self.input_error = self.target.choose_folder(path).err();
                                    }
                                }
                            });
                        });
                        ui.menu_button("실시간 갱신", |ui| {
                            ui.set_max_width(380.0);
                            ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Wrap);
                            self.tracking_controls(ui);
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
                        ui.menu_button("스캔 옵션 · 앱 정보", |ui| {
                            ui.set_max_width(340.0);
                            egui::ScrollArea::vertical()
                                .max_height((ui.ctx().content_rect().height() - 80.0).max(120.0))
                                .show(ui, |ui| {
                                    ui.collapsing("글꼴 라이선스 · Pretendard", |ui| {
                                        ui.label(include_str!("../assets/fonts/OFL.txt"));
                                    });
                                    ui.label("일반 스캔");
                                    ui.small("용량·디스크 할당 크기·파일 ID를 함께 확인합니다.");
                                    ui.separator();
                                    let mut parallelism = self.scan.parallelism();
                                    ui.add_enabled_ui(
                                        !self.scan.is_running()
                                            && !self.file_blocked()
                                            && self.archive_record.is_none(),
                                        |ui| {
                                            egui::ComboBox::from_id_salt("scan_parallelism")
                                                .selected_text(format!(
                                                    "동시 작업: {}",
                                                    parallelism.label()
                                                ))
                                                .show_ui(ui, |ui| {
                                                    for choice in [
                                                        crate::scanner::Parallelism::Auto,
                                                        crate::scanner::Parallelism::Sequential,
                                                        crate::scanner::Parallelism::Two,
                                                        crate::scanner::Parallelism::Four,
                                                        crate::scanner::Parallelism::Eight,
                                                    ] {
                                                        ui.selectable_value(
                                                            &mut parallelism,
                                                            choice,
                                                            choice.label(),
                                                        );
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
                                    ui.small("기본 스캔에 적용 · 자동 모드는 최대 8개");
                                    ui.small("작업 수가 많을수록 디스크 부하가 커질 수 있습니다.");
                                });
                        });
                    });
                },
            );
            let path_width = (available - 298.0).max(80.0);
            let mut path = self.target.text().to_owned();
            if ui
                .add_enabled(
                    editable,
                    egui::TextEdit::singleline(&mut path)
                        .desired_width((path_width - 24.0).max(40.0))
                        .margin(egui::vec2(10.0, 6.0))
                        .min_size(egui::vec2(40.0, 32.0))
                        .align(egui::Align2::LEFT_CENTER)
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
                    egui::Button::new("폴더 선택").min_size(egui::vec2(84.0, 32.0)),
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
                    .min_size(egui::vec2(114.0, 32.0)),
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
    fn summary(&self, ui: &mut egui::Ui) {
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
            let state = if self.scan.is_running() {
                "스캔 중"
            } else if stale {
                "갱신 필요"
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
            ui.label(
                egui::RichText::new(if preview.is_some() {
                    format!("{} · 계산 중", size.text)
                } else if errors > 0 {
                    format!("{} · 일부", size.text)
                } else {
                    size.text
                })
                .size(12.0),
            )
            .on_hover_text(size.exact);
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
                            if let Some(action) = open_menu(ui, Item::Folder(folder)) {
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
    }
    fn center(&mut self, ui: &mut egui::Ui) {
        self.preview_banner(ui);
        if self.show_preview() {
            self.preview_table(ui);
            return;
        }
        let mut destination = None;
        let compact = ui.available_height() < 300.0;
        let available = ui.available_width();
        let layout = crate::workspace::SearchLayout::new(available);
        let mut chosen_scope = None;
        egui::Frame::new()
            .inner_margin(egui::Margin::symmetric(12, 0))
            .show(ui, |ui| {
                let control_width = layout.scope + layout.input + layout.filter + 16.0;
                let crumb_width = if layout.stacked {
                    available - 24.0
                } else {
                    available - 24.0 - control_width - 8.0
                };
                let crumb_source = self.source();
                let crumb_explorer = self.explorer.as_ref();
                let whole = self.search.whole_scan;
                let global = self.locations_view.global_mode;
                let draw_crumb = |ui: &mut egui::Ui| {
                    ui.spacing_mut().button_padding = egui::vec2(6.0, 3.0);
                    ui.spacing_mut().interact_size.y = 24.0;
                    if let (Some(source), Some(explorer)) = (&crumb_source, crumb_explorer) {
                        let folder = if whole { 0 } else { explorer.folder() };
                        if !global {
                            if crate::ui_style::back_button(ui, folder != 0).clicked() {
                                destination = source.scan.folders[folder].parent;
                            }
                            let path = source.scan.folders[folder].path.display().to_string();
                            let (rect, response) = ui.allocate_exact_size(
                                egui::vec2(ui.available_width(), 30.0),
                                egui::Sense::click(),
                            );
                            crate::ui_style::text(
                                ui,
                                rect,
                                &path,
                                false,
                                crate::ui_style::TEXT,
                                13.0,
                            );
                            response.on_hover_text(&path).context_menu(|ui| {
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
                        } else {
                            ui.strong("모든 분석 위치");
                        }
                    } else {
                        ui.strong("폴더");
                    }
                };
                if layout.stacked {
                    crate::ui_style::slot(
                        ui,
                        egui::vec2(crumb_width, 32.0),
                        egui::Layout::left_to_right(egui::Align::Center),
                        draw_crumb,
                    );
                    crate::ui_style::slot(
                        ui,
                        egui::vec2(available - 24.0, 36.0),
                        egui::Layout::left_to_right(egui::Align::Center),
                        |ui| {
                            chosen_scope = self.search.controls(
                                ui,
                                self.locations_view.global_mode,
                                self.scan.snapshots().len() > 1,
                            );
                        },
                    );
                } else {
                    crate::ui_style::slot(
                        ui,
                        egui::vec2(available - 24.0, layout.header),
                        egui::Layout::left_to_right(egui::Align::Center),
                        |ui| {
                            crate::ui_style::slot(
                                ui,
                                egui::vec2(crumb_width, 40.0),
                                egui::Layout::left_to_right(egui::Align::Center),
                                draw_crumb,
                            );
                            crate::ui_style::slot(
                                ui,
                                egui::vec2(control_width, 40.0),
                                egui::Layout::left_to_right(egui::Align::Center),
                                |ui| {
                                    chosen_scope = self.search.controls(
                                        ui,
                                        self.locations_view.global_mode,
                                        self.scan.snapshots().len() > 1,
                                    );
                                },
                            );
                        },
                    );
                }
            });
        if let Some(scope) = chosen_scope {
            self.locations_view.choose_scope(&mut self.search, scope);
            self.detail_target = None;
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
                        egui::vec2(available - 16.0, layout.chips),
                        egui::Layout::left_to_right(egui::Align::Center),
                        |ui| self.search.filter_chips(ui),
                    );
                });
        }
        if let Some(folder) = destination.take() {
            self.navigate(folder, false);
        }
        self.refresh_derived();
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
        self.search.headers(ui);
        self.search.poll();
        let mut open = None;
        let mut enter = None;
        let mut analyze = false;
        let mut pick = None;
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
                                    if crate::ui_style::menu_action(ui, "이 폴더에서 검색", true)
                                        .clicked()
                                    {
                                        destination = Some(id);
                                        ui.close();
                                    }
                                }
                                if let Some(action) = open_menu(ui, item) {
                                    open = Some(OpenRequest::for_item(&source.scan, item, action));
                                }
                            });
                        });
                    }
                });
            self.search.horizontal_offset = output.state.clamped_offset().x;
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
            if let Some(explorer) = &mut self.explorer {
                self.search.analyze(&source.scan, explorer);
            }
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
    }
    fn details(&mut self, ui: &mut egui::Ui) {
        if self.show_preview() {
            self.preview_details(ui);
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
        crate::ui_style::property(ui, "크기", &info.size.text).on_hover_text(info.size.exact);
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
                        ui.spinner();
                        ui.label("연결 개수 확인 중…");
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
        if let Some(event) = self.map_view.show(ui, &self.map, selected) {
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
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
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

        if self.archive_record.is_none() {
            if self.scan.is_running() {
                self.tracking.stop();
                self.tracking.location = None;
            }
            self.sync_scan();
            if let Some(model) = self.scan.preview() {
                if self.preview_location != self.scan.selected_id() {
                    self.preview_view = Default::default();
                    self.preview_location = self.scan.selected_id();
                }
                self.preview_view.sync(model);
            }
            self.sync_tracking();
            self.file_tick();
            self.close_windows(ui.ctx());
            if self.scan.is_running()
                || self.tracking.running()
                || self.file_job.working()
                || self.file_refresh.busy()
            {
                ui.ctx().request_repaint_after(Duration::from_millis(30));
            }
        }
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
        if let crate::documents::View::Comparison(report) = &self.results.job.view {
            let report = Arc::clone(report);
            egui::CentralPanel::default()
                .frame(egui::Frame::new().fill(bg()))
                .show(ui, |ui| self.results.comparison(ui, &report));
            return;
        }
        if let crate::documents::View::Saved(record) = &self.results.job.view {
            let saved = crate::local_date::display(Some(record.saved));
            let busy = self
                .saved_viewer
                .as_ref()
                .is_some_and(|viewer| viewer.results.busy());
            let mut back = false;
            egui::Panel::top("saved_navigation")
                .exact_size(36.0)
                .frame(egui::Frame::new().fill(surface()).inner_margin(4))
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        back = ui
                            .add_enabled(!busy, egui::Button::new("← 현재 분석으로 돌아가기"))
                            .clicked();
                        ui.label(format!("저장된 결과 · {saved}"));
                    });
                });
            if !back {
                if let Some(viewer) = &mut self.saved_viewer {
                    viewer.ui(ui, _frame);
                }
                return;
            }
            self.results.job.back();
            self.saved_viewer = None;
        }
        if self.map_height == 0.0 {
            self.map_height = 192.0;
            if ui.ctx().content_rect().height() < 650.0 {
                self.map_collapsed = true;
            }
        }
        self.file_open.poll();
        if self.archive_record.is_some() {
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
            self.details_open,
            self.map_collapsed || self.show_preview(),
        );
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
            .show(ui, |ui| self.header(ui));
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
                    .inner_margin(egui::Margin::symmetric(14, 4)),
            )
            .show(ui, |ui| self.summary(ui));
        egui::Panel::bottom("status")
            .exact_size(panels.status)
            .frame(egui::Frame::new().fill(surface()).inner_margin(4))
            .show(ui, |ui| {
                ui.spacing_mut().interact_size.y = 20.0;
                ui.horizontal(|ui| {
                    if self.scan.is_running() {
                        ui.spinner();
                    }
                    let status = self.status();
                    let status_width = (ui.available_width() * 0.40).min(500.0);
                    ui.add_sized(
                        [status_width, 18.0],
                        egui::Label::new(egui::RichText::new(&status).small()).truncate(),
                    )
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
        egui::Panel::left("tree")
            .exact_size(panels.left)
            .resizable(false)
            .frame(egui::Frame::new().fill(surface()))
            .show(ui, |ui| self.tree(ui));
        if size.x < 1050.0 && self.details_open {
            let mut open = self.details_open;
            egui::Window::new("선택 항목")
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
                    crate::ui_style::panel_title(ui, "선택 항목", "");
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
                self.center(ui);
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
            self.input_error = crate::path_copy::copy_with(&paths, |text| {
                ui.ctx().copy_text(text);
                Ok(())
            })
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
            let mut context = self.file_context(Some(paths.clone()));
            for entry in &self.scan.entries {
                if crate::file_ops::contains_path(&entry.path, &destination).unwrap_or(true) {
                    context.writable &= !entry.stale && entry.scan.completed().is_some();
                }
            }
            self.input_error = self
                .file_ui
                .folder_drop(paths, destination, context)
                .err()
                .map(|e| e.to_string());
        }
        if let Some(kind) = self.locations_view.file_requested.take() {
            self.begin_file_action(kind, None);
        }
        if !self.file_blocked()
            && !ui.ctx().egui_wants_keyboard_input()
            && ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::F2))
        {
            self.begin_file_action(crate::file_ui::Kind::Rename, None);
        }
        if !self.file_blocked()
            && !ui.ctx().egui_wants_keyboard_input()
            && ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Delete))
        {
            self.begin_file_action(crate::file_ui::Kind::Recycle, None);
        }
        self.file_ui.show(ui.ctx(), &mut self.file_job);
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
                        ui.spinner();
                        if ui.button("목록 갱신 취소").clicked() {
                            self.file_refresh.cancel(&mut self.scan);
                        }
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
}
