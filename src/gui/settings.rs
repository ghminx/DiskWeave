use super::DiskWeaveApp;
use crate::quick_settings::{Preferences, Shortcut};
use eframe::egui;

const MODIFIERS: [(u32, &str); 4] = [(2, "Ctrl"), (1, "Alt"), (4, "Shift"), (8, "Win")];

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
        ui.horizontal(|ui| {
            ui.label("검색 단축키");
            let mut keys: Vec<_> = MODIFIERS
                .into_iter()
                .filter_map(|(bit, label)| {
                    (self.preferences.shortcut.modifiers & bit != 0).then_some(label)
                })
                .collect();
            keys.push(&self.key);
            ui.label(keys.join("+"));
            if ui
                .button(if self.editing_key { "접기" } else { "변경" })
                .clicked()
            {
                self.editing_key = !self.editing_key;
            }
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
        let modal = egui::Modal::new(egui::Id::unique("workspace_settings")).show(ctx, |ui| {
            ui.set_width((ctx.content_rect().width() - 40.0).clamp(260.0, 500.0));
            ui.heading("설정");
            ui.separator();
            egui::ScrollArea::vertical()
                .max_height((ctx.content_rect().height() - 130.0).max(140.0))
                .show(ui, |ui| {
                    ui.spacing_mut().item_spacing.y = 8.0;
                    ui.strong("스캔 및 갱신");
                    let mut refresh = !self.folder_refresh_disabled;
                    if ui
                        .checkbox(&mut refresh, "보고 있는 폴더 자동 갱신")
                        .changed()
                    {
                        self.folder_refresh_disabled = !refresh;
                    }
                    self.scan_settings(ui);
                    ui.separator();
                    ui.strong("검색 및 단축키");
                    ui.add_enabled_ui(!self.desktop.busy, |ui| {
                        ui.checkbox(&mut draft.preferences.enabled, "다른 앱에서도 검색 열기");
                        draft.shortcut_editor(ui);
                        ui.checkbox(
                            &mut draft.preferences.stay,
                            "창을 닫아도 백그라운드에서 실행",
                        );
                        let changed = draft.preferences != self.desktop.applied
                            || draft.key != self.desktop.applied.shortcut.key_name();
                        if ui
                            .add_enabled(changed, egui::Button::new("검색 설정 적용"))
                            .clicked()
                        {
                            let result = (|| -> std::io::Result<()> {
                                draft.preferences.shortcut.key = Shortcut::parse_key(&draft.key)?;
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
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.button("닫기").clicked()
            })
            .inner
        });
        if modal.inner || modal.should_close() {
            self.save_ui_settings();
        } else {
            self.settings_draft = Some(draft);
        }
    }
}
