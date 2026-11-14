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
            crate::ui_style::apply(&creation.egui_ctx);
            Ok(Box::new(DiskWeaveApp::default()))
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
    let (area, _) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), 108.0),
        egui::Sense::hover(),
    );
    let diameter = 108.0;
    let center = area.left_center() + egui::vec2(diameter / 2.0, 0.0);
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
    let legend = egui::Rect::from_min_max(area.min + egui::vec2(122.0, 0.0), area.max);
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
    ui.collapsing("파일 할당 합계·수집 기준", |ui| {
        for (label, total) in [
            ("파일 할당 · 경로별", stats.allocation),
            ("중복 제외 참고 합계", stats.unique_allocation),
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
                    "동일 ID의 할당값 불일치 {}개 제외",
                    stats.identity_conflicts
                ),
            );
        }
        ui.small("기본 스트림 · ADS 미수집 · 회수 가능 공간 아님");
    });
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
            ui.add(egui::Image::new((texture.id(), egui::vec2(32.0, 32.0))).uv(
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
                ui.small("읽기 전용");
                ui.menu_button("수집 옵션", |ui| {
                    let mut detailed = self.scan.details_requested();
                    let response = ui.add_enabled(
                        !self.scan.is_running(),
                        egui::Checkbox::new(&mut detailed, "할당 크기·파일 ID 추가 수집"),
                    );
                    if response.on_hover_text("끄면 추가 핸들 조회를 생략합니다. 다음 스캔부터 적용 · 기본 스트림 기준 · ADS 미수집").changed() {
                        self.input_error = self.scan.configure(detailed).err().map(|e| e.to_string());
                    }
                });
                if ui.available_width() < 700.0 && ui.button("상세 패널").clicked() {
                    self.details_open = !self.details_open;
                }
            });
        });
    }
    fn toolbar(&mut self, ui: &mut egui::Ui) {
        let (label_rect, _) =
            ui.allocate_exact_size(egui::vec2(ui.available_width(), 18.0), egui::Sense::hover());
        crate::ui_style::text(
            ui,
            label_rect,
            "분석 대상",
            false,
            crate::ui_style::TEXT,
            13.0,
        );
        let hint = egui::Rect::from_min_max(label_rect.min + egui::vec2(75.0, 0.0), label_rect.max);
        crate::ui_style::text(
            ui,
            hint,
            "드라이브 또는 폴더를 선택한 뒤 스캔",
            false,
            crate::ui_style::SOFT,
            12.0,
        );
        ui.add_space(9.0);
        let editable = !self.scan.is_running();
        let available = ui.available_width();
        ui.horizontal(|ui| {
            crate::ui_style::slot(
                ui,
                egui::vec2(64.0, 38.0),
                egui::Layout::left_to_right(egui::Align::Center),
                |ui| {
                    if let Some(path) =
                        self.locations_view
                            .drive_control(ui, !editable, self.target.text())
                    {
                        self.input_error = self.target.choose_folder(path).err();
                    }
                },
            );
            let path_width = (available - 438.0).max(80.0);
            let mut path = self.target.text().to_owned();
            if ui
                .add_enabled(
                    editable,
                    egui::TextEdit::singleline(&mut path)
                        .desired_width((path_width - 24.0).max(40.0))
                        .margin(egui::vec2(12.0, 10.0))
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
                    egui::Button::new("폴더 선택").min_size(egui::vec2(84.0, 38.0)),
                )
                .clicked()
            {
                if let Some(path) = rfd::FileDialog::new().pick_folder() {
                    self.input_error = self.target.choose_folder(path).err();
                }
            }
            crate::ui_style::slot(
                ui,
                egui::vec2(80.0, 38.0),
                egui::Layout::left_to_right(egui::Align::Center),
                |ui| {
                    ui.menu_button("위치 관리", |ui| {
                        ui.spacing_mut().item_spacing.y = 6.0;
                        egui::ScrollArea::vertical()
                            .max_height(360.0)
                            .show(ui, |ui| {
                                if let Some(path) = self.locations_view.controls(
                                    ui,
                                    &mut self.scan,
                                    self.target.path(),
                                ) {
                                    self.input_error = self.target.choose_folder(path).err();
                                }
                            });
                    });
                },
            );
            if ui
                .add_enabled(
                    matches!(self.scan.status(), ScanStatus::Running),
                    egui::Button::new("취소").min_size(egui::vec2(56.0, 38.0)),
                )
                .clicked()
            {
                self.scan.cancel();
            }
            if ui
                .add_enabled(
                    editable,
                    egui::Button::new(
                        egui::RichText::new("▷  스캔 시작")
                            .color(egui::Color32::from_rgb(6, 17, 27))
                            .strong(),
                    )
                    .fill(egui::Color32::from_rgb(56, 154, 236))
                    .stroke(egui::Stroke::new(
                        1.0,
                        egui::Color32::from_rgb(75, 166, 237),
                    ))
                    .min_size(egui::vec2(114.0, 38.0)),
                )
                .clicked()
            {
                self.begin_scan();
            }
        });
        ui.add_space(7.0);
        ui.small("또는 탐색기에서 폴더를 이 창으로 끌어놓으세요 · 선택 후 스캔 시작");
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
        let card_height = ui.available_height().max(50.0);
        ui.horizontal(|ui| {
            let unit = (ui.available_width() - 30.0) / 4.4;
            ui.spacing_mut().item_spacing.x = 10.0;
            for (index, (label, value)) in [
                ("논리적 파일 크기", size.text),
                (
                    "파일",
                    if has_results {
                        crate::ui_style::count(files)
                    } else {
                        "—".into()
                    },
                ),
                (
                    "하위 폴더",
                    if has_results {
                        crate::ui_style::count(folders)
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
            ]
            .into_iter()
            .enumerate()
            {
                let width = unit * if index == 0 { 1.4 } else { 1.0 };
                let (rect, response) = ui.allocate_exact_size(
                    egui::vec2(width, card_height - 1.0),
                    egui::Sense::hover(),
                );
                ui.painter()
                    .rect_filled(rect, 8.0, crate::ui_style::SURFACE);
                ui.painter().rect_stroke(
                    rect,
                    8.0,
                    egui::Stroke::new(1.0, crate::ui_style::LINE),
                    egui::StrokeKind::Inside,
                );
                let inner = rect.shrink2(egui::vec2(
                    15.0,
                    if card_height < 65.0 { 6.0 } else { 10.0 },
                ));
                let title = egui::Rect::from_min_size(inner.min, egui::vec2(inner.width(), 15.0));
                crate::ui_style::text(ui, title, label, false, crate::ui_style::MUTED, 12.0);
                let number = egui::Rect::from_min_max(inner.min + egui::vec2(0.0, 20.0), inner.max);
                let shown = if index == 3 && has_results {
                    if self.scan.is_running() {
                        "수집 중"
                    } else {
                        "스캔 완료"
                    }
                } else {
                    &value
                };
                crate::ui_style::text(
                    ui,
                    number,
                    shown,
                    false,
                    if index == 0 {
                        egui::Color32::from_rgb(155, 212, 255)
                    } else {
                        crate::ui_style::TEXT
                    },
                    if card_height < 65.0 { 20.0 } else { 23.0 },
                );
                response.on_hover_text(if index == 0 {
                    size.exact.clone()
                } else {
                    value
                });
            }
        });
    }
    fn tree(&mut self, ui: &mut egui::Ui) {
        crate::ui_style::panel_title(ui, "폴더 탐색", "크기순");
        let mut chosen = None;
        egui::Frame::new().inner_margin(8).show(ui, |ui| {
            let current = self.scan.selected_id().and_then(|id| self.scan.get(id));
            let label = if self.locations_view.global_mode {
                "등록 위치 전체".into()
            } else {
                current
                    .map(|e| {
                        e.path
                            .file_name()
                            .unwrap_or(e.path.as_os_str())
                            .to_string_lossy()
                            .into_owned()
                    })
                    .unwrap_or_else(|| "분석 위치".into())
            };
            ui.menu_button(format!("●  {label}  ▾"), |ui| {
                if ui
                    .selectable_label(self.locations_view.global_mode, "등록 위치 전체 검색")
                    .clicked()
                {
                    self.locations_view.global_mode = true;
                    ui.close();
                }
                for entry in &self.scan.entries {
                    if ui
                        .selectable_label(
                            self.scan.selected_id() == Some(entry.id)
                                && !self.locations_view.global_mode,
                            entry.path.display().to_string(),
                        )
                        .on_hover_text(if entry.stale {
                            "오래된 결과 · 전체 검색에서 제외"
                        } else {
                            "마지막 완료 스캔 기준"
                        })
                        .clicked()
                    {
                        chosen = Some(entry.id);
                        ui.close();
                    }
                }
            });
        });
        if let Some(id) = chosen {
            self.scan.choose(id);
            self.locations_view.global_mode = false;
            self.sync_scan();
        }
        let Some(source) = self.source() else {
            ui.add_space(15.0);
            ui.weak("  스캔 후 폴더가 표시됩니다");
            return;
        };
        if self.locations_view.global_mode {
            ui.weak("  정상 수집 위치 전체를 검색합니다");
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
                        egui::Sense::click(),
                    );
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
                        crate::ui_style::text(
                            ui,
                            arrow,
                            if explorer.is_expanded(folder) {
                                "▾"
                            } else {
                                "▸"
                            },
                            false,
                            crate::ui_style::SOFT,
                            12.0,
                        );
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
                        &item_name(&source.scan, Item::Folder(folder)).to_string_lossy(),
                        false,
                        if selected {
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
                        &SizeDisplay::new(source.scan.folders[folder].logical_bytes).text,
                        true,
                        crate::ui_style::SOFT,
                        11.0,
                    );
                    if response.clicked() {
                        destination = Some(folder);
                    }
                    response
                        .on_hover_text(source.scan.folders[folder].path.display().to_string())
                        .context_menu(|ui| {
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
                }
            });
        ui.add_space(5.0);
        ui.small("   화살표로 펼치기 · 이름으로 이동");
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
        let mut destination = None;
        let compact = ui.available_height() < 240.0;
        let available = ui.available_width();
        egui::Frame::new()
            .inner_margin(egui::Margin::symmetric(17, 0))
            .show(ui, |ui| {
                crate::ui_style::slot(
                    ui,
                    egui::vec2(available - 34.0, if compact { 42.0 } else { 58.0 }),
                    egui::Layout::left_to_right(egui::Align::Center),
                    |ui| {
                        let search_width = ((available - 34.0) * 0.46).clamp(160.0, 280.0);
                        let crumb_width = (ui.available_width() - search_width - 12.0).max(50.0);
                        crate::ui_style::slot(
                            ui,
                            egui::vec2(crumb_width, 40.0),
                            egui::Layout::left_to_right(egui::Align::Center),
                            |ui| {
                                if let (Some(source), Some(explorer)) = (
                                    self.source().filter(|_| !self.locations_view.global_mode),
                                    &self.explorer,
                                ) {
                                    if ui
                                        .add_enabled(
                                            explorer.folder() != 0,
                                            egui::Button::new("↑").frame(false),
                                        )
                                        .on_hover_text("상위 폴더")
                                        .clicked()
                                    {
                                        destination = source.scan.folders[explorer.folder()].parent;
                                    }
                                    let crumbs = explorer.breadcrumbs(&source.scan);
                                    for folder in crumbs
                                        .into_iter()
                                        .rev()
                                        .take(if crumb_width > 360.0 { 3 } else { 1 })
                                        .collect::<Vec<_>>()
                                        .into_iter()
                                        .rev()
                                    {
                                        if ui
                                            .add(
                                                egui::Button::new(
                                                    egui::RichText::new(
                                                        item_name(
                                                            &source.scan,
                                                            Item::Folder(folder),
                                                        )
                                                        .to_string_lossy(),
                                                    )
                                                    .strong(),
                                                )
                                                .frame(false),
                                            )
                                            .clicked()
                                        {
                                            destination = Some(folder);
                                        }
                                        if folder != explorer.folder() {
                                            ui.weak("/");
                                        }
                                    }
                                } else {
                                    ui.strong(if self.locations_view.global_mode {
                                        "등록 위치 전체"
                                    } else {
                                        "폴더"
                                    });
                                }
                            },
                        );
                        crate::ui_style::slot(
                            ui,
                            egui::vec2(search_width, 40.0),
                            egui::Layout::left_to_right(egui::Align::Center),
                            |ui| self.search.controls(ui, self.locations_view.global_mode),
                        );
                    },
                );
            });
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
                ui.horizontal(|ui| {
                    for (tab, label) in [(0, "폴더·파일"), (1, "큰 파일"), (2, "확장자")]
                    {
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
                                .min_size(egui::vec2(
                                    if narrow { 65.0 } else { 75.0 },
                                    if compact { 26.0 } else { 32.0 },
                                )),
                            )
                            .clicked()
                        {
                            self.tab = tab;
                        }
                    }
                    if self.tab != 0 {
                        if narrow {
                            ui.menu_button("범위", |ui| {
                                ui.checkbox(&mut self.statistics.matched, "검색 일치 파일만");
                            });
                        } else {
                            ui.checkbox(&mut self.statistics.matched, "검색 일치 파일만");
                        }
                    } else {
                        self.search.column_controls(ui);
                    }
                });
            });
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
                                    &item_name(&source.scan, item).to_string_lossy(),
                                    matches!(item, Item::Folder(_)),
                                );
                                self.search.widths.cells(
                                    ui,
                                    &source.scan,
                                    item,
                                    self.search.show_path,
                                    self.search.show_modified,
                                    self.search.show_allocated,
                                );
                            },
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
                    self.search.status(ui);
                    if let Some(explorer) = &self.explorer {
                        ui.response().on_hover_text(format!(
                            "직계 항목 {}개 · {}",
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
        ui.small(if self.statistics.matched {
            "유형별 비율 · 검색 일치 파일"
        } else {
            "유형별 비율 · 폴더 전체"
        });
        ui.add_space(8.0);
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
        crate::ui_style::property(ui, "논리 크기", &info.size.text).on_hover_text(info.size.exact);
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
        crate::ui_style::property(ui, "수정 시각", &info.modified);
        ui.collapsing("추가 메타데이터", |ui| {
            crate::ui_style::property(ui, "생성 시각", &info.created);
            ui.small(format!("속성 {}", info.attributes));
            if let Item::File(file) = item {
                crate::ui_style::property(ui, "Hard Link", &info.links);
                ui.small(format!("OS 파일 ID {}", info.identity));
                if scan.files[file].extra.links.value().is_some_and(|n| n > 1) {
                    ui.small("같은 파일에 여러 이름이 있습니다. 경로별 합계에 중복될 수 있습니다");
                }
            }
        });
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
        if let Some(output) = &self.statistics.output {
            allocation_summary(ui, &output.stats);
        }
        ui.small("크기 필터·Treemap: 논리 크기 기준 · ADS 미수집");
    }
    fn map_panel(&mut self, ui: &mut egui::Ui) {
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
                    ui.small("면적 = 논리 크기");
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
                    ui.menu_button("지도 옵션", |ui| {
                        ui.add(egui::Slider::new(&mut self.map_height, 96.0..=360.0).text("높이"));
                    });
                    ui.selectable_value(&mut self.map_matched, true, "검색 일치 파일");
                    ui.selectable_value(&mut self.map_matched, false, "폴더 전체");
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
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        if self.map_height == 0.0 {
            self.map_height = 245.0;
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
            .exact_size(panels.header)
            .frame(
                egui::Frame::new()
                    .fill(egui::Color32::from_rgb(16, 23, 32))
                    .inner_margin(egui::Margin::symmetric(24, 14)),
            )
            .show(ui, |ui| self.header(ui));
        egui::Panel::top("target")
            .exact_size(panels.target)
            .frame(
                egui::Frame::new()
                    .fill(crate::ui_style::SURFACE)
                    .inner_margin(egui::Margin::symmetric(
                        24,
                        if size.y < 650.0 { 6 } else { 12 },
                    )),
            )
            .show(ui, |ui| self.toolbar(ui));
        self.sync_scan();
        egui::Panel::top("summary")
            .exact_size(panels.summary)
            .frame(
                egui::Frame::new()
                    .fill(bg())
                    .inner_margin(egui::Margin::symmetric(
                        24,
                        if size.y < 650.0 { 6 } else { 12 },
                    )),
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
                    if !sources.is_empty() {
                        let detailed = sources.iter().filter(|s| s.scan.detailed).count();
                        ui.small(if detailed == sources.len() {
                            "현재 결과: 상세 수집"
                        } else if detailed == 0 {
                            "현재 결과: 빠른 스캔"
                        } else {
                            "현재 결과: 수집 방식 혼합"
                        });
                    }
                    let extra_errors: u64 =
                        sources.iter().map(|s| s.scan.metadata_error_count).sum();
                    if extra_errors > 0 {
                        ui.colored_label(
                            egui::Color32::YELLOW,
                            format!("추가 조회 문제 {extra_errors}개"),
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
            .frame(egui::Frame::new().fill(surface()))
            .show(ui, |ui| self.tree(ui));
        if panels.right > 0.0 {
            egui::Panel::right("details")
                .exact_size(panels.right)
                .resizable(false)
                .frame(egui::Frame::new().fill(surface()))
                .show(ui, |ui| {
                    crate::ui_style::panel_title(ui, "저장 공간 구성", "분석·상세");
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
