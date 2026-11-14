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
use crate::search::{item_name, SearchInput};
use crate::search_view::SearchView;
use crate::target_selection::TargetSelection;
use crate::treemap::MapScope;
use crate::treemap_view::{MapEvent, MapView};
use eframe::egui;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

#[derive(Default)]
struct DiskWeaveApp {
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
            creation.egui_ctx.set_theme(egui::Theme::Dark);
            let mut visuals = egui::Visuals::dark();
            visuals.panel_fill = bg();
            visuals.override_text_color = Some(egui::Color32::from_rgb(225, 234, 244));
            visuals.weak_text_color = Some(egui::Color32::from_rgb(156, 176, 197));
            visuals.widgets.inactive.bg_fill = egui::Color32::from_rgb(25, 35, 48);
            visuals.widgets.hovered.bg_fill = egui::Color32::from_rgb(34, 53, 71);
            visuals.selection.bg_fill = egui::Color32::from_rgb(31, 59, 80);
            visuals.selection.stroke.color = accent();
            creation.egui_ctx.set_visuals(visuals);
            creation.egui_ctx.global_style_mut(|style| {
                style.spacing.item_spacing = egui::vec2(8.0, 6.0);
                style
                    .text_styles
                    .insert(egui::TextStyle::Body, egui::FontId::proportional(13.0));
                style
                    .text_styles
                    .insert(egui::TextStyle::Button, egui::FontId::proportional(13.0));
            });
            Ok(Box::new(DiskWeaveApp::default()))
        }),
    )
}
fn bg() -> egui::Color32 {
    egui::Color32::from_rgb(12, 17, 24)
}
fn surface() -> egui::Color32 {
    egui::Color32::from_rgb(16, 24, 34)
}
fn accent() -> egui::Color32 {
    egui::Color32::from_rgb(119, 196, 255)
}
fn type_chart(ui: &mut egui::Ui, stats: &crate::analysis::Statistics) {
    let mut groups: Vec<_> = stats
        .extensions
        .iter()
        .take(4)
        .map(|e| {
            (
                e.name.as_str(),
                e.bytes,
                crate::presentation::category_color(std::ffi::OsStr::new(&e.name)),
            )
        })
        .collect();
    let other = stats.bytes - groups.iter().map(|(_, bytes, _)| bytes).sum::<u64>();
    if other > 0 {
        groups.push(("기타", other, egui::Color32::from_rgb(89, 111, 131)));
    }
    ui.horizontal_top(|ui| {
        let diameter = (ui.available_width() * 0.37).clamp(64.0, 92.0);
        let (rect, _) =
            ui.allocate_exact_size(egui::vec2(diameter, diameter), egui::Sense::hover());
        let center = rect.center();
        let radius = diameter * 0.40;
        ui.painter().circle_stroke(
            center,
            radius,
            egui::Stroke::new(diameter * 0.12, egui::Color32::from_rgb(40, 53, 67)),
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
                ui.painter().add(egui::Shape::line(
                    points,
                    egui::Stroke::new(diameter * 0.12, color),
                ));
                start += angle as f32;
            }
        }
        ui.painter().text(
            center,
            egui::Align2::CENTER_CENTER,
            SizeDisplay::new(stats.bytes).text,
            egui::FontId::proportional(11.0),
            egui::Color32::WHITE,
        );
        ui.vertical(|ui| {
            for (name, bytes, color) in groups {
                ui.horizontal(|ui| {
                    let (rect, _) =
                        ui.allocate_exact_size(egui::vec2(6.0, 6.0), egui::Sense::hover());
                    ui.painter().rect_filled(rect, 1.0, color);
                    ui.add_sized(
                        [ui.available_width().max(45.0) - 40.0, 15.0],
                        egui::Label::new(egui::RichText::new(name).size(11.0)).truncate(),
                    );
                    ui.small(crate::presentation::percentage(bytes, stats.bytes));
                });
            }
        });
    });
}
fn frame() -> egui::Frame {
    egui::Frame::new().fill(surface()).inner_margin(12)
}
fn open_menu(ui: &mut egui::Ui, item: Item) -> Option<OpenAction> {
    let mut action = None;
    match item {
        Item::Folder(_) => {
            if ui.button("탐색기에서 열기").clicked() {
                action = Some(OpenAction::OpenFolder);
            }
        }
        Item::File(_) => {
            if ui.button("기본 앱으로 열기").clicked() {
                action = Some(OpenAction::OpenFile);
            }
            if ui.button("파일 위치 열기").clicked() {
                action = Some(OpenAction::RevealFile);
            }
        }
    }
    if action.is_some() {
        ui.close();
    }
    action
}
fn folder_icon(ui: &mut egui::Ui) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(16.0, 16.0), egui::Sense::hover());
    let p = rect.min + egui::vec2(1.0, 3.0);
    let points = vec![
        p,
        p + egui::vec2(5.0, 0.0),
        p + egui::vec2(7.0, 2.0),
        p + egui::vec2(13.0, 2.0),
        p + egui::vec2(13.0, 11.0),
        p + egui::vec2(0.0, 11.0),
        p,
    ];
    ui.painter()
        .add(egui::Shape::line(points, egui::Stroke::new(1.2, accent())));
}

impl DiskWeaveApp {
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
        let entry = self.scan.get(self.scan.selected_id()?)?;
        Some(Snapshot {
            id: entry.id,
            generation: entry.generation,
            scan: Arc::clone(&entry.scan.completed()?.result),
        })
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
            self.explorer = self.scan.completed().map(|c| Explorer::new(&c.result));
            self.detail_target = None;
        }
    }
    fn begin_scan(&mut self) {
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
            self.input_error = Some("결과가 변경됐습니다. 다시 수집하세요".into());
            return;
        }
        if let Some(source) = self.source() {
            let explorer = source.analyze(item);
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
            let folder = if matched && self.search.whole_scan && self.search.input.is_active() {
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
        match self.scan.status() {
            ScanStatus::Idle => "대기 중".into(),
            ScanStatus::Running | ScanStatus::Cancelling => {
                let p = self.scan.progress().expect("진행 중 스캔");
                format!(
                    "{} · 파일 {} · 폴더 {} · {:.1}초",
                    if matches!(self.scan.status(), ScanStatus::Cancelling) {
                        "취소 중"
                    } else {
                        "스캔 중"
                    },
                    p.files_scanned,
                    p.folders_scanned,
                    self.scan.elapsed().unwrap_or_default().as_secs_f64()
                )
            }
            ScanStatus::Completed => "스캔 완료".into(),
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
            ui.add(egui::Image::new((texture.id(), egui::vec2(29.0, 29.0))).uv(
                egui::Rect::from_min_max(
                    egui::pos2(39.0 / 960.0, 37.0 / 240.0),
                    egui::pos2(205.0 / 960.0, 203.0 / 240.0),
                ),
            ));
            ui.add(
                egui::Image::new((texture.id(), egui::vec2(119.0, 28.0))).uv(
                    egui::Rect::from_min_max(
                        egui::pos2(270.0 / 960.0, 92.0 / 240.0),
                        egui::pos2(575.0 / 960.0, 157.0 / 240.0),
                    ),
                ),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.weak("읽기 전용");
                if ui.button("상세 패널").clicked() {
                    self.details_open = !self.details_open;
                }
            });
        });
    }
    fn toolbar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.strong("분석 대상");
            ui.weak("폴더 선택 또는 탐색기에서 폴더 한 개 끌어놓기");
        });
        ui.horizontal(|ui| {
            let editable = !self.scan.is_running();
            let mut path = self.target.text().to_owned();
            if ui
                .add_enabled(
                    editable,
                    egui::TextEdit::singleline(&mut path)
                        .desired_width((ui.available_width() - 355.0).max(180.0))
                        .hint_text("분석할 폴더 경로"),
                )
                .changed()
            {
                self.target.set_text(path);
                self.input_error = None;
            }
            if ui
                .add_enabled(editable, egui::Button::new("폴더 선택"))
                .clicked()
            {
                if let Some(path) = rfd::FileDialog::new().pick_folder() {
                    self.input_error = self.target.choose_folder(path).err();
                }
            }
            ui.menu_button("위치 관리", |ui| {
                egui::ScrollArea::vertical()
                    .max_height(360.0)
                    .show(ui, |ui| {
                        if let Some(path) =
                            self.locations_view
                                .controls(ui, &mut self.scan, self.target.path())
                        {
                            self.input_error = self.target.choose_folder(path).err();
                        }
                    });
            });
            if ui
                .add_enabled(
                    editable,
                    egui::Button::new("스캔 시작").fill(egui::Color32::from_rgb(50, 143, 210)),
                )
                .clicked()
            {
                self.begin_scan();
            }
            if ui
                .add_enabled(
                    matches!(self.scan.status(), ScanStatus::Running),
                    egui::Button::new("취소"),
                )
                .clicked()
            {
                self.scan.cancel();
            }
        });
    }
    fn summary(&self, ui: &mut egui::Ui) {
        let sources = if self.locations_view.global_mode {
            self.scan.snapshots()
        } else {
            self.source().into_iter().collect()
        };
        let bytes: u128 = sources
            .iter()
            .map(|s| s.scan.folders[0].logical_bytes as u128)
            .sum();
        let files: u64 = sources.iter().map(|s| s.scan.folders[0].file_count).sum();
        let folders: u64 = sources.iter().map(|s| s.scan.folders[0].folder_count).sum();
        let errors: u64 = sources.iter().map(|s| s.scan.error_count).sum();
        let elapsed = self
            .scan
            .completed()
            .map_or(0.0, |c| c.elapsed.as_secs_f64());
        let has_results = !sources.is_empty();
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
        ui.columns(4, |cols| {
            for (col, (label, value)) in cols.iter_mut().zip([
                ("논리적 파일 크기", size.text),
                (
                    "파일",
                    if has_results {
                        format!("{files}개")
                    } else {
                        "—".into()
                    },
                ),
                (
                    "하위 폴더",
                    if has_results {
                        format!("{folders}개")
                    } else {
                        "—".into()
                    },
                ),
                (
                    "수집 상태",
                    if !has_results && !self.scan.is_running() {
                        "미수집".into()
                    } else {
                        format!(
                            "{} · {elapsed:.2}초 · 오류 {errors}",
                            if self.scan.is_running() {
                                "수집 중"
                            } else {
                                "완료"
                            }
                        )
                    },
                ),
            ]) {
                egui::Frame::new()
                    .fill(surface())
                    .stroke(egui::Stroke::new(1.0, egui::Color32::from_rgb(41, 55, 71)))
                    .corner_radius(6)
                    .inner_margin(10)
                    .show(col, |ui| {
                        ui.set_min_width(ui.available_width());
                        ui.small(label);
                        ui.add(
                            egui::Label::new(
                                egui::RichText::new(&value)
                                    .size(18.0)
                                    .strong()
                                    .color(accent()),
                            )
                            .truncate(),
                        )
                        .on_hover_text(
                            if label == "논리적 파일 크기" {
                                size.exact.clone()
                            } else {
                                value
                            },
                        );
                    });
            }
        });
    }
    fn tree(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.strong("폴더 탐색");
            ui.weak("크기순");
        });
        ui.separator();
        ui.checkbox(&mut self.locations_view.global_mode, "등록 위치 전체 검색");
        let mut chosen = None;
        for entry in &self.scan.entries {
            if ui
                .selectable_label(
                    if self.locations_view.global_mode {
                        self.detail_target.map(|t| t.0).or(self.scan.selected_id())
                            == Some(entry.id)
                    } else {
                        self.scan.selected_id() == Some(entry.id)
                    },
                    entry
                        .path
                        .file_name()
                        .unwrap_or(entry.path.as_os_str())
                        .to_string_lossy(),
                )
                .on_hover_text(format!(
                    "{}{}",
                    entry.path.display(),
                    if entry.stale {
                        " · 오래된 결과"
                    } else {
                        ""
                    }
                ))
                .clicked()
            {
                chosen = Some(entry.id);
            }
        }
        if let Some(id) = chosen {
            self.scan.choose(id);
            self.locations_view.global_mode = false;
            self.sync_scan();
        }
        ui.separator();
        let Some(source) = self.source() else {
            ui.weak("스캔 후 폴더가 표시됩니다");
            return;
        };
        if self.locations_view.global_mode {
            ui.weak("정상 수집 위치 전체를 검색합니다");
            return;
        }
        let Some(explorer) = &mut self.explorer else {
            return;
        };
        let mut destination = None;
        let mut toggle = None;
        let mut open = None;
        egui::ScrollArea::both().id_salt("folder_tree").show_rows(
            ui,
            27.0,
            explorer.visible_tree().len(),
            |ui, rows| {
                for row in rows {
                    let (folder, depth) = explorer.visible_tree()[row];
                    ui.horizontal(|ui| {
                        ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Extend);
                        ui.add_space(depth as f32 * 14.0);
                        if source.scan.children[folder].is_empty() {
                            ui.add_space(17.0);
                        } else if ui
                            .add_sized(
                                [17.0, 24.0],
                                egui::Button::new(if explorer.is_expanded(folder) {
                                    "▾"
                                } else {
                                    "▸"
                                })
                                .frame(false),
                            )
                            .clicked()
                        {
                            toggle = Some(folder);
                        }
                        folder_icon(ui);
                        let name = item_name(&source.scan, Item::Folder(folder)).to_string_lossy();
                        let response = ui.selectable_label(
                            explorer.folder() == folder
                                || self.detail_target
                                    == Some((source.id, source.generation, Item::Folder(folder))),
                            name,
                        );
                        if response.clicked() {
                            destination = Some(folder);
                        }
                        response.context_menu(|ui| {
                            if ui.button("이 폴더에서 검색").clicked() {
                                destination = Some(folder);
                                ui.close();
                            }
                            if let Some(action) = open_menu(ui, Item::Folder(folder)) {
                                open = Some(OpenRequest::for_item(
                                    &source.scan,
                                    Item::Folder(folder),
                                    action,
                                ));
                            }
                        });
                        ui.weak(SizeDisplay::new(source.scan.folders[folder].logical_bytes).text);
                    });
                }
            },
        );
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
        self.search.controls(ui, self.locations_view.global_mode);
        ui.horizontal(|ui| {
            for (tab, label) in [(0, "폴더·파일"), (1, "큰 파일"), (2, "확장자")] {
                ui.selectable_value(&mut self.tab, tab, label);
            }
            if self.tab != 0 {
                ui.checkbox(&mut self.statistics.matched, "검색 일치 파일만");
            } else {
                self.search.column_controls(ui);
            }
        });
        ui.separator();
        if self.tab != 0 {
            self.statistics.large_files = self.tab == 1;
            if self.tab == 1 {
                ui.horizontal(|ui| {
                    for n in [10, 50, 100] {
                        ui.selectable_value(&mut self.statistics.top_count, n, format!("Top {n}"));
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
            ui.label("선택한 경로에서 ‘스캔 시작’을 누르면 파일과 폴더가 표시됩니다");
            return;
        };
        let mut destination = None;
        let Some(explorer) = &self.explorer else {
            return;
        };
        if ui.available_height() > 200.0 {
            ui.horizontal_wrapped(|ui| {
                if ui
                    .add_enabled(explorer.folder() != 0, egui::Button::new("↑ 상위"))
                    .clicked()
                {
                    destination = source.scan.folders[explorer.folder()].parent;
                }
                for folder in explorer.breadcrumbs(&source.scan) {
                    if ui
                        .selectable_label(
                            folder == explorer.folder(),
                            item_name(&source.scan, Item::Folder(folder)).to_string_lossy(),
                        )
                        .clicked()
                    {
                        destination = Some(folder);
                    }
                    ui.weak("/");
                }
            });
        } else {
            ui.horizontal(|ui| {
                if ui
                    .add_enabled(explorer.folder() != 0, egui::Button::new("↑ 상위"))
                    .clicked()
                {
                    destination = source.scan.folders[explorer.folder()].parent;
                }
                ui.add(
                    egui::Label::new(
                        item_name(&source.scan, Item::Folder(explorer.folder())).to_string_lossy(),
                    )
                    .truncate(),
                );
            });
        }
        if let Some(folder) = destination {
            self.navigate(folder, false);
        }
        if ui.available_height() > 200.0 {
            if let Some(explorer) = &self.explorer {
                ui.small(format!("직계 항목 {}개", explorer.entries().len()))
                    .on_hover_text(
                        self.scan
                            .completed()
                            .map(|c| c.path.display().to_string())
                            .unwrap_or_default(),
                    );
            }
        }
        self.search.headers(ui);
        self.search.poll();
        if ui.available_height() > 150.0 {
            self.search.status(ui);
        }
        let mut open = None;
        let mut enter = None;
        let mut analyze = false;
        let mut pick = None;
        if let Some(results) = self.search.job.results() {
            if !self.search.editing
                && ui.input_mut(|i| i.consume_key(egui::Modifiers::CTRL, egui::Key::A))
            {
                self.search.selection.all(results.items.len());
                self.detail_target = None;
            }
            let mut scroll = egui::ScrollArea::both()
                .id_salt("entry_list")
                .horizontal_scroll_offset(self.search.horizontal_offset);
            if let Some(row) = self.search.reveal_row.take() {
                scroll = scroll
                    .vertical_scroll_offset(row as f32 * (26.0 + ui.spacing().item_spacing.y));
            }
            let output = scroll.show_rows(ui, 26.0, results.items.len(), |ui, rows| {
                for row in rows {
                    let item = results.items[row];
                    let background = ui.painter().add(egui::Shape::Noop);
                    let row_ui = ui.horizontal(|ui| {
                        ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Extend);
                        let name = item_name(&source.scan, item).to_string_lossy();
                        let response = ui.add_sized(
                            [self.search.widths.get(0), 26.0],
                            egui::Button::new(format!(
                                "{} {name}",
                                if matches!(item, Item::Folder(_)) {
                                    "▱"
                                } else {
                                    "·"
                                }
                            ))
                            .truncate()
                            .frame(false)
                            .selected(self.search.selection.contains(row)),
                        );
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
                            ui.add_enabled_ui(self.search.selection.count() <= 1, |ui| {
                                if ui.button("분석에서 보기").clicked() {
                                    analyze = true;
                                    ui.close();
                                }
                                if let Item::Folder(id) = item {
                                    if ui.button("이 폴더에서 검색").clicked() {
                                        destination = Some(id);
                                        ui.close();
                                    }
                                }
                                if let Some(action) = open_menu(ui, item) {
                                    open = Some(OpenRequest::for_item(&source.scan, item, action));
                                }
                            });
                        });
                        self.search.widths.cells(
                            ui,
                            &source.scan,
                            item,
                            self.search.show_path,
                            self.search.show_modified,
                        );
                    });
                    if self.search.selection.contains(row) {
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
            self.search.horizontal_offset = output.state.clamped_offset().x;
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
        ui.strong("저장 공간 구성");
        ui.separator();
        ui.small(if self.statistics.matched {
            "검색 일치 파일 기준"
        } else {
            "분석 폴더 전체 기준"
        });
        if let Some(output) = &self.statistics.output {
            type_chart(ui, &output.stats);
        } else {
            ui.weak(if self.statistics.running {
                "집계 중…"
            } else {
                "수집된 결과 없음"
            });
        }
        ui.add_space(8.0);
        ui.separator();
        ui.strong("선택 항목");
        let target = self.selection_target();
        let Some((id, generation, item)) = target else {
            ui.weak("항목을 선택하세요 · 여러 항목은 개별 열기를 실행하지 않습니다");
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
        egui::Frame::new()
            .fill(egui::Color32::from_rgb(23, 35, 49))
            .corner_radius(6)
            .inner_margin(10)
            .show(ui, |ui| {
                ui.small(info.kind);
                ui.label(
                    egui::RichText::new(item_name(&scan, item).to_string_lossy())
                        .size(17.0)
                        .strong()
                        .color(accent()),
                );
                ui.add(egui::Label::new(info.path.display().to_string()).wrap());
            });
        ui.add_space(5.0);
        ui.label(format!("논리 크기 {}", info.size.text))
            .on_hover_text(info.size.exact);
        ui.label(format!("부모 대비 {}", info.parent_percentage));
        ui.small(format!("수정 {}", info.modified));
        if let (Some(files), Some(folders)) = (info.files, info.folders) {
            ui.label(format!("파일 {files}개 · 하위 폴더 {folders}개"));
        }
        let mut action = None;
        ui.horizontal_wrapped(|ui| {
            if ui
                .button(if matches!(item, Item::Folder(_)) {
                    "탐색기에서 열기"
                } else {
                    "파일 위치 열기"
                })
                .clicked()
            {
                action = Some(if matches!(item, Item::Folder(_)) {
                    OpenAction::OpenFolder
                } else {
                    OpenAction::RevealFile
                });
            }
            if matches!(item, Item::File(_)) && ui.button("파일 열기").clicked() {
                action = Some(OpenAction::OpenFile);
            }
            if ui.button("분석에서 보기").clicked() {
                self.analyze_at(id, generation, item);
            }
        });
        if let Some(action) = action {
            self.start_open(OpenRequest::for_item(&scan, item, action));
        }
        ui.small("논리 크기 기준 · Hard Link는 경로별 집계");
    }
    fn map_panel(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.strong("공간 지도");
            ui.selectable_value(&mut self.map_matched, false, "폴더 전체");
            ui.selectable_value(&mut self.map_matched, true, "검색 일치 파일");
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
                if !self.map_collapsed {
                    ui.add(
                        egui::Slider::new(&mut self.map_height, 96.0..=360.0)
                            .text("높이")
                            .show_value(false),
                    );
                }
            });
        });
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
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        if self.map_height == 0.0 {
            self.map_height = 200.0;
        }
        self.file_open.poll();
        self.sync_scan();
        let dropped: Vec<_> = ui.ctx().input(|i| {
            i.raw
                .dropped_files
                .iter()
                .map(|f| f.path().to_path_buf())
                .collect()
        });
        if !dropped.is_empty() {
            self.accept_drop(dropped);
        }
        self.refresh_derived();
        let size = ui.available_size();
        let panels = crate::workspace::Panels::new(
            size.x,
            size.y,
            self.map_height,
            self.details_open,
            self.map_collapsed,
        );
        egui::Panel::top("brand")
            .exact_size(50.0)
            .frame(frame())
            .show(ui, |ui| self.header(ui));
        egui::Panel::top("target")
            .exact_size(84.0)
            .frame(frame())
            .show(ui, |ui| self.toolbar(ui));
        self.sync_scan();
        egui::Panel::top("summary")
            .exact_size(80.0)
            .frame(egui::Frame::new().fill(bg()).inner_margin(10))
            .show(ui, |ui| self.summary(ui));
        egui::Panel::bottom("status")
            .exact_size(26.0)
            .frame(egui::Frame::new().fill(surface()).inner_margin(4))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    if self.scan.is_running() {
                        ui.spinner();
                    }
                    ui.small(self.status()).on_hover_text(
                        self.scan
                            .active_path()
                            .map(|p| p.display().to_string())
                            .unwrap_or_default(),
                    );
                    let sources = if self.locations_view.global_mode {
                        self.scan.snapshots()
                    } else {
                        self.source().into_iter().collect()
                    };
                    let errors: u64 = sources.iter().map(|s| s.scan.error_count).sum();
                    let skipped: u64 = sources.iter().map(|s| s.scan.skipped_reparse_points).sum();
                    ui.small(format!("오류 {errors} · Reparse 제외 {skipped}"));
                    if let Some(results) = self
                        .search
                        .job
                        .results()
                        .filter(|_| !self.locations_view.global_mode)
                    {
                        if results.unknown_dates > 0 {
                            ui.small(format!("수정 시각 미수집 {}개 제외", results.unknown_dates));
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
                        ui.small(format!("미수집/오래된 위치 {excluded}개 제외"));
                    }
                    if !self.locations_view.global_mode
                        && self
                            .scan
                            .selected_id()
                            .and_then(|id| self.scan.get(id))
                            .is_some_and(|e| e.stale)
                    {
                        ui.colored_label(egui::Color32::YELLOW, "오래된 결과 · 전체 검색 제외");
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
                            ui.small("Windows 열기 처리 중…");
                        }
                        OpenStatus::Failed(error) => {
                            ui.colored_label(egui::Color32::LIGHT_RED, error);
                        }
                        OpenStatus::Submitted => {
                            ui.small("Windows에 열기 요청 전달됨");
                        }
                        OpenStatus::Idle => {}
                    }
                });
            });
        egui::Panel::bottom("map")
            .exact_size(panels.map)
            .frame(
                egui::Frame::new()
                    .fill(surface())
                    .inner_margin(if self.map_collapsed { 4 } else { 8 }),
            )
            .show(ui, |ui| self.map_panel(ui));
        egui::Panel::left("tree")
            .exact_size(panels.left)
            .resizable(false)
            .frame(frame())
            .show(ui, |ui| self.tree(ui));
        if panels.right > 0.0 {
            egui::Panel::right("details")
                .exact_size(panels.right)
                .resizable(false)
                .frame(frame())
                .show(ui, |ui| {
                    egui::ScrollArea::vertical()
                        .id_salt("detail_scroll")
                        .show(ui, |ui| self.details(ui));
                });
        }
        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(bg()).inner_margin(12))
            .show(ui, |ui| {
                ui.set_max_width((panels.center - 24.0).max(0.0));
                ui.set_max_height((panels.body - 24.0).max(0.0));
                self.center(ui);
            });
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
        {
            ui.ctx().request_repaint_after(Duration::from_millis(30));
        }
    }
}
