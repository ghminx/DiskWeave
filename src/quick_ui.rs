use crate::{
    explorer::Item,
    file_open::{OpenAction, OpenRequest, TargetKind},
    locations::Locations,
    presentation::SizeDisplay,
    quick_search::{Hit, QuickSearch, Scope},
};
use eframe::egui;
pub enum Action {
    ShowMain,
    Settings,
    Open(OpenRequest),
    Analyze((u64, u64, Item)),
    AnalyzePath(Hit),
    ScanFolder(Hit),
    Copy(std::path::PathBuf),
    Recycle(Hit),
}

pub enum InlineAction {
    Scope(crate::search_view::ScopeChoice),
    Close,
    Result(Action),
}

#[derive(Clone, Copy)]
enum RowAction {
    Open,
    Reveal,
    Copy,
    Analyze,
    Recycle,
}
#[derive(Default)]
pub struct QuickUi {
    pub error: Option<String>,
    pub opening: bool,
    pub focused: bool,
    seen_focus: bool,
    scroll: Option<usize>,
    scan_confirmation: Option<Hit>,
}
pub struct Layout {
    pub width: f32,
    pub height: f32,
    pub row: f32,
}
impl Layout {
    pub fn new(width: f32, height: f32) -> Self {
        Self {
            width: width.clamp(480.0, 1000.0),
            height: height.clamp(320.0, 800.0),
            row: 36.0,
        }
    }
}
impl QuickUi {
    pub fn confirm_folder_scan(&mut self, hit: Hit) {
        self.scan_confirmation = Some(hit);
    }

    fn folder_field(&mut self, ui: &mut egui::Ui, quick: &mut QuickSearch) {
        let mut path = quick.folder().to_string_lossy().into_owned();
        let width = (ui.available_width() - 92.0).max(80.0);
        let folder_input = ui.add(
            crate::ui_style::text_field(&mut path, width).hint_text("폴더를 끌어놓거나 경로 입력"),
        );
        if folder_input.changed() {
            quick.set_folder(path.into());
            self.error = None;
        }
        if folder_input.has_focus() || folder_input.lost_focus() {
            ui.input_mut(|input| input.consume_key(egui::Modifiers::NONE, egui::Key::Enter));
        }
        if ui.button("폴더 선택").clicked() {
            let mut dialog = rfd::FileDialog::new();
            if quick.folder().is_absolute() {
                dialog = dialog.set_directory(quick.folder());
            }
            if let Some(path) = dialog.pick_folder() {
                quick.set_folder(path);
                self.error = None;
            }
            quick.focus = true;
        }
    }

    fn matching_fields(&mut self, ui: &mut egui::Ui, quick: &mut QuickSearch) {
        ui.horizontal(|ui| {
            use crate::search::{EntryKind, SearchMode};
            for (kind, label) in [
                (EntryKind::All, "모두"),
                (EntryKind::Files, "파일"),
                (EntryKind::Folders, "폴더"),
            ] {
                if ui.selectable_label(quick.kind() == kind, label).clicked() {
                    quick.set_kind(kind);
                    self.error = None;
                }
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                egui::ComboBox::from_id_salt("quick_mode")
                    .selected_text(if quick.mode == SearchMode::Name {
                        "이름"
                    } else {
                        "전체 경로"
                    })
                    .show_ui(ui, |ui| {
                        let name = ui.selectable_value(&mut quick.mode, SearchMode::Name, "이름");
                        let path =
                            ui.selectable_value(&mut quick.mode, SearchMode::Path, "전체 경로");
                        if name.changed() || path.changed() {
                            self.error = None;
                        }
                    });
            });
        });
    }

    fn folder_confirmation(
        &mut self,
        ctx: &egui::Context,
        escape: bool,
        id: &'static str,
    ) -> Option<Action> {
        if let Some(hit) = self.scan_confirmation.as_ref() {
            let mut start = false;
            let mut cancel = escape;
            egui::Modal::new(egui::Id::unique(id)).show(ctx, |ui| {
                ui.set_width(360.0);
                ui.heading(if hit.kind == TargetKind::Folder {
                    "이 폴더 분석"
                } else {
                    "이 파일이 있는 폴더 분석"
                });
                if let Some(folder) = hit.analysis_folder() {
                    ui.add(egui::Label::new(folder.display().to_string()).wrap());
                }
                ui.horizontal(|ui| {
                    start = ui.button("분석 시작").clicked();
                    cancel |= ui.button("취소").clicked();
                });
            });
            if cancel {
                self.scan_confirmation = None;
            } else if start {
                return self.scan_confirmation.take().map(Action::ScanFolder);
            }
        }
        None
    }

    pub fn close_inline(&mut self, quick: &mut QuickSearch) {
        quick.hide();
        quick.live.clear();
        self.scan_confirmation = None;
        self.error = None;
        self.scroll = None;
    }

    pub fn show_inline(
        &mut self,
        ui: &mut egui::Ui,
        quick: &mut QuickSearch,
        locations: &Locations,
        interactive: bool,
    ) -> Option<InlineAction> {
        use crate::search_view::{scope_picker, ScopeChoice};
        let interactive = interactive && self.scan_confirmation.is_none();
        let mut enter = false;
        let scope = ui
            .add_enabled_ui(interactive, |ui| {
                ui.horizontal(|ui| {
                    let width = 170.0_f32.min(ui.available_width() * 0.55);
                    let response = ui.add(
                        crate::ui_style::text_field(
                            &mut quick.text,
                            (ui.available_width() - width - ui.spacing().item_spacing.x).max(70.0),
                        )
                        .id(egui::Id::unique("search_query"))
                        .hint_text("파일·폴더 검색"),
                    );
                    if quick.focus {
                        response.request_focus();
                        quick.focus = false;
                    }
                    if response.changed() {
                        self.error = None;
                    }
                    enter = (response.has_focus() || response.lost_focus())
                        && ui.input_mut(|input| {
                            input.consume_key(egui::Modifiers::NONE, egui::Key::Enter)
                        });
                    scope_picker(ui, ScopeChoice::Direct(quick.scope()), width, true)
                })
                .inner
            })
            .inner;
        if let Some(scope) = scope {
            return Some(InlineAction::Scope(scope));
        }

        if quick.scope() == Scope::Folder {
            ui.add_enabled_ui(interactive, |ui| {
                ui.horizontal(|ui| self.folder_field(ui, quick))
            });
        }
        ui.add_enabled_ui(interactive, |ui| self.matching_fields(ui, quick));
        quick.invalidate_live();
        let (start, close) = ui
            .add_enabled_ui(interactive, |ui| {
                ui.horizontal(|ui| {
                    let ready = !quick.live.running() && !quick.text.trim().is_empty();
                    let start = ui
                        .add_enabled(
                            ready,
                            egui::Button::new("검색").fill(crate::ui_style::SELECTED),
                        )
                        .clicked()
                        || ready && enter;
                    (start, ui.button("검색 닫기").clicked())
                })
            })
            .inner
            .inner;
        if close {
            return Some(InlineAction::Close);
        }
        if start {
            self.error = quick.start_live().err().map(|error| error.to_string());
        }
        live_status(ui, quick);
        if let Some(error) = &self.error {
            ui.add(
                egui::Label::new(egui::RichText::new(error).color(egui::Color32::LIGHT_RED)).wrap(),
            );
        }
        if interactive && quick.count() > 0 {
            let direction = ui.input_mut(|i| {
                if i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowDown) {
                    1
                } else if i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowUp) {
                    -1
                } else {
                    0
                }
            });
            if direction != 0 {
                quick.step(direction);
                self.scroll = quick.selection;
            }
        }
        let allow_enter = interactive
            && !start
            && !enter
            && !ui.ctx().egui_wants_keyboard_input()
            && !egui::Popup::is_any_open(ui.ctx());
        if quick.count() > 0 {
            let row = ui
                .add_enabled_ui(interactive, |ui| self.draw_results(ui, quick, allow_enter))
                .inner;
            if let Some(row) = row {
                match result_action(quick, locations, row) {
                    Ok(action) => return Some(InlineAction::Result(action)),
                    Err(error) => self.error = Some(error.to_string()),
                }
            }
        }
        let escape = ui.input(|i| i.key_pressed(egui::Key::Escape));
        self.folder_confirmation(ui.ctx(), escape, "inline_folder_analysis")
            .map(InlineAction::Result)
    }

    fn draw_search_fields(&mut self, ui: &mut egui::Ui, quick: &mut QuickSearch) -> bool {
        let gap = if ui.available_height() < 300.0 {
            1.0
        } else {
            6.0
        };
        ui.spacing_mut().interact_size.y = crate::ui_style::FIELD_HEIGHT;
        ui.horizontal(|ui| {
            let response = crate::ui_style::scope_button(ui, quick.scope().label(), 118.0);
            egui::Popup::menu(&response).show(|ui| {
                crate::ui_style::menu_spacing(ui);
                for scope in [Scope::Scanned, Scope::Folder, Scope::Computer] {
                    if ui
                        .selectable_label(quick.scope() == scope, scope.label())
                        .clicked()
                    {
                        quick.set_scope(scope);
                        self.error = None;
                        ui.close();
                    }
                }
            });
            if quick.scope() == Scope::Folder {
                self.folder_field(ui, quick);
            }
        });
        ui.add_space(gap);
        self.matching_fields(ui, quick);
        ui.add_space(gap);
        let started = ui
            .horizontal(|ui| {
                let manual = quick.scope() != Scope::Scanned;
                let width = ui.available_width() - if manual { 76.0 } else { 0.0 };
                let input = ui.add(
                    crate::ui_style::text_field(&mut quick.text, width)
                        .hint_text("파일 또는 폴더 찾기"),
                );
                if input.changed() {
                    self.error = None;
                }
                if quick.focus {
                    input.request_focus();
                    quick.focus = false;
                }
                if manual {
                    let enter = input.lost_focus()
                        && ui.input_mut(|input| {
                            input.consume_key(egui::Modifiers::NONE, egui::Key::Enter)
                        });
                    let ready = !quick.live.running() && !quick.text.trim().is_empty();
                    ui.add_enabled(
                        ready,
                        egui::Button::new("검색")
                            .fill(crate::ui_style::SELECTED)
                            .min_size(egui::vec2(62.0, crate::ui_style::FIELD_HEIGHT)),
                    )
                    .clicked()
                        || (ready && enter)
                } else {
                    false
                }
            })
            .inner;
        ui.add_space(gap);
        started
    }

    fn draw_results(
        &mut self,
        ui: &mut egui::Ui,
        quick: &mut QuickSearch,
        allow_enter: bool,
    ) -> Option<RowAction> {
        let count = quick.count();
        let scanned = quick.scope() == Scope::Scanned;
        let row_height = Layout::new(ui.available_width(), ui.available_height()).row;
        let (header, _) =
            ui.allocate_exact_size(egui::vec2(ui.available_width(), 29.0), egui::Sense::hover());
        ui.painter()
            .rect_filled(header, 4.0, crate::ui_style::SURFACE);
        let [name, kind, path, bytes] = result_columns(header, scanned);
        for (rect, title, right) in [
            (name, "이름", false),
            (kind, "종류", false),
            (path, "전체 경로", false),
            (bytes, "크기", true),
        ] {
            if rect.width() > 0.0 {
                crate::ui_style::text(ui, rect, title, right, crate::ui_style::MUTED, 13.0);
            }
        }
        let mut scroll = egui::ScrollArea::vertical()
            .id_salt("quick_results")
            .auto_shrink([false, false])
            .max_height((ui.available_height() - 28.0).max(0.0));
        if let Some(row) = self.scroll.take() {
            scroll = scroll
                .vertical_scroll_offset(row as f32 * (row_height + ui.spacing().item_spacing.y));
        }
        let mut pick = None;
        let mut action = None;
        scroll.show_rows(ui, row_height, count, |ui, range| {
            for row in range {
                let Some(hit) = quick.row(row) else { continue };
                let folder = hit.kind == TargetKind::Folder;
                let name = hit
                    .path
                    .file_name()
                    .unwrap_or(hit.path.as_os_str())
                    .to_string_lossy();
                let size = hit.bytes.map(SizeDisplay::new);
                let description = if folder { "폴더" } else { "파일" };
                let (rect, response) = ui.allocate_exact_size(
                    egui::vec2(ui.available_width(), row_height),
                    egui::Sense::click(),
                );
                if quick.selection == Some(row) || response.hovered() {
                    ui.painter()
                        .rect_filled(rect, 5.0, crate::ui_style::SELECTED);
                }
                let [name_rect, kind_rect, path_rect, bytes_rect] = result_columns(rect, scanned);
                let icon_rect = egui::Rect::from_center_size(
                    egui::pos2(rect.left() + 18.0, rect.center().y),
                    egui::vec2(18.0, 18.0),
                );
                crate::ui_style::icon(ui, icon_rect, folder);
                crate::ui_style::text(ui, name_rect, &name, false, crate::ui_style::TEXT, 15.0);
                crate::ui_style::text(
                    ui,
                    kind_rect,
                    description,
                    false,
                    crate::ui_style::MUTED,
                    13.0,
                );
                crate::ui_style::text(
                    ui,
                    path_rect,
                    &hit.path.display().to_string(),
                    false,
                    crate::ui_style::MUTED,
                    13.0,
                );
                if scanned {
                    if let Some(size) = &size {
                        crate::ui_style::text(
                            ui,
                            bytes_rect,
                            &size.text,
                            true,
                            crate::ui_style::MUTED,
                            13.0,
                        );
                    }
                }
                response.widget_info(|| {
                    egui::WidgetInfo::selected(
                        egui::Role::Button,
                        true,
                        quick.selection == Some(row),
                        format!("{name}, {description}, {}", hit.path.display()),
                    )
                });
                let response = response.on_hover_text(match size {
                    Some(size) => format!("{}\n{}", hit.path.display(), size.exact),
                    None => hit.path.display().to_string(),
                });
                if response.clicked() || response.secondary_clicked() {
                    pick = Some(row);
                }
                if response.double_clicked() {
                    pick = Some(row);
                    action = Some(RowAction::Open);
                }
                response.context_menu(|ui| {
                    crate::ui_style::menu_spacing(ui);
                    let mut choose = |choice, clicked| {
                        if clicked {
                            pick = Some(row);
                            action = Some(choice);
                        }
                        clicked
                    };
                    let mut chosen = choose(
                        RowAction::Open,
                        ui.button(if folder {
                            "탐색기에서 열기"
                        } else {
                            "파일 열기"
                        })
                        .clicked(),
                    );
                    if !folder {
                        chosen |= choose(RowAction::Reveal, ui.button("파일 위치 열기").clicked());
                    }
                    chosen |= choose(RowAction::Copy, ui.button("경로 복사").clicked());
                    chosen |= choose(
                        RowAction::Analyze,
                        ui.button(if scanned {
                            "분석에서 보기"
                        } else {
                            "용량 분석…"
                        })
                        .clicked(),
                    );
                    ui.separator();
                    let recycle = ui
                        .add_enabled(
                            hit.identity.is_some(),
                            egui::Button::new(
                                egui::RichText::new("휴지통으로 이동…")
                                    .color(egui::Color32::LIGHT_RED),
                            ),
                        )
                        .on_disabled_hover_text(
                            "검색 결과에서 파일 또는 폴더 ID를 확인할 수 없습니다.",
                        );
                    chosen |= choose(RowAction::Recycle, recycle.clicked());
                    if chosen {
                        ui.close();
                    }
                });
            }
        });
        if let Some(row) = pick {
            quick.selection = Some(row);
        }
        if allow_enter && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
            quick.selection.get_or_insert(0);
            let ctrl = ui.input(|i| i.modifiers.ctrl);
            action = Some(if ctrl {
                RowAction::Analyze
            } else {
                RowAction::Open
            });
        }
        action
    }
    pub fn show(
        &mut self,
        ctx: &egui::Context,
        quick: &mut QuickSearch,
        locations: &Locations,
        keep_alive: bool,
    ) -> Option<Action> {
        if !quick.visible {
            self.seen_focus = false;
            self.scan_confirmation = None;
            if !keep_alive {
                return None;
            }
        }
        let mut action = None;
        let initial = Layout::new(720.0, 460.0);
        let shown = quick.visible;
        let window_level = if shown && quick.scope() != Scope::Scanned {
            egui::WindowLevel::AlwaysOnTop
        } else {
            egui::WindowLevel::Normal
        };
        let draw = |root: &mut egui::Ui, _: egui::ViewportClass| {
            if !quick.visible {
                return;
            }
            let ctx = root.ctx().clone();
            if quick.focus {
                self.seen_focus = false;
                ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
            }
            let focused = ctx.input(|i| i.viewport().focused.unwrap_or(false));
            self.focused = focused;
            if focused {
                self.seen_focus = true;
            }
            let escape = ctx.input(|i| i.key_pressed(egui::Key::Escape));
            if ctx.input(|i| i.viewport().close_requested())
                || escape && self.scan_confirmation.is_none()
                || self.seen_focus && !focused && quick.scope() == Scope::Scanned
            {
                ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                quick.hide();
                self.seen_focus = false;
                ctx.request_repaint();
                return;
            }
            let interactive = self.scan_confirmation.is_none();
            egui::CentralPanel::default()
                .frame(
                    egui::Frame::new()
                        .fill(crate::ui_style::BG)
                        .inner_margin(16),
                )
                .show(root, |ui| {
                    ui.horizontal(|ui| {
                        ui.heading("빠른 검색");
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.button("설정").clicked() {
                                action = Some(Action::Settings);
                            }
                            if ui.button("메인 창").clicked() {
                                action = Some(Action::ShowMain);
                            }
                        });
                    });
                    ui.add_space(4.0);
                    if interactive {
                        let direction = ui.input_mut(|i| {
                            if i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowDown) {
                                1
                            } else if i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowUp) {
                                -1
                            } else {
                                0
                            }
                        });
                        if direction != 0 {
                            quick.step(direction);
                            self.scroll = quick.selection;
                        }
                    }
                    if interactive {
                        let dropped = ctx.input(|i| {
                            i.raw
                                .dropped_files
                                .iter()
                                .map(|file| file.path().to_path_buf())
                                .collect::<Vec<_>>()
                        });
                        if !dropped.is_empty() {
                            self.error = quick.drop_folder(&dropped).err().map(|e| e.to_string());
                        }
                    }
                    let start_search = ui
                        .add_enabled_ui(interactive, |ui| self.draw_search_fields(ui, quick))
                        .inner;
                    quick.invalidate_live();
                    if start_search {
                        self.error = quick.start_live().err().map(|e| e.to_string());
                    }

                    let scanned = quick.scope() == Scope::Scanned;
                    let current = if scanned {
                        scanned_status(ui, quick, locations)
                    } else {
                        live_status(ui, quick);
                        true
                    };
                    if let Some(error) = &self.error {
                        ui.add(
                            egui::Label::new(
                                egui::RichText::new(error).color(egui::Color32::LIGHT_RED),
                            )
                            .truncate(),
                        )
                        .on_hover_text(error);
                    }
                    if current && quick.count() > 0 {
                        let menu = ui
                            .add_enabled_ui(interactive, |ui| {
                                self.draw_results(ui, quick, interactive && !start_search)
                            })
                            .inner;
                        if let Some(menu) = menu {
                            match result_action(quick, locations, menu) {
                                Ok(next) => action = Some(next),
                                Err(e) => self.error = Some(e.to_string()),
                            }
                        }
                    }

                    ui.with_layout(egui::Layout::bottom_up(egui::Align::LEFT), |ui| {
                        if scanned {
                            ui.small("↑↓ 선택   Enter 열기   Ctrl+Enter 분석에서 보기   Esc 닫기");
                        } else if quick.count() > 0 {
                            ui.small("Ctrl+Enter 용량 분석");
                        }
                    });
                });
            if let Some(next) = self.folder_confirmation(&ctx, escape, "quick_folder_analysis") {
                action = Some(next);
            }
        };
        ctx.show_viewport_immediate(
            egui::ViewportId::from_hash_of("quick_search"),
            egui::ViewportBuilder::default()
                .with_title("DiskWeave · 빠른 검색")
                .with_visible(shown)
                .with_active(false)
                .with_drag_and_drop(true)
                .with_window_level(window_level)
                .with_inner_size([initial.width, initial.height])
                .with_min_inner_size([480.0, 320.0]),
            draw,
        );
        action
    }
}

fn result_columns(row: egui::Rect, scanned: bool) -> [egui::Rect; 4] {
    let name = row.left() + 34.0;
    let kind = name + (row.width() * 0.28).max(92.0);
    let path = kind + 55.0;
    let end = row.right() - 8.0;
    let bytes = end - if scanned { 76.0 } else { 0.0 };
    let cell = |start, end| {
        egui::Rect::from_min_max(egui::pos2(start, row.top()), egui::pos2(end, row.bottom()))
    };
    [
        cell(name, kind - 8.0),
        cell(kind, path - 8.0),
        cell(path, bytes - 8.0),
        cell(bytes, end),
    ]
}

fn scanned_status(ui: &mut egui::Ui, quick: &QuickSearch, locations: &Locations) -> bool {
    let available = locations
        .entries
        .iter()
        .filter(|entry| !entry.stale && entry.scan.completed().is_some())
        .count();
    let skipped = locations.entries.len() - available;
    let current = available > 0
        && !quick.text.trim().is_empty()
        && quick.scanned_request_current(locations.epoch);
    let error = current.then_some(quick.job.error.as_deref()).flatten();
    let (mut status, busy) = if available == 0 {
        (
            "스캔한 위치 없음 · 폴더 지정 또는 내 PC 전체 선택".to_owned(),
            false,
        )
    } else if quick.text.trim().is_empty() {
        ("입력하면 스캔한 위치에서 검색".to_owned(), false)
    } else if !current {
        ("검색 준비 중…".to_owned(), true)
    } else if quick.job.running {
        ("검색 중…".to_owned(), true)
    } else if let Some(error) = error {
        (format!("검색 실패 · {error}"), false)
    } else if quick.job.results.is_some() {
        (format!("검색 완료 · {}건", quick.count()), false)
    } else {
        ("검색 대기 중".to_owned(), false)
    };
    if skipped > 0 {
        status.push_str(&format!(" · 스캔 전/갱신 필요 {skipped}곳 제외"));
    }
    if available > 0 && locations.is_running() {
        status.push_str(" · 이전 완료 결과 포함");
    }
    ui.horizontal(|ui| {
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if current {
                elapsed_label(ui, quick.elapsed(std::time::Instant::now()), busy);
            }
            ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                status_label(ui, &status, busy, error.is_some(), None);
            });
        });
    });
    current
}

fn live_status(ui: &mut egui::Ui, quick: &mut QuickSearch) {
    let scope = quick.scope();
    let count = quick.count();
    let elapsed = quick.elapsed(std::time::Instant::now());
    let live = &mut quick.live;
    let running = live.running();
    let mut status = if live.input.is_none() {
        if running {
            "이전 검색 중지 중…".to_owned()
        } else if scope == Scope::Computer {
            "검색을 누르면 로컬 고정 드라이브 탐색 시작".to_owned()
        } else {
            "검색을 누르면 지정한 폴더 탐색 시작".to_owned()
        }
    } else if running && live.stopped {
        format!("검색 중지 중 · {count}건 발견")
    } else if running {
        format!("검색 중 · {count}건 발견")
    } else if live.stopped {
        format!("검색 중지됨 · {count}건")
    } else if let Some(error) = &live.error {
        format!("검색 실패 · {error}")
    } else if live.coverage.limited {
        format!("표시 한도 도달 · {count}건 · 범위를 좁혀 주세요")
    } else if live.coverage.skipped > 0 {
        format!("검색 완료 · {count}건 · 일부 제외")
    } else {
        format!("검색 완료 · {count}건")
    };
    if live.coverage.skipped > 0 {
        status.push_str(&format!(
            " · 접근 불가·특수 경로 {}건",
            live.coverage.skipped
        ));
    }
    ui.horizontal(|ui| {
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if running && !live.stopped && live.input.is_some() && ui.button("중지").clicked() {
                live.stop();
            }
            if live.input.is_some() {
                elapsed_label(ui, elapsed, running);
            }
            ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                status_label(
                    ui,
                    &status,
                    running,
                    live.error.is_some(),
                    live.coverage.first_issue.as_deref(),
                );
            });
        });
    });
}

fn elapsed_label(ui: &mut egui::Ui, elapsed: Option<std::time::Duration>, running: bool) {
    if let Some(elapsed) = elapsed {
        let label = if !running && elapsed < std::time::Duration::from_millis(100) {
            "경과 0.1초 미만".to_owned()
        } else {
            format!("경과 {:.1}초", elapsed.as_secs_f64())
        };
        ui.small(label);
    }
}

fn status_label(ui: &mut egui::Ui, status: &str, busy: bool, failed: bool, detail: Option<&str>) {
    if busy {
        crate::ui_style::activity_indicator(ui);
    }
    let text = if failed {
        egui::RichText::new(status).color(egui::Color32::LIGHT_RED)
    } else {
        egui::RichText::new(status)
    };
    let response = ui.add(egui::Label::new(text).truncate());
    if let Some(detail) = detail {
        response.on_hover_text(format!("{status}\n{detail}"));
    } else {
        response.on_hover_text(status);
    }
}

fn result_action(
    quick: &QuickSearch,
    locations: &Locations,
    action: RowAction,
) -> std::io::Result<Action> {
    if matches!(action, RowAction::Analyze) {
        return if quick.scope() == Scope::Scanned {
            quick.selected(locations).map(Action::Analyze)
        } else {
            quick.selected_hit(locations).map(Action::AnalyzePath)
        };
    }
    let hit = quick.selected_hit(locations)?;
    Ok(match action {
        RowAction::Open | RowAction::Reveal => {
            let opening = match hit.kind {
                TargetKind::Folder => OpenAction::OpenFolder,
                TargetKind::File if matches!(action, RowAction::Reveal) => OpenAction::RevealFile,
                TargetKind::File => OpenAction::OpenFile,
            };
            Action::Open(OpenRequest::new(hit.path, hit.kind, opening))
        }
        RowAction::Copy => Action::Copy(hit.path),
        RowAction::Recycle => Action::Recycle(hit),
        RowAction::Analyze => unreachable!(),
    })
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        path::{Path, PathBuf},
        sync::Arc,
        time::{Duration, Instant},
    };

    #[derive(Debug)]
    struct FolderDrop(PathBuf);

    impl egui::DroppedFile for FolderDrop {
        fn path(&self) -> &Path {
            &self.0
        }

        fn bytes(&self) -> Result<Vec<u8>, String> {
            panic!("폴더 검색에서는 드롭한 파일 내용을 읽지 않음");
        }
    }

    #[test]
    fn folder_search_accepts_explorer_drop_without_focus_and_renders_at_minimum_width() {
        let path = std::env::temp_dir().join(format!("diskweave-quick-ui-{}", std::process::id()));
        fs::create_dir_all(path.join("sample-folder")).unwrap();
        fs::write(path.join("sample.txt"), b"example").unwrap();
        let ctx = egui::Context::default();
        ctx.set_embed_viewports(true);
        crate::ui_style::apply(&ctx);
        let mut ui = QuickUi::default();
        let mut quick = QuickSearch::default();
        let locations = Locations::default();
        quick.set_scope(Scope::Folder);
        quick.text = "sample".into();
        quick.open();
        let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(480.0, 320.0));
        let mut render = |quick: &mut QuickSearch, focused, drop| {
            let mut input = egui::RawInput {
                screen_rect: Some(screen),
                ..Default::default()
            };
            input
                .viewports
                .get_mut(&egui::ViewportId::ROOT)
                .unwrap()
                .focused = Some(focused);
            if drop {
                input.dropped_files.push(Arc::new(FolderDrop(path.clone())));
            }
            let mut output = ctx.run_ui(input, |root| {
                ui.show(root.ctx(), quick, &locations, false);
            });
            output.textures_delta.clear();
            output
        };
        render(&mut quick, true, false);
        render(&mut quick, false, true);
        assert!(quick.visible);
        assert_eq!(quick.folder(), path);
        assert_eq!(quick.text, "sample");

        quick.start_live().unwrap();
        let until = Instant::now() + Duration::from_secs(5);
        while quick.pending() {
            quick.tick(&locations, Instant::now());
            assert!(Instant::now() < until);
            std::thread::yield_now();
        }
        render(&mut quick, false, false);
        let output = render(&mut quick, false, false);
        let visible: Vec<_> = output
            .shapes
            .iter()
            .filter_map(|shape| match &shape.shape {
                egui::Shape::Text(draw)
                    if screen.contains_rect(draw.galley.rect.translate(draw.pos.to_vec2())) =>
                {
                    Some(draw.galley.text())
                }
                _ => None,
            })
            .collect();
        for label in [
            "sample.txt",
            "sample-folder",
            "검색 완료 · 2건",
            "전체 경로",
        ] {
            assert!(
                visible.contains(&label),
                "화면 안에 {label} 필요: {visible:?}"
            );
        }
        let label_y = |label| {
            output
                .shapes
                .iter()
                .find_map(|shape| match &shape.shape {
                    egui::Shape::Text(draw) if draw.galley.text() == label => Some(draw.pos.y),
                    _ => None,
                })
                .expect("검색 상태와 결과 머리글 필요")
        };
        assert!(label_y("검색 완료 · 2건") < label_y("전체 경로"));
        quick.step(1);
        let hit = quick.selected_hit(&locations).unwrap();
        let Action::Open(request) = result_action(&quick, &locations, RowAction::Open).unwrap()
        else {
            panic!("결과 열기 요청 필요");
        };
        let mut opened = false;
        crate::file_open::open_with(request, |request| {
            assert_eq!(request.path(), hit.path);
            opened = true;
            Ok(())
        })
        .unwrap();
        assert!(opened);
        let mut draw_key = |ui: &mut QuickUi, key: Option<(egui::Key, egui::Modifiers)>| {
            let mut input = egui::RawInput {
                screen_rect: Some(screen),
                ..Default::default()
            };
            if let Some((key, modifiers)) = key {
                input.events.push(egui::Event::ModifiersChanged(modifiers));
                input.events.push(egui::Event::Key {
                    key,
                    physical_key: None,
                    pressed: true,
                    repeat: false,
                    modifiers,
                });
            }
            let mut action = None;
            let mut frame = ctx.run_ui(input, |root| {
                action = ui.show(root.ctx(), &mut quick, &locations, false);
            });
            frame.textures_delta.clear();
            (action, frame)
        };
        let (action, _) = draw_key(&mut ui, Some((egui::Key::Enter, egui::Modifiers::CTRL)));
        let Some(Action::AnalyzePath(match_hit)) = action else {
            panic!("Ctrl+Enter로 직접 검색 항목 분석 요청 필요");
        };
        assert_eq!(match_hit.path, hit.path);
        ui.confirm_folder_scan(match_hit);
        draw_key(&mut ui, None);
        let (action, frame) = draw_key(&mut ui, None);
        assert!(action.is_none());
        assert!(frame
            .shapes
            .iter()
            .any(|shape| matches!(&shape.shape, egui::Shape::Text(draw)
            if draw.galley.text() == "분석 시작")));
        let (action, _) = draw_key(&mut ui, Some((egui::Key::Escape, egui::Modifiers::NONE)));
        assert!(action.is_none() && quick.visible);
        fs::remove_dir_all(path).unwrap();
    }
    #[test]
    fn status_at_minimum_size_distinguishes_scope_progress_outcome_and_new_input() {
        let fixture = crate::duplicates::tests::Fixture::new();
        let search_root = fixture.0.join("search");
        let outside = fixture.0.join("outside");
        fs::create_dir(&search_root).unwrap();
        fs::create_dir(&outside).unwrap();
        fs::write(search_root.join("sample.txt"), b"example").unwrap();
        let ctx = egui::Context::default();
        ctx.set_embed_viewports(true);
        crate::ui_style::apply(&ctx);
        let mut ui = QuickUi::default();
        let mut quick = QuickSearch::default();
        let mut locations = Locations::default();
        quick.open();
        let mut render = |quick: &mut QuickSearch, locations: &Locations| {
            let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(480.0, 320.0));
            let mut raw = egui::RawInput {
                screen_rect: Some(screen),
                ..Default::default()
            };
            raw.viewports
                .get_mut(&egui::ViewportId::ROOT)
                .unwrap()
                .focused = Some(true);
            let mut frame = ctx.run_ui(raw, |root| {
                ui.show(root.ctx(), quick, locations, false);
            });
            frame.textures_delta.clear();
            frame
                .shapes
                .iter()
                .filter_map(|shape| match &shape.shape {
                    egui::Shape::Text(text)
                        if screen.contains_rect(text.visual_bounding_rect()) =>
                    {
                        Some(text.galley.text().to_owned())
                    }
                    _ => None,
                })
                .collect::<Vec<_>>()
        };
        let label = |labels: Vec<String>, expected: &str| {
            assert!(
                labels.iter().any(|label| label.starts_with(expected)),
                "{expected:?}: {labels:?}"
            );
        };
        let finish = |quick: &mut QuickSearch, locations: &Locations| {
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                let now = Instant::now();
                quick.tick(locations, now + Duration::from_millis(200));
                if !quick.pending() {
                    break;
                }
                assert!(now < deadline);
                std::thread::yield_now();
            }
        };

        render(&mut quick, &locations);
        label(render(&mut quick, &locations), "스캔한 위치 없음");
        quick.set_scope(Scope::Computer);
        label(
            render(&mut quick, &locations),
            "검색을 누르면 로컬 고정 드라이브 탐색 시작",
        );
        quick.set_folder(search_root.clone());
        quick.text = "sample".into();
        label(
            render(&mut quick, &locations),
            "검색을 누르면 지정한 폴더 탐색 시작",
        );
        quick.start_live().unwrap();
        let active = render(&mut quick, &locations);
        assert!(active.iter().any(|label| label == "중지"));
        label(active.clone(), "경과 ");
        label(active, "검색 중 · 0건 발견");
        quick.live.stop();
        finish(&mut quick, &locations);
        let stopped = render(&mut quick, &locations);
        label(stopped.clone(), "경과 ");
        label(stopped, "검색 중지됨 · 0건");

        quick.text = "missing".into();
        quick.start_live().unwrap();
        finish(&mut quick, &locations);
        label(render(&mut quick, &locations), "검색 완료 · 0건");
        quick.set_folder(fixture.0.join("deleted-folder"));
        quick.start_live().unwrap();
        finish(&mut quick, &locations);
        label(render(&mut quick, &locations), "검색 실패");
        quick.set_scope(Scope::Computer);
        let switched = render(&mut quick, &locations);
        assert!(!switched.iter().any(|label| label.starts_with("검색 실패")));
        assert!(!switched.iter().any(|label| label.starts_with("경과 ")));
        label(switched, "검색을 누르면 로컬 고정 드라이브 탐색 시작");

        locations.start(search_root.clone()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while locations.is_running() {
            locations.poll();
            assert!(Instant::now() < deadline);
            std::thread::yield_now();
        }
        quick.set_scope(Scope::Scanned);
        quick.text.clear();
        label(
            render(&mut quick, &locations),
            "입력하면 스캔한 위치에서 검색",
        );
        quick.text = "sample".into();
        finish(&mut quick, &locations);
        let completed = render(&mut quick, &locations);
        let elapsed = completed
            .iter()
            .find(|label| label.starts_with("경과 "))
            .expect("완료한 검색의 경과 시간")
            .clone();
        label(completed, "검색 완료 · 1건");
        quick.tick(&locations, Instant::now() + Duration::from_secs(10));
        assert!(render(&mut quick, &locations).contains(&elapsed));
        quick.text = "new query".into();
        let pending = render(&mut quick, &locations);
        assert!(!pending.iter().any(|label| label == "sample.txt"
            || label.starts_with("검색 완료")
            || label.starts_with("경과 ")));
        label(pending, "검색 준비 중…");

        use std::os::windows::process::CommandExt;
        let junction = search_root.join("junction");
        let status = std::process::Command::new("cmd.exe")
            .args(["/d", "/c", "mklink", "/J"])
            .arg(&junction)
            .arg(&outside)
            .creation_flags(0x08000000)
            .output()
            .unwrap();
        assert!(status.status.success(), "{status:?}");
        quick.set_folder(search_root);
        quick.text = "sample".into();
        quick.start_live().unwrap();
        finish(&mut quick, &locations);
        let partial = render(&mut quick, &locations);
        fs::remove_dir(junction).unwrap();
        label(
            partial,
            "검색 완료 · 1건 · 일부 제외 · 접근 불가·특수 경로 1건",
        );
    }

    #[test]
    fn reopening_requests_focus_before_dismissing_for_old_focus_state() {
        let ctx = egui::Context::default();
        ctx.set_embed_viewports(true);
        let mut ui = QuickUi::default();
        let mut quick = QuickSearch::default();
        let locations = Locations::default();
        quick.open();
        for focused in [true, true] {
            let mut input = egui::RawInput::default();
            input
                .viewports
                .get_mut(&egui::ViewportId::ROOT)
                .unwrap()
                .focused = Some(focused);
            let mut output = ctx.run_ui(input, |root| {
                ui.show(root.ctx(), &mut quick, &locations, false);
            });
            output.textures_delta.clear();
        }
        quick.hide();
        quick.open();
        let mut input = egui::RawInput::default();
        input
            .viewports
            .get_mut(&egui::ViewportId::ROOT)
            .unwrap()
            .focused = Some(false);
        let mut output = ctx.run_ui(input, |root| {
            ui.show(root.ctx(), &mut quick, &locations, false);
        });
        output.textures_delta.clear();
        assert!(
            quick.visible,
            "재호출 시 이전 포커스 상태로 즉시 숨기지 않음"
        );
    }
    #[test]
    fn compact_search_layout_keeps_readable_rows() {
        for (w, h) in [
            (100.0, 100.0),
            (480.0, 320.0),
            (720.0, 460.0),
            (1400.0, 1000.0),
        ] {
            let l = Layout::new(w, h);
            assert!((480.0..=1000.0).contains(&l.width));
            assert!((320.0..=800.0).contains(&l.height));
            assert!(l.height >= l.row * 4.0);
        }
    }
}
