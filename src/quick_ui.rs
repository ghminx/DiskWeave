use crate::{
    explorer::Item,
    file_open::{OpenAction, OpenRequest, TargetKind},
    locations::Locations,
    presentation::SizeDisplay,
    quick_search::{Hit, QuickSearch, Scope},
    quick_settings::{Preferences, Shortcut},
};
use eframe::egui;
pub enum Action {
    ShowMain,
    Apply(Preferences),
    Open(OpenRequest),
    Analyze((u64, u64, Item)),
    Copy(std::path::PathBuf),
    Recycle(Hit),
}

#[derive(Clone, Copy)]
enum RowAction {
    Open,
    Reveal,
    Copy,
    Analyze,
    Recycle,
}
pub struct QuickUi {
    pub error: Option<String>,
    pub pending: bool,
    pub opening: bool,
    pub settings: bool,
    pub focused: bool,
    seen_focus: bool,
    draft: Preferences,
    key: String,
    scroll: Option<usize>,
}
impl Default for QuickUi {
    fn default() -> Self {
        Self {
            error: None,
            pending: false,
            opening: false,
            settings: false,
            focused: false,
            seen_focus: false,
            draft: Preferences::default(),
            key: "Space".into(),
            scroll: None,
        }
    }
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
    pub fn open_settings(&mut self, p: Preferences) {
        self.draft = p;
        self.key = p.shortcut.key_name();
        self.settings = true;
    }

    fn draw_search_fields(&mut self, ui: &mut egui::Ui, quick: &mut QuickSearch) -> bool {
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
                let mut path = quick.folder().to_string_lossy().into_owned();
                let width = (ui.available_width() - 92.0).max(80.0);
                let folder_input = ui.add(
                    crate::ui_style::text_field(&mut path, width)
                        .hint_text("폴더를 끌어놓거나 경로 입력"),
                );
                if folder_input.changed() {
                    quick.set_folder(path.into());
                    self.error = None;
                }
                if folder_input.has_focus() || folder_input.lost_focus() {
                    ui.input_mut(|input| {
                        input.consume_key(egui::Modifiers::NONE, egui::Key::Enter)
                    });
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
            } else if quick.scope() == Scope::Computer {
                ui.small("연결된 로컬 고정 드라이브에서 검색합니다.");
            }
        });
        ui.add_space(6.0);
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
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            use crate::search::{EntryKind, SearchMode};
            for (kind, label) in [
                (EntryKind::All, "모두"),
                (EntryKind::Files, "파일"),
                (EntryKind::Folders, "폴더"),
            ] {
                if ui.selectable_label(quick.kind() == kind, label).clicked() {
                    quick.set_kind(kind);
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
                        ui.selectable_value(&mut quick.mode, SearchMode::Name, "이름");
                        ui.selectable_value(&mut quick.mode, SearchMode::Path, "전체 경로");
                    });
            });
        });
        ui.add_space(8.0);
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
            .max_height((ui.available_height() - if scanned { 28.0 } else { 56.0 }).max(0.0));
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
                    if scanned {
                        chosen |= choose(RowAction::Analyze, ui.button("분석에서 보기").clicked());
                    }
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
        if allow_enter && !self.settings && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
            quick.selection.get_or_insert(0);
            let ctrl = ui.input(|i| i.modifiers.ctrl);
            if !ctrl || scanned {
                action = Some(if ctrl {
                    RowAction::Analyze
                } else {
                    RowAction::Open
                });
            }
        }
        action
    }
    pub fn show(
        &mut self,
        ctx: &egui::Context,
        quick: &mut QuickSearch,
        locations: &Locations,
        applied: Preferences,
        keep_alive: bool,
    ) -> Option<Action> {
        if !quick.visible {
            self.seen_focus = false;
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
            if ctx.input(|i| i.viewport().close_requested())
                || ctx.input(|i| i.key_pressed(egui::Key::Escape))
                || self.seen_focus && !focused && quick.scope() == Scope::Scanned
            {
                ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                quick.hide();
                self.seen_focus = false;
                ctx.request_repaint();
                return;
            }
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
                                self.open_settings(applied);
                            }
                            if ui.button("메인 창").clicked() {
                                action = Some(Action::ShowMain);
                            }
                        });
                    });
                    ui.add_space(8.0);
                    if !self.settings {
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
                    if !self.settings {
                        let dropped = ctx.input(|i| {
                            i.raw.dropped_files.iter().map(|file| file.path().to_path_buf()).collect::<Vec<_>>()
                        });
                        if !dropped.is_empty() {
                            self.error = quick.drop_folder(&dropped).err().map(|e| e.to_string());
                        }
                    }
                    let start_search = ui.add_enabled_ui(!self.settings, |ui| {
                        self.draw_search_fields(ui, quick)
                    }).inner;
                    quick.invalidate_live();
                    if start_search {
                        self.error = quick.start_live().err().map(|e| e.to_string());
                    }

                    let scanned = quick.scope() == Scope::Scanned;
                    if let Some(error) = self.error.as_ref().or(quick.job.error.as_ref()).or(quick.live.error.as_ref()) {
                        ui.colored_label(egui::Color32::LIGHT_RED, error);
                    }
                    if scanned {
                        let sources = locations.snapshots();
                        let skipped = locations.entries.len() - sources.len();
                        if sources.is_empty() {
                            ui.small("스캔한 위치가 없습니다. 폴더를 끌어놓거나 다른 검색 범위를 선택하세요.");
                        } else if skipped > 0 || locations.is_running() {
                            let previous = if locations.is_running() { " · 스캔 중인 위치는 이전 결과 사용" } else { "" };
                            ui.small(format!("스캔한 위치 {}곳 · 제외 {skipped}곳{previous}", sources.len()));
                        }
                    }
                    if quick.count() > 0 {
                        let menu = ui.add_enabled_ui(!self.settings, |ui| {
                            self.draw_results(ui, quick, !start_search)
                        }).inner;
                        if let Some(menu) = menu {
                            match result_action(quick, locations, menu) {
                                Ok(next) => action = Some(next),
                                Err(e) => self.error = Some(e.to_string()),
                            }
                        }
                    } else if scanned && quick.job.running {
                        ui.horizontal(|ui| {
                            crate::ui_style::activity_indicator(ui);
                            ui.label("검색 중…");
                        });
                    } else if (scanned && quick.job.results.is_some())
                        || (quick.live.input.is_some() && !quick.live.running() && !quick.live.stopped
                            && quick.live.error.is_none() && quick.live.coverage.skipped == 0)
                    {
                        ui.label("일치하는 파일이나 폴더가 없습니다.");
                    }

                    ui.with_layout(egui::Layout::bottom_up(egui::Align::LEFT), |ui| {
                        if scanned {
                            ui.small("↑↓ 선택   Enter 열기   Ctrl+Enter 분석에서 보기   Esc 닫기");
                        } else {
                            live_status(ui, quick);
                        }
                    });
                });
            if self.settings {
                let mut open = true;
                let settings = |ui: &mut egui::Ui| {
                    ui.set_max_width(380.0);
                    ui.checkbox(&mut self.draft.enabled, "전역 단축키 사용");
                    ui.horizontal(|ui| {
                        for (bit, label) in [(2, "Ctrl"), (1, "Alt"), (4, "Shift"), (8, "Win")] {
                            let mut selected = self.draft.shortcut.modifiers & bit != 0;
                            if ui.checkbox(&mut selected, label).changed() {
                                if selected {
                                    self.draft.shortcut.modifiers |= bit;
                                } else {
                                    self.draft.shortcut.modifiers &= !bit;
                                }
                            }
                        }
                    });
                    ui.horizontal(|ui| {
                        ui.label("키");
                        ui.text_edit_singleline(&mut self.key);
                    });
                    ui.small("Space, 영문자 또는 F1~F24");
                    ui.separator();
                    ui.checkbox(
                        &mut self.draft.stay,
                        "메인 창을 닫아도 트레이에서 계속 실행",
                    );
                    ui.small("스캔·파일 작업·실시간 갱신이 계속됩니다. 트레이에서 완전 종료할 수 있습니다.");
                    ui.small(format!(
                        "현재 적용: 단축키 {} · 상주 {}",
                        if applied.enabled { "켜짐" } else { "꺼짐" },
                        if applied.stay { "켜짐" } else { "꺼짐" }
                    ));
                    if ui
                        .add_enabled(!self.pending, egui::Button::new("적용"))
                        .clicked()
                    {
                        match Shortcut::parse_key(&self.key).and_then(|key| {
                            self.draft.shortcut.key = key;
                            self.draft.validate()
                        }) {
                            Ok(()) => action = Some(Action::Apply(self.draft)),
                            Err(e) => self.error = Some(e.to_string()),
                        }
                    }
                    if self.pending {
                        ui.label("Windows 설정 적용 중…");
                    }
                    if let Some(error) = &self.error {
                        ui.colored_label(egui::Color32::LIGHT_RED, error);
                    }
                };
                egui::Window::new("검색창 설정")
                    .open(&mut open)
                    .default_width(380.0)
                    .show(&ctx, settings);
                self.settings = open;
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

fn live_status(ui: &mut egui::Ui, quick: &mut QuickSearch) {
    let count = quick.count();
    let live = &mut quick.live;
    if live.input.is_none() {
        ui.small(if live.running() {
            "이전 검색 중지 중…"
        } else {
            "검색을 누르면 이 위치에서 파일과 폴더를 찾습니다."
        });
        return;
    }
    if live.coverage.skipped > 0 {
        ui.small(format!(
            "접근 불가·특수 경로 {}건 제외",
            live.coverage.skipped
        ))
        .on_hover_text(live.coverage.first_issue.as_deref().unwrap_or_default());
    }
    let running = live.running();
    ui.horizontal(|ui| {
        if running {
            crate::ui_style::activity_indicator(ui);
        }
        let status = if running && live.stopped {
            "검색 중지 중"
        } else if running {
            "검색 중"
        } else if live.stopped {
            "검색 중지됨"
        } else if live.error.is_some() {
            "검색 실패"
        } else if live.coverage.limited {
            "표시 한도 도달 · 범위를 좁혀 주세요"
        } else {
            "검색 완료"
        };
        ui.label(format!("{status} · {count}건"));
        if running && !live.stopped {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("중지").clicked() {
                    live.stop();
                }
            });
        }
    });
    ui.add_space(6.0);
    ui.separator();
}

fn result_action(
    quick: &QuickSearch,
    locations: &Locations,
    action: RowAction,
) -> std::io::Result<Action> {
    if matches!(action, RowAction::Analyze) {
        return quick.selected(locations).map(Action::Analyze);
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
                ui.show(root.ctx(), quick, &locations, Preferences::default(), false);
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
        fs::remove_dir_all(path).unwrap();
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
                ui.show(
                    root.ctx(),
                    &mut quick,
                    &locations,
                    Preferences::default(),
                    false,
                );
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
            ui.show(
                root.ctx(),
                &mut quick,
                &locations,
                Preferences::default(),
                false,
            );
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
