use crate::{
    explorer::Item,
    file_open::OpenAction,
    locations::Locations,
    presentation::SizeDisplay,
    quick_search::QuickSearch,
    quick_settings::{Preferences, Shortcut},
};
use eframe::egui;
pub enum Action {
    ShowMain,
    Apply(Preferences),
    Open((u64, u64, Item), OpenAction),
    Analyze((u64, u64, Item)),
    Copy((u64, u64, Item)),
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
            row: 48.0,
        }
    }
}
impl QuickUi {
    pub fn open_settings(&mut self, p: Preferences) {
        self.draft = p;
        self.key = p.shortcut.key_name();
        self.settings = true;
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
                || self.seen_focus && !focused
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
                    ui.horizontal(|ui| {
                        ui.spacing_mut().interact_size.y = crate::ui_style::FIELD_HEIGHT;
                        egui::ComboBox::from_id_salt("quick_mode")
                            .selected_text(if quick.mode == crate::search::SearchMode::Name {
                                "이름"
                            } else {
                                "전체 경로"
                            })
                            .show_ui(ui, |ui| {
                                ui.selectable_value(
                                    &mut quick.mode,
                                    crate::search::SearchMode::Name,
                                    "이름",
                                );
                                ui.selectable_value(
                                    &mut quick.mode,
                                    crate::search::SearchMode::Path,
                                    "전체 경로",
                                );
                            });
                        let input = ui.add(
                            crate::ui_style::text_field(&mut quick.text, ui.available_width())
                                .hint_text("파일 또는 폴더 찾기"),
                        );
                        if quick.focus {
                            input.request_focus();
                            quick.focus = false;
                        }
                    });
                    ui.add_space(8.0);
                    let sources = locations.snapshots();
                    let skipped = locations.entries.len() - sources.len();
                    ui.small(format!(
                        "스캔한 위치 {}곳에서 검색{}{}",
                        sources.len(),
                        if locations.is_running() {
                            " · 스캔 중인 위치의 이전 완료 결과 포함"
                        } else {
                            ""
                        },
                        if skipped > 0 {
                            format!(" · 미수집/갱신 실패 {skipped}곳 제외")
                        } else {
                            String::new()
                        }
                    ));
                    if let Some(error) = &self.error {
                        ui.colored_label(egui::Color32::from_rgb(255, 135, 135), error);
                    }
                    if sources.is_empty() {
                        ui.add_space(24.0);
                        ui.label("먼저 메인 창에서 폴더를 스캔해 주세요.");
                        if ui.button("메인 창 열기").clicked() {
                            action = Some(Action::ShowMain);
                        }
                    } else if quick.text.trim().is_empty() {
                        ui.add_space(24.0);
                        ui.label("이름이나 경로를 입력하면 파일과 폴더를 찾습니다.");
                    } else if quick.job.running {
                        ui.add_space(10.0);
                        ui.horizontal(|ui| {
                            crate::ui_style::activity_indicator(ui);
                            ui.label("검색 중…");
                        });
                    } else if let Some(error) = &quick.job.error {
                        ui.colored_label(egui::Color32::LIGHT_RED, error);
                    } else if let Some(results) = &quick.job.results {
                        let count = results.hits.len();
                        ui.add_space(4.0);
                        ui.small(format!("{count}개 결과 · 크기순"));
                        ui.add_space(4.0);
                        let row_height =
                            Layout::new(ui.available_width(), ui.available_height()).row;
                        let mut scroll = egui::ScrollArea::vertical()
                            .id_salt("quick_results")
                            .auto_shrink([false, false])
                            .max_height((ui.available_height() - 28.0).max(80.0));
                        if let Some(row) = self.scroll.take() {
                            scroll = scroll.vertical_scroll_offset(
                                row as f32 * (row_height + ui.spacing().item_spacing.y),
                            );
                        }
                        let mut picked = None;
                        let mut opened = None;
                        let mut menu = None;
                        scroll.show_rows(ui, row_height, count, |ui, range| {
                            for row in range {
                                let hit = results.hits[row];
                                let source = &results.sources[hit.source];
                                let path = crate::search::item_path(&source.scan, hit.item);
                                let name = crate::search::item_name(&source.scan, hit.item)
                                    .to_string_lossy();
                                let size = SizeDisplay::new(crate::search::item_bytes(
                                    &source.scan,
                                    hit.item,
                                ));
                                let (rect, response) = ui.allocate_exact_size(
                                    egui::vec2(ui.available_width(), row_height),
                                    egui::Sense::click(),
                                );
                                if quick.selection == Some(row) || response.hovered() {
                                    ui.painter()
                                        .rect_filled(rect, 5.0, crate::ui_style::SELECTED);
                                }
                                let top = egui::Rect::from_min_max(
                                    rect.min + egui::vec2(8.0, 2.0),
                                    egui::pos2(rect.right() - 8.0, rect.top() + 24.0),
                                );
                                let bottom = egui::Rect::from_min_max(
                                    rect.min + egui::vec2(8.0, 24.0),
                                    rect.max - egui::vec2(8.0, 2.0),
                                );
                                let name_rect = egui::Rect::from_min_max(
                                    top.min,
                                    egui::pos2(top.right() - 100.0, top.bottom()),
                                );
                                crate::ui_style::text(
                                    ui,
                                    name_rect,
                                    &name,
                                    false,
                                    crate::ui_style::TEXT,
                                    15.0,
                                );
                                crate::ui_style::text(
                                    ui,
                                    top,
                                    &size.text,
                                    true,
                                    crate::ui_style::MUTED,
                                    13.0,
                                );
                                crate::ui_style::text(
                                    ui,
                                    bottom,
                                    &path.display().to_string(),
                                    false,
                                    crate::ui_style::MUTED,
                                    12.0,
                                );
                                response.widget_info(|| {
                                    egui::WidgetInfo::selected(
                                        egui::Role::Button,
                                        true,
                                        quick.selection == Some(row),
                                        format!("{name}, {}, {}", size.text, path.display()),
                                    )
                                });
                                let response = response.on_hover_text(format!(
                                    "{}\n{}",
                                    path.display(),
                                    size.exact
                                ));
                                if response.clicked() {
                                    picked = Some(row);
                                }
                                if response.double_clicked() {
                                    opened = Some(row);
                                }
                                response.context_menu(|ui| {
                                    crate::ui_style::menu_spacing(ui);
                                    for (label, kind) in [
                                        ("열기", 0),
                                        ("위치 열기", 1),
                                        ("경로 복사", 2),
                                        ("분석에서 보기", 3),
                                    ] {
                                        if ui.button(label).clicked() {
                                            picked = Some(row);
                                            menu = Some(kind);
                                            ui.close();
                                        }
                                    }
                                });
                            }
                        });
                        if let Some(row) = picked {
                            quick.selection = Some(row);
                        }
                        if let Some(row) = opened {
                            quick.selection = Some(row);
                            menu = Some(0);
                        }
                        if !self.settings && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                            if quick.selection.is_none() && count > 0 {
                                quick.selection = Some(0);
                            }
                            menu = Some(if ui.input(|i| i.modifiers.ctrl) { 3 } else { 0 });
                        }
                        if let Some(kind) = menu {
                            match quick.selected(locations) {
                                Ok(target) => {
                                    action = Some(match kind {
                                        2 => Action::Copy(target),
                                        3 => Action::Analyze(target),
                                        _ => Action::Open(
                                            target,
                                            match target.2 {
                                                Item::Folder(_) => OpenAction::OpenFolder,
                                                Item::File(_) => {
                                                    if kind == 1 {
                                                        OpenAction::RevealFile
                                                    } else {
                                                        OpenAction::OpenFile
                                                    }
                                                }
                                            },
                                        ),
                                    });
                                }
                                Err(e) => self.error = Some(e.to_string()),
                            }
                        }
                    }
                    ui.with_layout(egui::Layout::bottom_up(egui::Align::LEFT), |ui| {
                        ui.small("↑↓ 선택   Enter 열기   Ctrl+Enter 분석에서 보기   Esc 닫기");
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
                .with_inner_size([initial.width, initial.height])
                .with_min_inner_size([480.0, 320.0]),
            draw,
        );
        action
    }
}
#[cfg(test)]
mod tests {
    use super::*;
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
