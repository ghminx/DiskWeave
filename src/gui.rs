use crate::scan_job::{ScanController, ScanStatus};
use crate::target_selection::TargetSelection;
use eframe::egui;
use std::path::PathBuf;
use std::time::Duration;

#[derive(Default)]
struct DiskWeaveApp {
    scan: ScanController,
    target: TargetSelection,
    input_error: Option<String>,
}

pub fn run() -> eframe::Result {
    let icon =
        eframe::icon_data::from_png_bytes(include_bytes!("../assets/brand/diskweave-icon.png"))
            .expect("프로젝트 아이콘 PNG");
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("DiskWeave")
            .with_inner_size([1100.0, 720.0])
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
            visuals.panel_fill = egui::Color32::from_rgb(12, 19, 29);
            visuals.widgets.inactive.bg_fill = egui::Color32::from_rgb(28, 42, 57);
            visuals.widgets.hovered.bg_fill = egui::Color32::from_rgb(40, 64, 87);
            creation.egui_ctx.set_visuals(visuals);
            creation.egui_ctx.global_style_mut(|style| {
                style.spacing.item_spacing = egui::vec2(10.0, 9.0);
                style
                    .text_styles
                    .insert(egui::TextStyle::Body, egui::FontId::proportional(15.0));
            });
            Ok(Box::new(DiskWeaveApp::default()))
        }),
    )
}

impl DiskWeaveApp {
    fn accept_drop(&mut self, paths: Vec<PathBuf>) {
        if self.scan.is_running() {
            self.input_error = Some("스캔이 끝난 뒤 새 폴더를 선택하세요".to_owned());
        } else {
            self.input_error = self.target.accept_drop(&paths).err();
        }
    }

    fn begin_scan(&mut self) {
        let Some(path) = self.target.path().map(PathBuf::from) else {
            self.input_error = Some("분석할 폴더를 선택하세요".to_owned());
            return;
        };
        self.input_error = self.scan.start(path).err().map(str::to_owned);
    }

    fn show_status(&self, ui: &mut egui::Ui) {
        match self.scan.status() {
            ScanStatus::Idle => {
                ui.label("대기 중");
            }
            ScanStatus::Running | ScanStatus::Cancelling => {
                let progress = self.scan.progress().expect("진행 중인 스캔");
                let elapsed = self.scan.elapsed().expect("진행 중인 스캔");
                ui.horizontal(|ui| {
                    ui.add(egui::Spinner::new());
                    ui.label(format!(
                        "{} · 처리한 파일 {}개 · 폴더 {}개 · {:.1}초",
                        if matches!(self.scan.status(), ScanStatus::Cancelling) {
                            "취소 중"
                        } else {
                            "스캔 중"
                        },
                        progress.files_scanned,
                        progress.folders_scanned,
                        elapsed.as_secs_f64()
                    ));
                });
            }
            ScanStatus::Completed => {
                ui.label("스캔 완료");
            }
            ScanStatus::Cancelled => {
                ui.label("스캔 취소됨 · 이전 결과 유지");
            }
            ScanStatus::Failed(error) => {
                ui.colored_label(egui::Color32::LIGHT_RED, format!("스캔 실패: {error}"));
            }
        }
    }
}

impl eframe::App for DiskWeaveApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.scan.poll();
        let dropped: Vec<_> = ui.ctx().input(|input| {
            input
                .raw
                .dropped_files
                .iter()
                .map(|file| file.path().to_path_buf())
                .collect()
        });
        if !dropped.is_empty() {
            self.accept_drop(dropped);
        }
        if self.scan.is_running() {
            ui.ctx().request_repaint_after(Duration::from_millis(100));
        }

        egui::CentralPanel::default()
            .frame(
                egui::Frame::new()
                    .fill(egui::Color32::from_rgb(12, 19, 29))
                    .inner_margin(24),
            )
            .show(ui, |ui| {
                ui.label(
                    egui::RichText::new("DiskWeave")
                        .size(28.0)
                        .strong()
                        .color(egui::Color32::from_rgb(145, 205, 255)),
                );
                ui.label("저장 공간 분석");
                ui.add_space(18.0);

                ui.heading("분석 대상");
                ui.horizontal(|ui| {
                    let editable = !self.scan.is_running();
                    let width = (ui.available_width() - 290.0).max(200.0);
                    let mut path_text = self.target.text().to_owned();
                    let response = ui.add_enabled(
                        editable,
                        egui::TextEdit::singleline(&mut path_text)
                            .hint_text("분석할 폴더 경로")
                            .desired_width(width),
                    );
                    if response.changed() {
                        self.target.set_text(path_text);
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
                    if ui
                        .add_enabled(
                            editable,
                            egui::Button::new("스캔 시작")
                                .fill(egui::Color32::from_rgb(40, 105, 165)),
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
                ui.label("탐색기에서 폴더를 창으로 끌어놓을 수도 있습니다");
                if let Some(error) = &self.input_error {
                    ui.colored_label(egui::Color32::LIGHT_RED, error);
                }
                ui.add_space(10.0);
                self.show_status(ui);

                ui.add_space(20.0);
                ui.separator();
                ui.add_space(12.0);
                ui.heading("스캔 요약");
                if let Some(completed) = self.scan.completed() {
                    let root = &completed.result.folders[0];
                    egui::Grid::new("scan_summary")
                        .num_columns(2)
                        .spacing([24.0, 12.0])
                        .show(ui, |ui| {
                            ui.label("분석한 경로");
                            ui.label(completed.path.display().to_string());
                            ui.end_row();
                            ui.label("논리 크기");
                            ui.label(format!("{} Byte", root.logical_bytes));
                            ui.end_row();
                            ui.label("파일");
                            ui.label(format!("{}개", root.file_count));
                            ui.end_row();
                            ui.label("하위 폴더");
                            ui.label(format!("{}개", root.folder_count));
                            ui.end_row();
                            ui.label("스캔 시간");
                            ui.label(format!("{:.2}초", completed.elapsed.as_secs_f64()));
                            ui.end_row();
                            ui.label("접근 오류");
                            ui.label(format!("{}건", completed.result.error_count));
                            ui.end_row();
                        });
                    if completed.result.error_count > 0 {
                        ui.colored_label(
                            egui::Color32::YELLOW,
                            "접근 오류로 결과 일부가 누락됐습니다",
                        );
                    }
                } else {
                    ui.label("분석할 폴더를 선택하고 스캔을 시작하세요");
                }
            });
    }
}
