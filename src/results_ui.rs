use crate::documents::{Documents, Status, Task};
use crate::export::ExportScope;
use crate::global_search::Hit;
use crate::locations::Snapshot;
use crate::snapshot::SavedScan;
use eframe::egui;
use std::path::PathBuf;
use std::sync::{
    mpsc::{self, Receiver, TryRecvError},
    Arc,
};

#[derive(Default)]
pub struct ExportContext {
    pub sources: Vec<Snapshot>,
    pub selected: Option<usize>,
    pub folder: Option<usize>,
    pub matches: Option<(Vec<Snapshot>, Vec<Hit>)>,
}
enum Draft {
    Save(Arc<SavedScan>),
    Export {
        sources: Vec<Snapshot>,
        scope: ExportScope,
    },
}
impl Draft {
    fn task(self, path: PathBuf, overwrite: bool) -> Task {
        match self {
            Self::Save(record) => Task::Save {
                record,
                path,
                overwrite,
            },
            Self::Export { sources, scope } => Task::Export {
                sources,
                scope,
                path,
                overwrite,
            },
        }
    }
}
enum Picked {
    Write(Draft, PathBuf, bool),
    Open(PathBuf),
    Before(PathBuf),
    After(PathBuf),
    Cancelled,
    Failed(String),
}
#[derive(Default)]
pub struct ResultsUi {
    pub export_context: Option<ExportContext>,
    pub job: Documents,
    pub export_open: bool,
    pub compare_open: bool,
    pub scope: usize,
    pub before: Option<PathBuf>,
    pub after: Option<PathBuf>,
    chooser: Option<Receiver<Picked>>,
    overwrite: Option<(Draft, PathBuf)>,
    pub query: String,
    kind: Option<crate::comparison::ChangeKind>,
    folders_only: bool,
    visible: Vec<usize>,
    filter_key: Option<(String, Option<crate::comparison::ChangeKind>, bool)>,
}
fn dialog(title: &'static str, ctx: &egui::Context, width: f32) -> egui::Window<'static> {
    let rect = ctx.viewport_rect().shrink(16.0);
    let width = width.min(rect.width() - 32.0);
    egui::Window::new(title)
        .frame(
            egui::Frame::new()
                .fill(crate::ui_style::SURFACE)
                .stroke(egui::Stroke::new(1.0, crate::ui_style::LINE))
                .inner_margin(16)
                .corner_radius(8),
        )
        .title_frame(
            egui::Frame::new()
                .fill(crate::ui_style::SIDEBAR)
                .inner_margin(8),
        )
        .default_width(width)
        .max_width(rect.width() - 32.0)
        .max_height(rect.height() - 32.0)
        .default_pos(rect.center() - egui::vec2(width / 2.0, 160.0))
        .constrain_to(rect)
        .vscroll(true)
        .collapsible(false)
}
impl ResultsUi {
    pub fn busy(&self) -> bool {
        self.job.running() || self.chooser.is_some() || self.overwrite.is_some()
    }
    fn choose(&mut self, action: impl FnOnce() -> Picked + Send + 'static) {
        if self.busy() {
            return;
        }
        let (sender, receiver) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = sender.send(action());
        });
        self.chooser = Some(receiver);
    }
    fn destination(&mut self, draft: Draft, extension: &'static str, filename: &'static str) {
        self.choose(move || {
            let path = rfd::FileDialog::new()
                .set_title("저장 위치 선택")
                .add_filter(extension, &[extension])
                .set_file_name(filename)
                .save_file();
            match path {
                Some(path) => match path.try_exists() {
                    Ok(exists) => Picked::Write(draft, path, exists),
                    Err(error) => Picked::Failed(error.to_string()),
                },
                None => Picked::Cancelled,
            }
        });
    }
    pub fn poll(&mut self) -> bool {
        if let Some(receiver) = &self.chooser {
            let picked = match receiver.try_recv() {
                Ok(value) => Some(value),
                Err(TryRecvError::Empty) => None,
                Err(TryRecvError::Disconnected) => Some(Picked::Failed(
                    "파일 선택 창이 예기치 않게 종료되었습니다".into(),
                )),
            };
            if let Some(picked) = picked {
                self.chooser = None;
                match picked {
                    Picked::Write(draft, path, true) => self.overwrite = Some((draft, path)),
                    Picked::Write(draft, path, false) => self.start(draft.task(path, false)),
                    Picked::Open(path) => self.start(Task::Load(path)),
                    Picked::Before(path) => self.before = Some(path),
                    Picked::After(path) => self.after = Some(path),
                    Picked::Failed(error) => self.job.status = Status::Failed(error),
                    Picked::Cancelled => {}
                }
            }
        }
        let changed = self.job.poll();
        if changed {
            self.filter_key = None;
            self.query.clear();
            self.kind = None;
            self.folders_only = false;
        }
        changed
    }
    fn start(&mut self, task: Task) {
        if let Err(error) = self.job.start(task) {
            self.job.status = Status::Failed(error.to_string());
        }
    }
    pub fn menu(
        &mut self,
        ui: &mut egui::Ui,
        scanning: bool,
        record: Option<Arc<SavedScan>>,
        available: bool,
        archived: bool,
    ) {
        ui.add_enabled_ui(!scanning && !self.busy(), |ui| {
            ui.menu_button("결과", |ui| {
                if ui
                    .add_enabled(available, egui::Button::new("내보내기…"))
                    .clicked()
                {
                    self.export_open = true;
                    ui.close();
                }
                if ui
                    .add_enabled(record.is_some(), egui::Button::new("분석 결과 저장…"))
                    .clicked()
                {
                    self.destination(Draft::Save(record.unwrap()), "dwsnap", "DiskWeave.dwsnap");
                    ui.close();
                }
                if ui
                    .add_enabled(!archived, egui::Button::new("저장된 결과 열기…"))
                    .on_disabled_hover_text("현재 분석으로 돌아간 뒤 이용하세요.")
                    .clicked()
                {
                    self.choose(|| {
                        rfd::FileDialog::new()
                            .set_title("저장된 분석 결과 열기")
                            .add_filter("DiskWeave 분석 결과", &["dwsnap"])
                            .pick_file()
                            .map(Picked::Open)
                            .unwrap_or(Picked::Cancelled)
                    });
                    ui.close();
                }
                if ui
                    .add_enabled(!archived, egui::Button::new("결과 비교…"))
                    .on_disabled_hover_text("현재 분석으로 돌아간 뒤 이용하세요.")
                    .clicked()
                {
                    self.compare_open = true;
                    ui.close();
                }
            });
        });
    }
    pub fn status(&mut self, ui: &mut egui::Ui) {
        if self.job.running() {
            ui.horizontal_wrapped(|ui| {
                crate::ui_style::activity_indicator(ui);
                ui.label(self.job.stage());
                if ui.button("작업 취소").clicked() {
                    self.job.cancel();
                }
            });
            ui.ctx()
                .request_repaint_after(std::time::Duration::from_millis(30));
        } else if self.chooser.is_some() {
            ui.label("파일 선택 창에서 위치를 선택하세요.");
            ui.ctx()
                .request_repaint_after(std::time::Duration::from_millis(30));
        } else {
            match &self.job.status {
                Status::Failed(error) => {
                    ui.colored_label(egui::Color32::LIGHT_RED, error);
                }
                Status::Completed(message) => {
                    ui.label(message);
                }
                Status::Cancelled => {
                    ui.label("작업을 취소했습니다. 이전 결과는 유지됩니다.");
                }
                _ => {}
            }
        }
    }
    pub fn windows(&mut self, ctx: &egui::Context, context: &ExportContext) {
        if self.export_open {
            let mut open = true;
            let available = [
                context.selected.is_some() && context.folder.is_some(),
                context.selected.is_some(),
                context.matches.is_some(),
                !context.sources.is_empty(),
            ];
            dialog("파일 목록 내보내기",ctx,440.0).vscroll(false).open(&mut open).show(ctx, |ui| {
                ui.spacing_mut().item_spacing.y=6.0;
                egui::ScrollArea::vertical().id_salt("export_body").max_height((ctx.content_rect().height()-if matches!(self.job.status,Status::Idle){200.0}else{280.0}).max(100.0)).show(ui,|ui| {
                ui.label("저장할 범위");
                ui.small("이 창을 열었을 때의 결과를 저장합니다.");
                for (index, label) in ["현재 폴더와 하위 항목", "현재 분석 대상 전체", "현재 검색 결과", "모든 분석 위치"].into_iter().enumerate() {
                    if ui.add_enabled(available[index], egui::RadioButton::new(self.scope == index, label)).clicked() {
                        self.scope = index;
                    }
                }
                if self.scope < 2 {
                    if let Some(source) = context.selected.and_then(|id| context.sources.get(id)) {
                        let folder = if self.scope == 0 { context.folder.unwrap_or(0) } else { 0 };
                        let path = source.scan.folders[folder].path.display().to_string();
                        ui.add(egui::Label::new(&path).truncate()).on_hover_text(path);
                    }
                }
                ui.add_space(8.0);
                ui.label("CSV · Excel에서 열 수 있는 파일 목록");
                ui.small("확인하지 못한 할당 크기와 날짜는 빈칸으로 저장합니다.");
                if context.sources.iter().any(|s|s.scan.error_count>0 || s.scan.metadata_error_count>0) {
                    ui.colored_label(egui::Color32::YELLOW,"일부 항목을 읽지 못한 분석 결과입니다.");
                }
                ui.small("파일 내용은 저장하지 않습니다. 폴더 크기는 하위 항목을 포함하므로 행의 크기를 모두 더하면 중복됩니다.");
                ui.small("CSV에서는 파일명이 수식으로 실행되지 않도록 텍스트로 저장합니다. 이름·전체 경로·종류·크기·할당 크기·수정 날짜만 저장합니다.");
                });
                self.status(ui);
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui.add_enabled(available[self.scope.min(3)] && !self.busy(), egui::Button::new(egui::RichText::new("저장 위치 선택…").color(crate::ui_style::BG)).fill(crate::ui_style::ACCENT)).clicked() {
                        let (sources, scope) = match self.scope {
                            0 => (&context.sources, ExportScope::Folder {
                                source: context.selected.unwrap(),
                                folder: context.folder.unwrap(),
                            }),
                            1 => (&context.sources, ExportScope::Location(context.selected.unwrap())),
                            2 => {
                                let (sources, hits) = context.matches.as_ref().unwrap();
                                (sources, ExportScope::Matches(hits.clone()))
                            }
                            _ => (&context.sources, ExportScope::All),
                        };
                        self.destination(Draft::Export{sources:sources.clone(),scope},"csv","DiskWeave.csv");
                        self.export_open = false;
                    }
                    if ui.button("닫기").clicked() { self.export_open = false; }
                });
            });
            self.export_open &= open && !ctx.input(|i| i.key_pressed(egui::Key::Escape));
        }
        if self.compare_open {
            let mut open = true;
            dialog("분석 결과 비교",ctx,480.0).open(&mut open).show(ctx, |ui| {
                ui.spacing_mut().item_spacing.y=6.0;
                ui.label("같은 분석 경로의 전체 결과 두 개를 선택하세요.");
                for before in [true,false] {
                    ui.label(if before {"기준 결과"}else{"새 결과"});
                    let label=if before {&self.before}else{&self.after}.as_ref().map(|p|p.display().to_string()).unwrap_or_else(||"선택한 파일 없음".into());
                    ui.horizontal(|ui| {
                        ui.add(egui::Label::new(&label).truncate()).on_hover_text(label);
                        if ui.add_enabled(!self.busy(),egui::Button::new("찾아보기…")).clicked() {
                            self.choose(move||rfd::FileDialog::new().set_title(if before{"기준 결과 선택"}else{"새 결과 선택"}).add_filter("DiskWeave 분석 결과",&["dwsnap"]).pick_file().map(|p|if before{Picked::Before(p)}else{Picked::After(p)}).unwrap_or(Picked::Cancelled));
                        }
                    });
                }
                ui.small("경로와 메타데이터를 비교합니다. 파일 내용의 동일 여부를 검사하지 않습니다.");
                self.status(ui);
                ui.horizontal(|ui| {
                    if ui.add_enabled(self.before.is_some()&&self.after.is_some()&&!self.busy(),egui::Button::new(egui::RichText::new("비교 시작").color(crate::ui_style::BG)).fill(crate::ui_style::ACCENT)).clicked() {
                        self.start(Task::Compare{before:self.before.clone().unwrap(),after:self.after.clone().unwrap()});self.compare_open=false;
                    }
                    if ui.button("닫기").clicked(){self.compare_open=false;}
                });
            });
            self.compare_open &= open && !ctx.input(|i| i.key_pressed(egui::Key::Escape));
        }
        if let Some((_, path)) = &self.overwrite {
            let mut replace = false;
            let mut cancel = false;
            egui::Window::new("같은 이름의 파일이 있습니다")
                .collapsible(false)
                .resizable(false)
                .show(ctx, |ui| {
                    ui.label(path.display().to_string());
                    ui.label("기존 파일을 새 결과로 바꾸시겠습니까?");
                    ui.horizontal(|ui| {
                        replace = ui.button("덮어쓰기").clicked();
                        cancel = ui.button("취소").clicked();
                    });
                });
            if replace {
                let (draft, path) = self.overwrite.take().unwrap();
                self.start(draft.task(path, true));
            }
            if cancel || ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
                self.overwrite = None;
            }
        }
        if !self.export_open
            && !self.compare_open
            && (!matches!(self.job.status, Status::Idle) || self.chooser.is_some())
        {
            let mut dismiss = false;
            dialog("결과 작업", ctx, 380.0)
                .anchor(egui::Align2::RIGHT_BOTTOM, egui::vec2(-16.0, -42.0))
                .default_width(380.0)
                .default_height(160.0)
                .min_height(40.0)
                .collapsible(false)
                .show(ctx, |ui| {
                    self.status(ui);
                    if !self.busy() {
                        dismiss = ui.button("확인").clicked();
                    }
                });
            if dismiss {
                self.job.status = Status::Idle;
            }
        }
    }
    pub fn comparison(&mut self, ui: &mut egui::Ui, report: &crate::comparison::Comparison) {
        use crate::comparison::ChangeKind;
        ui.spacing_mut().item_spacing = egui::vec2(8.0, 8.0);
        egui::Frame::new().fill(crate::ui_style::SURFACE).inner_margin(18).show(ui,|ui| {
            ui.set_min_width(ui.available_width());
            ui.horizontal_wrapped(|ui| {
                if ui.add_enabled(!self.busy(),egui::Button::new("← 현재 분석으로 돌아가기")).clicked(){self.job.back();}
                ui.heading("분석 결과 비교");
            });
            ui.add(egui::Label::new(report.root.display().to_string()).truncate()).on_hover_text(report.root.display().to_string());
            ui.horizontal_wrapped(|ui| {
                ui.label(format!("기준 · {}",crate::local_date::display(Some(report.before_time))));
                ui.label(format!("새 결과 · {}",crate::local_date::display(Some(report.after_time))));
            });
            ui.horizontal_wrapped(|ui| {
                ui.strong(format!("기준 {}",crate::presentation::SizeDisplay::new(report.before_bytes).text));
                ui.strong(format!("새 결과 {}",crate::presentation::SizeDisplay::new(report.after_bytes).text));
                ui.colored_label(crate::ui_style::ACCENT,format!("확인된 크기 변화 {}",signed(report.delta)));
            });
            if report.incomplete {ui.colored_label(egui::Color32::YELLOW,"누락되었거나 제외된 항목이 있습니다. 추가·삭제 여부와 폴더 크기 변화는 확인이 필요합니다.");}
            if report.volume_unverified {ui.small("드라이브 식별 정보가 없어 같은 저장 장치인지 확인하지 못했습니다. 드라이브를 교체했다면 비교 결과를 해석할 때 주의하세요.");}
            ui.small("이름 변경·이동은 추가/삭제로 표시됩니다. 할당 크기 변화는 삭제 후 확보할 공간을 뜻하지 않습니다.");
        });
        egui::ScrollArea::horizontal()
            .id_salt("comparison_controls")
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.add(
                        crate::ui_style::text_field(&mut self.query, 200.0)
                            .hint_text("변경된 경로 찾기"),
                    );
                    for (kind, label) in [
                        (None, "전체"),
                        (Some(ChangeKind::Added), "추가"),
                        (Some(ChangeKind::Removed), "삭제"),
                        (Some(ChangeKind::Modified), "정보 변경"),
                        (Some(ChangeKind::Review), "확인 필요"),
                    ] {
                        ui.selectable_value(&mut self.kind, kind, label);
                    }
                    ui.checkbox(&mut self.folders_only, "폴더만");
                })
            });
        let key = (self.query.clone(), self.kind, self.folders_only);
        if self.filter_key.as_ref() != Some(&key) {
            let query = self.query.to_lowercase();
            self.visible = report
                .changes
                .iter()
                .enumerate()
                .filter(|(_, c)| {
                    self.kind.is_none_or(|k| c.kind == k)
                        && (!self.folders_only || c.folder)
                        && c.path.to_string_lossy().to_lowercase().contains(&query)
                })
                .map(|(i, _)| i)
                .collect();
            self.filter_key = Some(key);
        }
        ui.small(format!(
            "변경 항목 {}개 · 폴더 크기는 하위 항목을 포함하므로 합산하지 않습니다.",
            self.visible.len()
        ));
        if self.visible.is_empty() {
            ui.weak("표시할 변경 항목이 없습니다.");
        }
        let width = ui.available_width().max(840.0);
        let path_width = (width - 600.0).max(240.0);
        egui::ScrollArea::both()
            .id_salt("comparison_list")
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 0.0;
                    for (w, label) in [
                        (path_width, "상대 경로"),
                        (100.0, "상태"),
                        (120.0, "기준 크기"),
                        (120.0, "새 크기"),
                        (120.0, "크기 변화"),
                        (140.0, "할당 크기 변화"),
                    ] {
                        crate::ui_style::header_cell(ui, w, label, false);
                    }
                });
                egui::ScrollArea::vertical()
                    .id_salt("comparison_rows")
                    .show_rows(ui, crate::ui_style::ROW, self.visible.len(), |ui, rows| {
                        for row in rows {
                            let c = &report.changes[self.visible[row]];
                            crate::ui_style::row(ui, row, width, false, |ui| {
                                let path = if c.path.as_os_str().is_empty() {
                                    "(분석 대상 전체)".into()
                                } else {
                                    c.path.display().to_string()
                                };
                                crate::ui_style::name_cell(ui, path_width, &path, c.folder);
                                crate::ui_style::cell(ui, 100.0, c.kind.label(), false);
                                for bytes in [c.before, c.after] {
                                    crate::ui_style::cell(
                                        ui,
                                        120.0,
                                        &bytes
                                            .map(|b| crate::presentation::SizeDisplay::new(b).text)
                                            .unwrap_or_else(|| "—".into()),
                                        true,
                                    )
                                    .on_hover_text(
                                        bytes
                                            .map(|b| format!("{b} B"))
                                            .unwrap_or_else(|| "항목 없음".into()),
                                    );
                                }
                                for (w, value) in [(120.0, c.delta), (140.0, c.allocation_delta)] {
                                    crate::ui_style::cell(
                                        ui,
                                        w,
                                        &value.map(signed).unwrap_or_else(|| "확인 불가".into()),
                                        true,
                                    )
                                    .on_hover_text(
                                        value.map(|b| format!("{b:+} B")).unwrap_or_else(|| {
                                            if w==120.0 {"추가·삭제 여부의 확인이 필요해 변화량을 확정할 수 없습니다"}else{"할당 크기를 확인하지 못했거나 비교가 불확실합니다"}.into()
                                        }),
                                    );
                                }
                            })
                            .on_hover_text(report.root.join(&c.path).display().to_string());
                        }
                    });
            });
    }
}
fn signed(bytes: i128) -> String {
    format!(
        "{}{}",
        if bytes < 0 { "−" } else { "+" },
        crate::presentation::SizeDisplay::new(bytes.unsigned_abs() as u64).text
    )
}
