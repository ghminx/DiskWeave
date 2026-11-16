use crate::explorer::Item;
use crate::file_open::OpenAction;
use crate::map_job::{MapController, Target};
use crate::presentation::SizeDisplay;
use eframe::egui;

pub enum MapEvent {
    Pick(usize),
    Analyze(usize),
    Open(usize, OpenAction),
}
#[derive(Default)]
pub struct MapView {
    key: Option<(u64, [f32; 2])>,
    rects: Vec<crate::treemap::Rect>,
    colors: Vec<egui::Color32>,
}
impl MapView {
    pub fn show(
        &mut self,
        ui: &mut egui::Ui,
        map: &MapController,
        selected: Option<Target>,
    ) -> Option<MapEvent> {
        if map.running {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label("공간 지도 구성 중…");
            });
            ui.ctx()
                .request_repaint_after(std::time::Duration::from_millis(30));
        }
        if let Some(error) = &map.error {
            ui.colored_label(egui::Color32::LIGHT_RED, error);
        }
        let Some(output) = &map.output else {
            if !map.running {
                ui.painter().text(
                    ui.available_rect_before_wrap().center(),
                    egui::Align2::CENTER_CENTER,
                    "스캔 완료 후 용량별 공간 지도가 표시됩니다",
                    egui::FontId::proportional(12.0),
                    crate::ui_style::SOFT,
                );
            }
            return None;
        };
        if output.data.tiles.is_empty() {
            ui.label("표시할 용량이 없습니다 · 0 B 항목은 목록에서 확인하세요");
            return None;
        }
        let summary = format!(
            "전체 크기 {} · {}개 영역 · 0 B {}개 제외",
            SizeDisplay::new(output.data.total).text,
            output.data.tiles.len(),
            output.data.zero_count
        );
        let size = egui::vec2(
            ui.available_width().max(0.0),
            (ui.available_height() - 23.0).max(0.0),
        );
        let (area, _) = ui.allocate_exact_size(size, egui::Sense::hover());
        let key = (map.revision, [size.x, size.y]);
        if self.key != Some(key) {
            self.rects = crate::treemap::layout(&output.data.tiles, size.x as f64, size.y as f64)
                .expect("유효한 지도 영역");
            self.colors = output
                .data
                .tiles
                .iter()
                .enumerate()
                .map(|(row, tile)| {
                    if let Some(hit) = tile.target {
                        let scan = &output.sources[hit.source].scan;
                        match hit.item {
                            Item::Folder(_) => {
                                crate::ui_style::MAP_COLORS[row % crate::ui_style::MAP_COLORS.len()]
                            }
                            Item::File(id) => {
                                crate::presentation::category_color(std::ffi::OsStr::new(
                                    &crate::analysis::extension(&scan.files[id].name),
                                ))
                            }
                        }
                    } else {
                        egui::Color32::from_rgb(72, 87, 103)
                    }
                })
                .collect();
            self.key = Some(key);
        }
        let mut event = None;
        for (row, (tile, rect)) in output.data.tiles.iter().zip(&self.rects).enumerate() {
            let rect = egui::Rect::from_min_size(
                area.min + egui::vec2(rect.x as f32, rect.y as f32),
                egui::vec2(rect.width as f32, rect.height as f32),
            );
            if rect.width() < 1.0 || rect.height() < 1.0 {
                continue;
            }
            let paint = rect.shrink(1.5);
            if !paint.is_positive() {
                continue;
            }
            let target = map.route(row);
            let chosen = target.is_some() && target == selected;
            let response = ui.interact(
                rect,
                ui.scope_id().with(("map_tile", row)),
                egui::Sense::click(),
            );
            let color = if response.hovered() {
                self.colors[row].gamma_multiply(1.15)
            } else {
                self.colors[row]
            };
            ui.painter().rect_filled(paint, 3.0, color);
            if chosen {
                ui.painter().rect_stroke(
                    paint,
                    3.0,
                    egui::Stroke::new(2.0, egui::Color32::from_rgb(191, 225, 251)),
                    egui::StrokeKind::Inside,
                );
            }
            let name = tile
                .target
                .map(|hit| {
                    crate::search::item_name(&output.sources[hit.source].scan, hit.item)
                        .to_string_lossy()
                        .into_owned()
                })
                .unwrap_or_else(|| format!("묶인 항목 {}개", tile.count));
            if paint.width() > 65.0 && paint.height() > 24.0 {
                let painter = ui.painter().with_clip_rect(paint.shrink2(egui::vec2(
                    6.0,
                    if paint.height() > 46.0 { 6.0 } else { 2.0 },
                )));
                let center = paint.left_center()
                    + egui::vec2(10.0, if paint.height() > 46.0 { -7.0 } else { 0.0 });
                painter.text(
                    center,
                    egui::Align2::LEFT_CENTER,
                    &name,
                    egui::FontId::proportional(13.0),
                    egui::Color32::WHITE,
                );
                if paint.height() > 46.0 {
                    painter.text(
                        center + egui::vec2(0.0, 20.0),
                        egui::Align2::LEFT_CENTER,
                        SizeDisplay::new(tile.bytes).text,
                        egui::FontId::proportional(11.0),
                        egui::Color32::from_rgb(218, 232, 243),
                    );
                }
            }
            response.clone().on_hover_ui(|ui| {
                ui.strong(&name);
                if let Some(hit) = tile.target {
                    ui.label(
                        crate::search::item_path(&output.sources[hit.source].scan, hit.item)
                            .display()
                            .to_string(),
                    );
                } else {
                    ui.label(format!("{}개", tile.count));
                }
                ui.label(format!(
                    "{} · {} B · {}",
                    SizeDisplay::new(tile.bytes).text,
                    tile.bytes,
                    crate::presentation::percentage(tile.bytes, output.data.total)
                ));
            });
            if let Some(hit) = tile.target {
                if response.double_clicked() {
                    event = Some(match hit.item {
                        Item::Folder(_) => MapEvent::Analyze(row),
                        Item::File(_) => MapEvent::Open(row, OpenAction::OpenFile),
                    });
                } else if response.clicked() {
                    event = Some(MapEvent::Pick(row));
                }
                response.context_menu(|ui| {
                    if ui.button("경로 복사").clicked() {
                        let path =
                            crate::search::item_path(&output.sources[hit.source].scan, hit.item);
                        if let Err(error) = crate::path_copy::copy_with(&[path], |text| {
                            ui.ctx().copy_text(text);
                            Ok(())
                        }) {
                            ui.label(error.to_string());
                        } else {
                            ui.close();
                        }
                    }
                    if ui.button("용량 분석으로 이동").clicked() {
                        event = Some(MapEvent::Analyze(row));
                        ui.close();
                    }
                    if ui
                        .button(if matches!(hit.item, Item::Folder(_)) {
                            "탐색기에서 열기"
                        } else {
                            "기본 앱으로 열기"
                        })
                        .clicked()
                    {
                        event = Some(MapEvent::Open(
                            row,
                            if matches!(hit.item, Item::Folder(_)) {
                                OpenAction::OpenFolder
                            } else {
                                OpenAction::OpenFile
                            },
                        ));
                        ui.close();
                    }
                    if matches!(hit.item, Item::File(_)) && ui.button("파일 위치 열기").clicked()
                    {
                        event = Some(MapEvent::Open(row, OpenAction::RevealFile));
                        ui.close();
                    }
                });
            }
        }
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 12.0;
            for (row, tile) in output.data.tiles.iter().enumerate().take(5) {
                if let Some(hit) = tile.target {
                    let name = crate::search::item_name(&output.sources[hit.source].scan, hit.item)
                        .to_string_lossy();
                    ui.colored_label(self.colors[row], egui::RichText::new("■").size(8.0));
                    ui.add(
                        egui::Label::new(
                            egui::RichText::new(name)
                                .size(10.0)
                                .color(crate::ui_style::SOFT),
                        )
                        .truncate(),
                    );
                }
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.small(&summary)
                    .on_hover_text("클릭으로 선택 · 폴더 더블클릭으로 이동 · 0 B는 목록에서 확인");
            });
        });
        event
    }
}
