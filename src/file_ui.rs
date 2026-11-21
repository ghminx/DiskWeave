use crate::{
    file_job::Controller,
    file_ops::{Action, Collision, Outcome, Request},
};
use eframe::egui;
use std::{io, path::PathBuf};
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Folder,
    Rename,
    Copy,
    Move,
    Recycle,
}
impl Kind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Folder => "새 폴더",
            Self::Rename => "이름 변경",
            Self::Copy => "다른 폴더에 복사",
            Self::Move => "다른 폴더로 이동",
            Self::Recycle => "휴지통으로 이동",
        }
    }
}
#[derive(Clone)]
pub struct Context {
    pub paths: Vec<PathBuf>,
    pub current: Option<PathBuf>,
    pub protected: Vec<PathBuf>,
    pub writable: bool,
}
pub struct Dialog {
    pub kind: Kind,
    pub name: String,
    pub destination: String,
    transfer_choice: bool,
    context: Context,
    expected: Option<(crate::metadata::Identity, bool)>,
}
#[derive(Default)]
pub struct FileUi {
    pub copy_requested: Option<Option<Vec<PathBuf>>>,
    pub importing: bool,
    pub drop_requested: Option<(Vec<PathBuf>, PathBuf)>,
    pub delete_confirmed: bool,
    pub dialog: Option<Dialog>,
    pub requested: Option<(Kind, Option<Vec<PathBuf>>)>,
    pub visible: bool,
    pub error: Option<String>,
    pub replace_confirmed: bool,
}
#[derive(Clone)]
pub struct DragPayload {
    pub paths: Vec<PathBuf>,
}
pub fn drop_hint(ui: &egui::Ui, response: &egui::Response) {
    if response.dnd_hover_payload::<DragPayload>().is_some() {
        ui.painter().rect_stroke(
            response.rect,
            4.0,
            egui::Stroke::new(2.0, crate::ui_style::ACCENT),
            egui::StrokeKind::Inside,
        );
    }
}
impl FileUi {
    pub fn execution_allowed(&self, plan: &crate::file_ops::Plan) -> bool {
        (!matches!(plan.action, Action::Recycle) || self.delete_confirmed)
            && (!plan.items.iter().any(|i| i.collision == Collision::Replace)
                || self.replace_confirmed)
    }
    pub fn route_drop(
        &mut self,
        paths: Vec<PathBuf>,
        context: Context,
    ) -> io::Result<Option<Vec<PathBuf>>> {
        if !self.importing {
            return Ok(Some(paths));
        }
        if paths.is_empty() {
            return Err(io::Error::other("가져올 항목을 선택하세요"));
        }
        let mut context = context;
        context.paths = paths;
        self.begin(Kind::Copy, context)?;
        self.dialog.as_mut().unwrap().transfer_choice = true;
        self.importing = false;
        Ok(None)
    }
    pub fn folder_drop(&mut self, destination: PathBuf, context: Context) -> io::Result<()> {
        self.begin(Kind::Copy, context)?;
        let dialog = self.dialog.as_mut().unwrap();
        dialog.destination = destination.display().to_string();
        dialog.transfer_choice = true;
        Ok(())
    }

    pub fn begin(&mut self, kind: Kind, context: Context) -> io::Result<()> {
        if !context.writable {
            return Err(io::Error::other(
                "현재 파일을 다시 분석한 뒤 변경할 수 있습니다. 저장된 결과는 조회 전용입니다",
            ));
        }
        if (kind == Kind::Rename && context.paths.len() != 1)
            || (matches!(kind, Kind::Copy | Kind::Move | Kind::Recycle) && context.paths.is_empty())
            || (kind == Kind::Folder && context.current.is_none())
        {
            return Err(io::Error::other(
                "작업할 파일·폴더와 현재 위치를 확인하세요",
            ));
        }
        let name = if kind == Kind::Rename {
            context.paths[0]
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned()
        } else {
            "새 폴더".into()
        };
        let destination = context
            .current
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_default();
        self.dialog = Some(Dialog {
            kind,
            name,
            destination,
            transfer_choice: false,
            context,
            expected: None,
        });
        self.visible = false;
        self.error = None;
        self.replace_confirmed = false;
        self.delete_confirmed = false;
        Ok(())
    }
    pub fn begin_search_recycle(
        &mut self,
        context: Context,
        identity: crate::metadata::Identity,
        directory: bool,
    ) -> io::Result<()> {
        self.begin(Kind::Recycle, context)?;
        self.dialog.as_mut().unwrap().expected = Some((identity, directory));
        Ok(())
    }
    pub fn request(&self) -> io::Result<Request> {
        let d = self
            .dialog
            .as_ref()
            .ok_or_else(|| io::Error::other("파일 작업을 먼저 선택하세요"))?;
        let action = match d.kind {
            Kind::Folder => {
                crate::file_ops::validate_name(d.name.as_ref())?;
                Action::NewFolder {
                    parent: d.context.current.clone().unwrap(),
                    name: d.name.clone().into(),
                }
            }
            Kind::Rename => {
                crate::file_ops::validate_name(d.name.as_ref())?;
                Action::Rename {
                    name: d.name.clone().into(),
                }
            }
            Kind::Recycle => Action::Recycle,
            Kind::Copy | Kind::Move => {
                if d.destination.trim().is_empty() {
                    return Err(io::Error::other("목적지 폴더를 지정하세요"));
                }
                if d.kind == Kind::Move {
                    Action::Move {
                        destination: d.destination.clone().into(),
                    }
                } else {
                    Action::Copy {
                        destination: d.destination.clone().into(),
                    }
                }
            }
        };
        Ok(Request {
            action,
            paths: if d.kind == Kind::Folder {
                vec![]
            } else {
                d.context.paths.clone()
            },
            protected: d.context.protected.clone(),
        })
    }
    pub fn menu(
        &mut self,
        ui: &mut egui::Ui,
        enabled: bool,
        paths: Option<Vec<PathBuf>>,
        count: usize,
        has_folder: bool,
    ) {
        crate::ui_style::menu_spacing(ui);
        if crate::ui_style::menu_action(
            ui,
            if count > 1 {
                "선택한 경로 복사"
            } else {
                "경로 복사"
            },
            count > 0,
        )
        .clicked()
        {
            self.copy_requested = Some(paths.clone());
            ui.close();
        }
        ui.separator();
        for kind in [
            Kind::Folder,
            Kind::Copy,
            Kind::Rename,
            Kind::Move,
            Kind::Recycle,
        ] {
            if kind == Kind::Folder {
                ui.small("만들기 · 복사");
            }
            if kind == Kind::Rename {
                ui.separator();
                ui.small("원본 변경");
            }
            if kind == Kind::Recycle {
                ui.separator();
            }
            let available = enabled
                && match kind {
                    Kind::Folder => has_folder,
                    Kind::Rename => count == 1,
                    Kind::Copy | Kind::Move | Kind::Recycle => count > 0,
                };
            if crate::ui_style::menu_action(ui, kind.label(), available).clicked() {
                self.requested = Some((kind, paths.clone()));
                ui.close();
            }
        }
        ui.separator();
        if crate::ui_style::menu_action(ui, "파일 가져오기…", enabled).clicked() {
            self.importing = true;
            ui.close();
        }
    }
    pub fn blocked(&self, job: &Controller) -> bool {
        self.dialog.is_some() || self.importing || job.busy()
    }
    pub fn show(&mut self, ctx: &egui::Context, job: &mut Controller) {
        let width = (ctx.content_rect().width() - 48.0).clamp(260.0, 620.0);
        if self.importing {
            egui::Modal::new(egui::Id::unique("file_import")).show(ctx, |ui| {
                ui.set_width(width);
                ui.heading("파일 가져오기");
                ui.add_space(12.0);
                ui.label("탐색기에서 파일·폴더를 이 창으로 끌어다 놓으세요.");
                ui.label("다음 화면에서 복사/이동과 목적지를 확인합니다.");
                ui.small("분석 대상은 바뀌지 않습니다.");
                if ui.button("취소").clicked() {
                    self.importing = false;
                }
            });
        }
        if self.dialog.is_some() {
            let mut submit = false;
            let mut cancel = false;
            egui::Modal::new(egui::Id::unique("file_operation_input")).show(ctx, |ui| {
                ui.set_width(width);
                let d = self.dialog.as_mut().unwrap();
                ui.heading(d.kind.label());
                ui.add_space(12.0);
                ui.label("1. 작업 설정   →   2. 변경 내용 검토   →   3. 실행");
                ui.small(
                    "이 화면에서는 파일이 변경되지 않습니다. 다음 단계에서 검토 후 실행합니다.",
                );
                ui.add_space(12.0);
                ui.separator();
                ui.add_space(10.0);
                if d.transfer_choice {
                    ui.horizontal(|ui| {
                        ui.selectable_value(&mut d.kind, Kind::Copy, "복사");
                        ui.selectable_value(&mut d.kind, Kind::Move, "이동");
                    });
                }
                ui.add_space(10.0);
                ui.label(if d.kind == Kind::Folder {
                    "현재 폴더 안에 생성합니다".into()
                } else {
                    format!("선택한 {}개 항목", d.context.paths.len())
                });
                egui::ScrollArea::vertical()
                    .max_height(100.0)
                    .id_salt("operation_sources")
                    .show(ui, |ui| {
                        for path in &d.context.paths {
                            ui.add(egui::Label::new(path.display().to_string()).wrap());
                        }
                    });
                if matches!(d.kind, Kind::Copy | Kind::Move) {
                    ui.add_space(10.0);
                    ui.label("목적지 폴더");
                    ui.add_space(4.0);
                    ui.add(crate::ui_style::text_field(
                        &mut d.destination,
                        f32::INFINITY,
                    ));
                    ui.add_space(8.0);
                    if ui.button("목적지 선택…").clicked() {
                        if let Some(path) = rfd::FileDialog::new()
                            .set_title("목적지 폴더 선택")
                            .pick_folder()
                        {
                            d.destination = path.display().to_string();
                        }
                    }
                } else if d.kind == Kind::Recycle {
                    ui.colored_label(
                        egui::Color32::from_rgb(255, 145, 140),
                        "선택한 항목과 하위 항목을 휴지통으로 옮깁니다.",
                    );
                    ui.label("휴지통 지원을 확인할 수 없으면 실행하지 않습니다.");
                } else {
                    ui.add_space(10.0);
                    ui.label(if d.kind == Kind::Rename {
                        "새 이름"
                    } else {
                        "폴더 이름"
                    });
                    ui.add_space(4.0);
                    ui.add(crate::ui_style::text_field(&mut d.name, f32::INFINITY));
                    if let Some(parent) = &d.context.current {
                        ui.add_space(6.0);
                        ui.add(egui::Label::new(parent.display().to_string()).wrap());
                    }
                }
                if let Some(error) = &self.error {
                    ui.colored_label(egui::Color32::from_rgb(255, 145, 140), error);
                }
                ui.add_space(16.0);
                ui.separator();
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    cancel = ui.button("취소").clicked();
                    submit = ui
                        .add(egui::Button::new("변경 내용 검토 →").fill(crate::ui_style::SELECTED))
                        .clicked();
                });
            });
            if cancel {
                self.dialog = None;
            }
            if submit {
                let expected = self.dialog.as_ref().and_then(|dialog| dialog.expected);
                match self.request().and_then(|request| match expected {
                    Some(expected) => job.prepare_expected(request, Some(expected)),
                    None => job.prepare(request),
                }) {
                    Ok(()) => {
                        self.dialog = None;
                        self.visible = true;
                    }
                    Err(e) => self.error = Some(e.to_string()),
                }
            }
        }
        if !self.visible {
            return;
        }
        let mut open = true;
        let mut confirm = false;
        let mut cancel = false;
        egui::Window::new("파일 작업")
            .id(egui::Id::unique("file_operation_status"))
            .open(&mut open)
            .default_pos(egui::pos2(
                (ctx.content_rect().width() - width) * 0.5,
                (ctx.content_rect().height() - 300.0).max(20.0) * 0.5,
            ))
            .frame(
                egui::Frame::window(&ctx.style_of(egui::Theme::Dark))
                    .fill(crate::ui_style::SIDEBAR),
            )
            .default_width(width)
            .max_width(width)
            .max_height((ctx.content_rect().height() - 70.0).max(260.0))
            .resizable(true)
            .collapsible(false)
            .show(ctx, |ui| {
                ui.heading(job.phase.label());
                if let Some(error) = &job.error {
                    ui.add(egui::Label::new(error).wrap());
                }
                if job.working() {
                    let p = job.progress();
                    ui.horizontal(|ui| {
                        crate::ui_style::activity_indicator(ui);
                        ui.label(format!("{:.1}초", job.elapsed()));
                    });
                    ui.add(egui::Label::new(p.current.display().to_string()).wrap());
                    if p.total > 0 {
                        ui.label(format!("선택 항목 {} / {}개", p.completed, p.total));
                    }
                    if p.bytes_total > 0 {
                        ui.add(
                            egui::ProgressBar::new(
                                (p.bytes_done as f64 / p.bytes_total as f64).clamp(0.0, 1.0) as f32,
                            )
                            .text(format!(
                                "현재 파일 {} / {}",
                                crate::presentation::SizeDisplay::new(p.bytes_done).text,
                                crate::presentation::SizeDisplay::new(p.bytes_total).text
                            )),
                        );
                    }
                    cancel = ui.button("남은 작업 취소").clicked();
                    ui.small("이미 완료한 변경은 유지됩니다.");
                }
                if let Some(plan) = &mut job.plan {
                    ui.label(format!(
                        "{} · {}개 항목",
                        plan.action.label(),
                        plan.items.len()
                    ));
                    egui::ScrollArea::vertical()
                        .id_salt("operation_plan")
                        .max_height(260.0)
                        .show_rows(ui, 84.0, plan.items.len(), |ui, rows| {
                            for row in rows {
                                let item = &plan.items[row];
                                ui.add(
                                    egui::Label::new(item.path.display().to_string()).truncate(),
                                )
                                .on_hover_text(item.path.display().to_string());
                                if matches!(plan.action, Action::Recycle) {
                                    ui.small("→ 휴지통");
                                } else {
                                    let path = format!("→ {}", item.destination.display());
                                    ui.add(egui::Label::new(&path).truncate())
                                        .on_hover_text(path);
                                }
                                if let Some(issue) = &item.issue {
                                    ui.add(
                                        egui::Label::new(
                                            egui::RichText::new(issue)
                                                .color(egui::Color32::from_rgb(255, 145, 140)),
                                        )
                                        .truncate(),
                                    )
                                    .on_hover_text(issue);
                                } else if let Some(existing) = &item.existing {
                                    let mut choice = item.collision;
                                    egui::ComboBox::from_id_salt(("operation_collision", row))
                                        .selected_text(match choice {
                                            Collision::Skip => "건너뛰기",
                                            Collision::KeepBoth => "다른 이름으로 보관",
                                            Collision::Replace => "파일 바꾸기",
                                        })
                                        .show_ui(ui, |ui| {
                                            ui.selectable_value(
                                                &mut choice,
                                                Collision::Skip,
                                                "건너뛰기",
                                            );
                                            ui.selectable_value(
                                                &mut choice,
                                                Collision::KeepBoth,
                                                "다른 이름으로 보관",
                                            );
                                            if !existing.directory
                                                && item
                                                    .source
                                                    .as_ref()
                                                    .is_some_and(|s| !s.directory)
                                            {
                                                ui.selectable_value(
                                                    &mut choice,
                                                    Collision::Replace,
                                                    "파일 바꾸기",
                                                );
                                            }
                                        });
                                    if choice == Collision::Replace {
                                        ui.small(format!("기존 파일: {} B", existing.bytes));
                                    }
                                    if choice != item.collision {
                                        if let Err(error) = plan.resolve(row, choice) {
                                            self.error = Some(error.to_string());
                                        }
                                    }
                                }
                                ui.separator();
                            }
                        });
                    let deleting = matches!(plan.action, Action::Recycle);
                    if deleting {
                        ui.checkbox(
                            &mut self.delete_confirmed,
                            format!("확인한 {}개 항목을 휴지통으로 옮깁니다", plan.items.len()),
                        );
                    }
                    let replace = plan.items.iter().any(|i| i.collision == Collision::Replace);
                    if replace {
                        ui.checkbox(
                            &mut self.replace_confirmed,
                            "표시된 기존 파일을 바꾸는 데 동의합니다",
                        );
                    }
                    ui.horizontal(|ui| {
                        cancel = ui.button("취소").clicked();
                        confirm = ui
                            .add_enabled(
                                self.execution_allowed(plan),
                                egui::Button::new(plan.action.label()).fill(
                                    if matches!(
                                        plan.action,
                                        Action::Recycle
                                            | Action::Rename { .. }
                                            | Action::Move { .. }
                                    ) {
                                        egui::Color32::from_rgb(123, 43, 49)
                                    } else {
                                        crate::ui_style::SELECTED
                                    },
                                ),
                            )
                            .clicked();
                    });
                }
                if let Some(report) = &job.report {
                    if let Some(action) = &report.action {
                        ui.label(action.label());
                    }
                    let succeeded = report.items.iter().filter(|i| i.succeeded()).count();
                    let skipped = report
                        .items
                        .iter()
                        .filter(|i| i.outcome == Outcome::Skipped)
                        .count();
                    let failed = report
                        .items
                        .iter()
                        .filter(|i| matches!(i.outcome, Outcome::Failed(_)))
                        .count();
                    let cancelled = report
                        .items
                        .iter()
                        .filter(|i| i.outcome == Outcome::Cancelled)
                        .count();
                    let pending = report
                        .items
                        .iter()
                        .filter(|i| i.outcome == Outcome::NotProcessed)
                        .count();
                    ui.label(format!(
                        "완료 {succeeded} · 건너뜀 {skipped} · 실패 {failed}"
                    ));
                    if cancelled + pending > 0 {
                        ui.label(format!("취소 {cancelled} · 미처리 {pending}"));
                    }
                    egui::ScrollArea::vertical()
                        .id_salt("operation_results")
                        .max_height(260.0)
                        .show_rows(ui, 64.0, report.items.len(), |ui, rows| {
                            for row in rows {
                                let item = &report.items[row];
                                ui.add(
                                    egui::Label::new(
                                        item.source
                                            .as_ref()
                                            .unwrap_or(&item.destination)
                                            .display()
                                            .to_string(),
                                    )
                                    .truncate(),
                                )
                                .on_hover_text(
                                    item.source
                                        .as_ref()
                                        .unwrap_or(&item.destination)
                                        .display()
                                        .to_string(),
                                );
                                if matches!(report.action, Some(Action::Recycle)) {
                                    ui.small("→ 휴지통");
                                } else {
                                    let path = format!("→ {}", item.destination.display());
                                    ui.add(egui::Label::new(&path).truncate())
                                        .on_hover_text(path);
                                }
                                let outcome = match &item.outcome {
                                    Outcome::Completed => "완료",
                                    Outcome::Skipped => "건너뜀",
                                    Outcome::Cancelled => "취소됨",
                                    Outcome::NotProcessed => "미처리",
                                    Outcome::Failed(error) => error,
                                };
                                let detail = item.note.as_deref().unwrap_or(outcome);
                                ui.add(egui::Label::new(detail).truncate())
                                    .on_hover_text(detail);
                                ui.separator();
                            }
                        });
                    ui.small("파일 작업 결과와 분석 목록 갱신 상태는 별도로 표시됩니다.");
                }
            });
        if cancel || (!open && job.busy()) {
            job.cancel();
        }
        if confirm {
            if let Err(e) = job.confirm() {
                self.error = Some(e.to_string());
            }
        }
        self.visible = open;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn recycling_requires_explicit_confirmation_and_blocks_saved_results() {
        let root = std::env::temp_dir().join(format!(
            "diskweave-ui-confirm-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("keep"), b"keep").unwrap();
        let mut state = FileUi::default();
        let context = Context {
            paths: vec![root.join("keep")],
            current: Some(root.clone()),
            protected: vec![root.clone()],
            writable: true,
        };
        state.begin(Kind::Recycle, context.clone()).unwrap();
        let plan =
            crate::file_ops::prepare(state.request().unwrap(), &|| false, &mut |_| {}).unwrap();
        assert!(!state.execution_allowed(&plan));
        state.delete_confirmed = true;
        assert!(state.execution_allowed(&plan));
        state.begin(Kind::Recycle, context.clone()).unwrap();
        assert!(!state.execution_allowed(&plan));
        let mut saved = context;
        saved.writable = false;
        assert!(state.begin(Kind::Recycle, saved).is_err());
        assert_eq!(std::fs::read(root.join("keep")).unwrap(), b"keep");
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn external_and_internal_drops_choose_management_only_after_explicit_import_or_folder_drop() {
        let context = Context {
            paths: vec![],
            current: Some("C:\\fixture".into()),
            protected: vec!["C:\\fixture".into()],
            writable: true,
        };
        let paths = vec![
            PathBuf::from("C:\\other\\a.bin"),
            PathBuf::from("C:\\other\\b.bin"),
        ];
        let mut ui = FileUi::default();
        assert_eq!(
            ui.route_drop(paths.clone(), context.clone()).unwrap(),
            Some(paths.clone())
        );
        assert!(ui.dialog.is_none());
        ui.importing = true;
        assert!(ui
            .route_drop(paths.clone(), context.clone())
            .unwrap()
            .is_none());
        assert_eq!(ui.request().unwrap().paths, paths);
        ui.dialog.as_mut().unwrap().kind = Kind::Move;
        assert!(matches!(ui.request().unwrap().action, Action::Move { .. }));
        let mut ui = FileUi::default();
        ui.folder_drop(
            "C:\\fixture\\to".into(),
            Context {
                paths: paths.clone(),
                ..context.clone()
            },
        )
        .unwrap();
        assert_eq!(ui.dialog.as_ref().unwrap().destination, "C:\\fixture\\to");
        assert_eq!(ui.request().unwrap().paths, paths);
        let mut readonly = context;
        readonly.writable = false;
        let mut ui = FileUi {
            importing: true,
            ..Default::default()
        };
        assert!(ui.route_drop(vec!["C:\\file".into()], readonly).is_err());
        assert!(ui.dialog.is_none());
    }
    #[test]
    fn management_dialog_keeps_selection_and_blocks_saved_or_invalid_contexts() {
        let context = Context {
            paths: vec!["C:\\fixture\\a.bin".into()],
            current: Some("C:\\fixture".into()),
            protected: vec!["C:\\fixture".into()],
            writable: true,
        };
        let mut ui = FileUi::default();
        ui.begin(Kind::Rename, context.clone()).unwrap();
        assert_eq!(ui.dialog.as_ref().unwrap().name, "a.bin");
        let request = ui.request().unwrap();
        assert_eq!(request.paths, context.paths);
        ui.begin(Kind::Copy, context.clone()).unwrap();
        assert!(matches!(
            ui.request().unwrap().action,
            crate::file_ops::Action::Copy { .. }
        ));
        let mut saved = context.clone();
        saved.writable = false;
        assert!(FileUi::default().begin(Kind::Rename, saved).is_err());
        let mut multiple = context;
        multiple.paths.push("C:\\fixture\\b.bin".into());
        assert!(FileUi::default().begin(Kind::Rename, multiple).is_err());
    }
}
