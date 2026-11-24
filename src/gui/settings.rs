use super::DiskWeaveApp;
use crate::quick_settings::{Preferences, Shortcut};
use eframe::egui;

const MODIFIERS: [(u32, &str); 4] = [(2, "Ctrl"), (1, "Alt"), (4, "Shift"), (8, "Win")];

fn setting_row(ui: &mut egui::Ui, label: &str, control: impl FnOnce(&mut egui::Ui)) {
    ui.horizontal(|ui| {
        ui.label(label);
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), control);
    });
}

fn toggle(ui: &mut egui::Ui, label: &str, value: &mut bool) -> egui::Response {
    use crate::ui_style::{self, ACCENT, FIELD_HEIGHT, LINE, MUTED, TEXT};

    let (rect, mut response) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), FIELD_HEIGHT),
        egui::Sense::click(),
    );
    if response.clicked() {
        *value = !*value;
        response.mark_changed();
        response.request_focus();
    }
    response.widget_info(|| {
        egui::WidgetInfo::selected(egui::Role::CheckBox, ui.is_enabled(), *value, label)
    });
    let track = egui::Rect::from_min_size(
        rect.right_center() - egui::vec2(34.0, 9.0),
        egui::vec2(34.0, 18.0),
    );
    let label_rect =
        egui::Rect::from_min_max(rect.min, egui::pos2(track.left() - 8.0, rect.bottom()));
    let enabled = ui.is_enabled();
    ui_style::text(
        ui,
        label_rect,
        label,
        false,
        if enabled { TEXT } else { MUTED },
        13.0,
    );
    let painter = ui.painter();
    painter.rect_filled(track, 9.0, if *value && enabled { ACCENT } else { LINE });
    let knob = if *value {
        track.right() - 9.0
    } else {
        track.left() + 9.0
    };
    painter.circle_filled(
        egui::pos2(knob, track.center().y),
        6.0,
        if *value && enabled { TEXT } else { MUTED },
    );
    if response.has_focus() || response.hovered() {
        painter.rect_stroke(
            track.expand(2.0),
            10.0,
            egui::Stroke::new(1.0, ACCENT),
            egui::StrokeKind::Inside,
        );
    }
    response
}

pub(super) struct Draft {
    preferences: Preferences,
    key: String,
    editing_key: bool,
}

impl Draft {
    pub(super) fn new(preferences: Preferences) -> Self {
        Self {
            preferences,
            key: preferences.shortcut.key_name(),
            editing_key: false,
        }
    }

    fn shortcut_editor(&mut self, ui: &mut egui::Ui) {
        setting_row(ui, "검색 단축키", |ui| {
            let mut keys: Vec<_> = MODIFIERS
                .into_iter()
                .filter_map(|(bit, label)| {
                    (self.preferences.shortcut.modifiers & bit != 0).then_some(label)
                })
                .collect();
            keys.push(&self.key);
            if ui
                .button(if self.editing_key { "접기" } else { "변경" })
                .clicked()
            {
                self.editing_key = !self.editing_key;
            }
            ui.label(keys.join("+"));
        });
        if self.editing_key {
            ui.horizontal_wrapped(|ui| {
                for (bit, label) in MODIFIERS {
                    let mut selected = self.preferences.shortcut.modifiers & bit != 0;
                    if ui.checkbox(&mut selected, label).changed() {
                        self.preferences.shortcut.modifiers ^= bit;
                    }
                }
                ui.add(
                    egui::TextEdit::singleline(&mut self.key)
                        .desired_width(75.0)
                        .hint_text("키 입력"),
                );
            });
            ui.small("Space, 영문자 또는 F1~F24");
        }
    }
}

impl DiskWeaveApp {
    pub(super) fn open_settings(&mut self, ctx: &egui::Context) {
        self.show_main(ctx);
        self.quick.visible = false;
        self.settings_draft
            .get_or_insert_with(|| Draft::new(self.desktop.applied));
        ctx.request_repaint();
    }

    pub(super) fn settings_window(&mut self, ctx: &egui::Context) {
        let Some(mut draft) = self.settings_draft.take() else {
            return;
        };
        let frame = egui::Frame::new()
            .fill(crate::ui_style::SIDEBAR)
            .stroke(egui::Stroke::new(1.0, crate::ui_style::LINE))
            .corner_radius(10.0)
            .inner_margin(18.0);
        let modal = egui::Modal::new(egui::Id::unique("workspace_settings"))
            .frame(frame)
            .show(ctx, |ui| {
                ui.set_width((ctx.content_rect().width() - 40.0).clamp(260.0, 500.0));
                let close_from_title = ui
                    .horizontal(|ui| {
                        ui.heading("설정");
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            let close = ui.button("×");
                            close.widget_info(|| {
                                egui::WidgetInfo::labeled(egui::Role::Button, true, "설정 닫기")
                            });
                            close.on_hover_text("설정 닫기").clicked()
                        })
                        .inner
                    })
                    .inner;
                ui.separator();
                egui::ScrollArea::vertical()
                    .max_height((ctx.content_rect().height() - 130.0).max(140.0))
                    .show(ui, |ui| {
                        ui.spacing_mut().item_spacing.y = 8.0;
                        ui.label(
                            egui::RichText::new("스캔 및 갱신")
                                .strong()
                                .color(crate::ui_style::ACCENT),
                        );
                        let mut refresh = !self.folder_refresh_disabled;
                        if toggle(ui, "보고 있는 폴더 자동 갱신", &mut refresh).changed()
                        {
                            self.folder_refresh_disabled = !refresh;
                        }
                        setting_row(ui, "스캔 동시 작업", |ui| self.scan_settings(ui));
                        ui.small("자동은 최대 8개 작업을 사용합니다.");
                        ui.separator();
                        ui.label(
                            egui::RichText::new("검색 및 단축키")
                                .strong()
                                .color(crate::ui_style::ACCENT),
                        );
                        ui.add_enabled_ui(!self.desktop.busy, |ui| {
                            toggle(
                                ui,
                                "다른 앱에서도 검색 열기",
                                &mut draft.preferences.enabled,
                            );
                            draft.shortcut_editor(ui);
                            toggle(
                                ui,
                                "창을 닫아도 백그라운드에서 실행",
                                &mut draft.preferences.stay,
                            );
                            let changed = draft.preferences != self.desktop.applied
                                || draft.key != self.desktop.applied.shortcut.key_name();
                            let apply = ui
                                .with_layout(
                                    egui::Layout::right_to_left(egui::Align::Center),
                                    |ui| {
                                        ui.add_enabled(changed, egui::Button::new("검색 설정 적용"))
                                            .clicked()
                                    },
                                )
                                .inner;
                            if apply {
                                let result = (|| -> std::io::Result<()> {
                                    draft.preferences.shortcut.key =
                                        Shortcut::parse_key(&draft.key)?;
                                    draft.preferences.validate()?;
                                    self.desktop.apply(ctx, draft.preferences)
                                })();
                                self.desktop_settings_error = result.err().map(|e| e.to_string());
                            }
                        });
                        if self.desktop.busy {
                            ui.small("Windows 설정 적용 중…");
                        }
                        if let Some(error) = &self.desktop_settings_error {
                            ui.add(
                                egui::Label::new(
                                    egui::RichText::new(error).color(egui::Color32::LIGHT_RED),
                                )
                                .wrap(),
                            );
                        }
                        if let Some(error) = &self.settings_error {
                            ui.add(
                                egui::Label::new(
                                    egui::RichText::new(error).color(egui::Color32::LIGHT_RED),
                                )
                                .wrap(),
                            );
                            if ui.button("화면 설정 저장 다시 시도").clicked() {
                                self.settings_attempt = None;
                                self.save_ui_settings();
                            }
                        }
                        ui.separator();
                        ui.collapsing("도움말", |ui| {
                            for text in [
                                "Ctrl+F  목록 검색 · Ctrl+L  스캔 대상",
                                "F5  선택 위치 다시 스캔",
                                "Enter  폴더 이동 / 파일 열기",
                                "Ctrl+A  목록 전체 선택",
                                "Ctrl+Shift+C  경로 복사",
                                "F2  이름 변경 · Delete  휴지통 확인",
                            ] {
                                ui.label(text);
                            }
                        });
                        ui.collapsing("앱 정보", |ui| {
                            ui.small(concat!("DiskWeave ", env!("CARGO_PKG_VERSION")));
                            ui.collapsing("글꼴 라이선스 · Pretendard", |ui| {
                                ui.label(include_str!("../../assets/fonts/OFL.txt"));
                            });
                        });
                    });
                ui.separator();
                let close_from_footer = ui
                    .with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.button("닫기").clicked()
                    })
                    .inner;
                close_from_title || close_from_footer
            });
        if modal.inner || modal.should_close() {
            self.save_ui_settings();
        } else {
            self.settings_draft = Some(draft);
        }
    }
}
