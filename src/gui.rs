use crate::explorer::{Explorer, Item};
use crate::file_open::{OpenAction, OpenController, OpenRequest, OpenStatus};
use crate::scan_job::{ScanController, ScanStatus};
use crate::scanner::ScanResult;
use crate::search::{item_bytes, item_modified, item_path, SearchInput};
use crate::search_view::SearchView;
use crate::target_selection::TargetSelection;
use eframe::egui;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

#[derive(Default)]
struct DiskWeaveApp {
    scan: ScanController,
    target: TargetSelection,
    input_error: Option<String>,
    explorer: Option<Explorer>,
    file_open: OpenController,
    search: SearchView,
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

fn folder_name(result: &ScanResult, folder: usize) -> String {
    let path = &result.folders[folder].path;
    path.file_name()
        .unwrap_or(path.as_os_str())
        .to_string_lossy()
        .into_owned()
}

fn open_menu(ui: &mut egui::Ui, folder: bool) -> Option<OpenAction> {
    let mut action = None;
    if folder {
        if ui.button("탐색기에서 열기").clicked() {
            action = Some(OpenAction::OpenFolder);
        }
    } else {
        if ui.button("기본 앱으로 열기").clicked() {
            action = Some(OpenAction::OpenFile);
        }
        if ui.button("파일 위치 열기").clicked() {
            action = Some(OpenAction::RevealFile);
        }
    }
    if action.is_some() {
        ui.close();
    }
    action
}

fn show_explorer(
    ui: &mut egui::Ui,
    result: &Arc<ScanResult>,
    explorer: &mut Explorer,
    view: &mut SearchView,
) -> Option<OpenRequest> {
    view.poll();
    view.controls(ui);
    let mut open_request = None;
    let mut destination = None;
    egui::ScrollArea::horizontal()
        .id_salt("breadcrumbs")
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                if ui
                    .add_enabled(explorer.folder() != 0, egui::Button::new("↑ 상위 폴더"))
                    .clicked()
                {
                    explorer.up(result);
                    view.whole_scan = false;
                }
                for folder in explorer.breadcrumbs(result) {
                    if ui
                        .selectable_label(folder == explorer.folder(), folder_name(result, folder))
                        .clicked()
                    {
                        destination = Some(folder);
                    }
                    ui.label("/");
                }
            })
        });
    if let Some(folder) = destination.take() {
        explorer.open_folder(result, folder);
        view.whole_scan = false;
    }
    let scope = explorer.folder();
    let scope_folder = &result.folders[scope];
    ui.label(format!(
        "현재 폴더: {} Byte · 파일 {}개 · 하위 폴더 {}개 · 직계 항목 {}개",
        scope_folder.logical_bytes,
        scope_folder.file_count,
        scope_folder.folder_count,
        explorer.entries().len()
    ));

    let mut toggle = None;
    let mut selection = None;
    let mut result_navigation = false;
    let mut analyze = false;
    let height = (ui.available_height() - 100.0).max(80.0);
    ui.horizontal_top(|ui| {
        ui.allocate_ui_with_layout(
            egui::vec2(250.0, height),
            egui::Layout::top_down(egui::Align::Min),
            |ui| {
                ui.strong("폴더 탐색");
                egui::ScrollArea::both()
                    .id_salt("folder_tree")
                    .max_height(height - 28.0)
                    .show_rows(ui, 24.0, explorer.visible_tree().len(), |ui, rows| {
                        for row in rows {
                            let (folder, depth) = explorer.visible_tree()[row];
                            ui.horizontal(|ui| {
                                ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Extend);
                                ui.set_min_height(24.0);
                                ui.add_space(depth as f32 * 16.0);
                                if result.children[folder].is_empty() {
                                    ui.add_space(24.0);
                                } else if ui
                                    .small_button(if explorer.is_expanded(folder) {
                                        "▼"
                                    } else {
                                        "▶"
                                    })
                                    .clicked()
                                {
                                    toggle = Some(folder);
                                }
                                let response = ui
                                    .selectable_label(
                                        explorer.folder() == folder,
                                        folder_name(result, folder),
                                    )
                                    .on_hover_text(format!(
                                        "{}\n{} Byte",
                                        result.folders[folder].path.display(),
                                        result.folders[folder].logical_bytes
                                    ));
                                response.context_menu(|ui| {
                                    if ui.button("이 폴더에서 검색").clicked() {
                                        destination = Some(folder);
                                        view.whole_scan = false;
                                        ui.memory_mut(|memory| {
                                            memory.request_focus(egui::Id::unique("search_query"))
                                        });
                                        ui.close();
                                    }
                                    if let Some(action) = open_menu(ui, true) {
                                        selection = Some(Item::Folder(folder));
                                        open_request = Some(OpenRequest::for_item(
                                            result,
                                            Item::Folder(folder),
                                            action,
                                        ));
                                    }
                                });
                                if response.clicked() {
                                    destination = Some(folder);
                                }
                            });
                        }
                    });
            },
        );
        ui.separator();
        ui.allocate_ui_with_layout(
            egui::vec2(ui.available_width(), height),
            egui::Layout::top_down(egui::Align::Min),
            |ui| {
                view.headers(ui);
                view.refresh(result, explorer.folder());
                view.status(ui);
                let scope = if view.whole_scan && view.input.is_active() {
                    0
                } else {
                    explorer.folder()
                };
                ui.label(format!(
                    "검색 범위: {}",
                    result.folders[scope].path.display()
                ));
                if let Some(results) = view.job.results() {
                    let items = &results.items;
                    if items.is_empty() {
                        ui.label("조건에 맞는 항목이 없습니다");
                    }
                    if !view.editing
                        && ui.input_mut(|input| {
                            input.consume_key(egui::Modifiers::CTRL, egui::Key::A)
                        })
                    {
                        view.selection.all(items.len());
                    }
                    ui.horizontal(|ui| {
                        if ui
                            .add_enabled(
                                view.selection.count() == 1,
                                egui::Button::new("분석에서 보기"),
                            )
                            .clicked()
                        {
                            analyze = true;
                        }
                    });
                    let mut scroll = egui::ScrollArea::both()
                        .id_salt(("entry_list", scope))
                        .max_height((height - 120.0).max(60.0));
                    if let Some(row) = view.reveal_row.take() {
                        scroll = scroll.vertical_scroll_offset(
                            row as f32 * (26.0 + ui.spacing().item_spacing.y),
                        );
                    }
                    scroll.show_rows(ui, 26.0, items.len(), |ui, rows| {
                        for row in rows {
                            let item = items[row];
                            let (name, kind) = match item {
                                Item::Folder(id) => (folder_name(result, id), "폴더"),
                                Item::File(id) => {
                                    (result.files[id].name.to_string_lossy().into_owned(), "파일")
                                }
                            };
                            ui.horizontal(|ui| {
                                ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Extend);
                                ui.set_min_height(26.0);
                                let response = ui.add_sized(
                                    [200.0, 26.0],
                                    egui::Button::new(format!("[{kind}] {name}"))
                                        .selected(view.selection.contains(row)),
                                );
                                if response.clicked() {
                                    let modifiers = ui.input(|input| input.modifiers);
                                    view.selection.click(row, modifiers.ctrl, modifiers.shift);
                                    if let Some(item) = view.selection.single(items) {
                                        selection = Some(item);
                                    }
                                }
                                if response.secondary_clicked() && !view.selection.contains(row) {
                                    view.selection.click(row, false, false);
                                    selection = Some(item);
                                }
                                if response.double_clicked() && view.selection.count() <= 1 {
                                    match item {
                                        Item::Folder(folder) => {
                                            destination = Some(folder);
                                            result_navigation = true;
                                        }
                                        Item::File(_) => {
                                            open_request = Some(OpenRequest::for_item(
                                                result,
                                                item,
                                                OpenAction::OpenFile,
                                            ))
                                        }
                                    }
                                }
                                response.context_menu(|ui| {
                                    if let Item::Folder(folder) = item {
                                        if ui.button("이 폴더에서 검색").clicked() {
                                            destination = Some(folder);
                                            view.whole_scan = false;
                                            ui.memory_mut(|memory| {
                                                memory
                                                    .request_focus(egui::Id::unique("search_query"))
                                            });
                                            ui.close();
                                        }
                                    }
                                    ui.add_enabled_ui(view.selection.count() <= 1, |ui| {
                                        if let Some(action) =
                                            open_menu(ui, matches!(item, Item::Folder(_)))
                                        {
                                            selection = Some(item);
                                            open_request =
                                                Some(OpenRequest::for_item(result, item, action));
                                        }
                                    });
                                });
                                let path = item_path(result, item).display().to_string();
                                ui.add_sized([260.0, 26.0], egui::Label::new(&path).truncate())
                                    .on_hover_text(&path);
                                ui.add_sized(
                                    [110.0, 26.0],
                                    egui::Label::new(format!("{} B", item_bytes(result, item))),
                                );
                                ui.add_sized(
                                    [145.0, 26.0],
                                    egui::Label::new(crate::local_date::display(item_modified(
                                        result, item,
                                    ))),
                                );
                            });
                        }
                    });
                }
            },
        );
    });
    if let Some(folder) = toggle {
        explorer.toggle_folder(result, folder);
    }
    if analyze {
        view.analyze(result, explorer);
    }
    if let Some(item) = selection {
        explorer.select(item);
    }
    if let Some(folder) = destination {
        explorer.open_folder(result, folder);
        view.whole_scan = false;
        if result_navigation {
            view.input = SearchInput::default();
        }
    }
    view.refresh(result, explorer.folder());
    if matches!(view.job.status(), crate::search_job::SearchStatus::Running) {
        ui.ctx().request_repaint_after(Duration::from_millis(30));
    }
    ui.separator();
    let selected = view
        .job
        .results()
        .and_then(|results| view.selection.single(&results.items));
    match selected {
        Some(Item::Folder(folder)) => {
            ui.label(format!(
                "선택 폴더: {}",
                result.folders[folder].path.display()
            ));
        }
        Some(Item::File(id)) => {
            let file = &result.files[id];
            let path = result.folders[file.parent].path.join(&file.name);
            ui.label(format!("선택 파일: {}", path.display()));
            ui.label(format!("{} Byte", file.logical_bytes));
            if file.modified.is_none() {
                ui.weak("수정 시간을 조회하지 못했습니다");
            }
        }
        None => {
            ui.label(format!("선택 항목 {}개", view.selection.count()));
        }
    }
    open_request
}

impl eframe::App for DiskWeaveApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.file_open.poll();
        if self.scan.poll() {
            self.search.reset();
            self.explorer = Some(Explorer::new(
                &self.scan.completed().expect("완료 결과").result,
            ));
        }
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
        if self.scan.is_running() || matches!(self.file_open.status(), OpenStatus::Running) {
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
                match self.file_open.status() {
                    OpenStatus::Idle => {},
                    OpenStatus::Running => { ui.label("Windows에서 열기 요청 처리 중…"); },
                    OpenStatus::Submitted => { ui.label("Windows에 열기 요청을 전달했습니다"); },
                    OpenStatus::Failed(error) => { ui.colored_label(egui::Color32::LIGHT_RED, format!("열기 실패: {error}")); },
                }

                ui.separator();
                if let Some(completed) = self.scan.completed() {
                    ui.label(format!("분석한 경로: {}", completed.path.display()));
                    let root = &completed.result.folders[0];
                    ui.label(format!(
                        "전체 {} Byte · 파일 {}개 · 폴더 {}개 · {:.2}초 · 오류 {}건 · Reparse 제외 {}개",
                        root.logical_bytes, root.file_count, root.folder_count,
                        completed.elapsed.as_secs_f64(), completed.result.error_count,
                        completed.result.skipped_reparse_points
                    ));
                    if completed.result.error_count > 0 {
                        ui.colored_label(egui::Color32::YELLOW, "접근 오류로 결과 일부가 누락됐습니다");
                    }
                    if let Some(explorer) = &mut self.explorer {
                        if let Some(request) = show_explorer(ui, &completed.result, explorer, &mut self.search) {
                            self.input_error = self.file_open.start(request).err().map(|error| error.to_string());
                        }
                    }
                } else {
                    ui.label("분석할 폴더를 선택하고 스캔을 시작하세요");
                }
            });
    }
}
