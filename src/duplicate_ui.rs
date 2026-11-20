use crate::{
    duplicate_job::Controller,
    duplicates::{Report, Scope},
    explorer::Item,
    file_open::{OpenAction, OpenRequest},
    locations::Snapshot,
    map_job::Target,
};
use eframe::egui;
use std::{collections::HashSet, io};

pub struct Context {
    pub sources: Vec<Snapshot>,
    pub selected: Option<u64>,
    pub folder: Option<usize>,
    pub blocked: bool,
    pub archived: bool,
    pub excluded: usize,
}
#[derive(Clone, Copy)]
enum Row {
    Group(bool, usize),
    Member(bool, usize, usize),
}
#[derive(Default)]
pub struct DuplicateUi {
    scope: usize,
    expanded: HashSet<(bool, usize)>,
    rows: Vec<Row>,
    revision: Option<u64>,
    dirty: bool,
    pub picked: Option<Target>,
    error: Option<String>,
}
impl DuplicateUi {
    pub fn request(&self, context: &Context) -> io::Result<(Vec<Snapshot>, Scope, String)> {
        if context.archived {
            return Err(io::Error::other(
                "저장된 결과에서는 검사할 수 없습니다. 현재 분석으로 돌아가세요",
            ));
        }
        if context.blocked {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "진행 중인 스캔·파일 작업·실시간 갱신을 먼저 중지하세요",
            ));
        }
        if self.scope == 2 {
            if context.sources.is_empty() {
                return Err(io::Error::other("먼저 폴더를 스캔하세요"));
            }
            return Ok((context.sources.clone(), Scope::All, "모든 스캔 위치".into()));
        }
        let source = context
            .sources
            .iter()
            .find(|s| Some(s.id) == context.selected)
            .ok_or_else(|| io::Error::other("현재 위치의 완료된 분석이 없습니다"))?;
        let folder = if self.scope == 0 {
            context.folder.unwrap_or(0)
        } else {
            0
        };
        let path = source
            .scan
            .folders
            .get(folder)
            .ok_or_else(|| io::Error::other("현재 폴더가 분석 결과에 없습니다"))?
            .path
            .display()
            .to_string();
        Ok((
            vec![source.clone()],
            Scope::Folder {
                location: source.id,
                folder,
            },
            path,
        ))
    }
    pub fn target(&self, report: &Report, key: (usize, usize), stale: bool) -> io::Result<Target> {
        if stale {
            return Err(io::Error::other(
                "분석이 바뀌었습니다. 중복 검사를 다시 실행하세요",
            ));
        }
        let s = report
            .sources
            .get(key.0)
            .ok_or_else(|| io::Error::other("검사 결과에 없는 위치"))?;
        if key.1 >= s.scan.files.len() {
            return Err(io::Error::other("검사 결과에 없는 파일"));
        }
        Ok((s.id, s.generation, Item::File(key.1)))
    }
    pub fn open(
        &self,
        report: &Report,
        key: (usize, usize),
        stale: bool,
        action: OpenAction,
    ) -> io::Result<OpenRequest> {
        let (_, _, item) = self.target(report, key, stale)?;
        if action == OpenAction::OpenFolder {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "파일 열기 동작이 필요합니다",
            ));
        }
        Ok(OpenRequest::for_item(
            &report.sources[key.0].scan,
            item,
            action,
        ))
    }
    fn rebuild(&mut self, report: &Report) {
        self.rows.clear();
        for (links, groups) in [(false, &report.groups), (true, &report.links)] {
            for (g, group) in groups.iter().enumerate() {
                self.rows.push(Row::Group(links, g));
                if self.expanded.contains(&(links, g)) {
                    for m in 0..group.len() {
                        self.rows.push(Row::Member(links, g, m));
                    }
                }
            }
        }
        self.dirty = false;
    }
    pub fn show(
        &mut self,
        ui: &mut egui::Ui,
        job: &mut Controller,
        context: &Context,
    ) -> (Option<OpenRequest>, Option<Target>) {
        let mut opened = None;
        let mut analyze = None;
        let compact = ui.available_height() < 400.0;
        ui.spacing_mut().item_spacing.y = if compact { 4.0 } else { 7.0 };
        ui.horizontal_wrapped(|ui| {
            ui.add_enabled_ui(!job.running(), |ui| {
                egui::ComboBox::from_id_salt("duplicate_scope")
                    .selected_text(
                        ["현재 폴더 아래", "현재 스캔 위치 전체", "모든 스캔 위치"][self.scope],
                    )
                    .show_ui(ui, |ui| {
                        for (i, label) in
                            ["현재 폴더 아래", "현재 스캔 위치 전체", "모든 스캔 위치"]
                                .iter()
                                .enumerate()
                        {
                            ui.selectable_value(&mut self.scope, i, *label);
                        }
                    });
            });
            if job.running() {
                crate::ui_style::activity_indicator(ui);
                if ui.button("검사 취소").clicked() {
                    job.cancel();
                }
            } else if ui
                .add_enabled(
                    !context.blocked && !context.archived,
                    egui::Button::new("검사 시작"),
                )
                .clicked()
            {
                self.error = match self.request(context).and_then(|(sources, scope, label)| {
                    job.start(sources, scope, label, context.blocked)
                }) {
                    Ok(()) => {
                        self.picked = None;
                        None
                    }
                    Err(e) => Some(e.to_string()),
                };
            }
            if !job.status.is_empty() {
                ui.label(&job.status).on_hover_text(
                    "파일 내용은 검사 당시를 기준으로 비교하며 자동 삭제하지 않습니다",
                );
            }
        });
        if !compact || job.report.is_none() {
            ui.small("파일 내용을 읽어 비교합니다 · 기본 파일 내용 기준 · 자동 삭제 없음");
        }
        if context.archived {
            ui.weak("저장된 결과에서는 검사할 수 없습니다");
        }
        if context.blocked {
            ui.colored_label(
                crate::ui_style::MUTED,
                "스캔·파일 작업·실시간 갱신을 마치거나 중지한 뒤 검사하세요",
            );
        }
        if context.excluded > 0 {
            ui.weak(format!(
                "미수집·갱신 실패 위치 {}곳은 검사 범위에서 제외됩니다",
                context.excluded
            ));
        }
        if let Some(e) = &self.error {
            ui.colored_label(egui::Color32::LIGHT_RED, e);
        }
        if job.running() {
            ui.label(format!("검사 범위: {}", job.scope_label));
            let p = job.progress();
            ui.label(format!(
                "{} · 확인한 파일 {}개 · 읽은 용량 {} · {:.1}초",
                p.stage,
                p.files,
                crate::presentation::SizeDisplay::new(p.bytes).text,
                job.elapsed()
            ));
        }
        if job.stale {
            self.picked = None;
            ui.colored_label(
                egui::Color32::YELLOW,
                "분석 결과가 바뀌었습니다 · 재검사 필요",
            );
        }
        let Some(report) = &job.report else {
            ui.add_space(12.0);
            ui.weak("검사를 시작하면 내용이 같은 파일을 그룹으로 표시합니다");
            return (opened, analyze);
        };
        if self.revision != Some(job.revision) {
            self.expanded.clear();
            if !report.groups.is_empty() {
                self.expanded.insert((false, 0));
            }
            self.picked = None;
            self.revision = Some(job.revision);
            self.dirty = true;
        }
        if self.dirty {
            self.rebuild(report);
        }
        if job.previous {
            ui.weak("이전 검사 결과 — 현재 진행 중인 검사의 부분 결과가 아닙니다");
        }
        ui.add(egui::Label::new(format!("결과 범위: {}", job.report_label)).truncate())
            .on_hover_text(&job.report_label);
        ui.add(egui::Label::new(format!(
            "중복 {}그룹 · Hard Link {}객체 · 빈 파일 {}개 제외 · 확인 불가 {}개",
            report.groups.len(),
            report.links.len(),
            report.empty,
            report.issues.len()
        )).truncate()).on_hover_text("중복 그룹은 전체 내용 SHA-256이 일치하는 별도 파일입니다. Hard Link는 동일 파일 객체의 여러 경로입니다. 기본 스트림만 비교하며 ADS·권한은 비교하지 않습니다.");
        if !compact {
            ui.small(
                "검사 당시 결과이며 이후 파일이 바뀔 수 있습니다. ADS·권한 등은 비교하지 않습니다.",
            );
        }
        if report.sources.iter().any(|s| s.scan.error_count > 0) {
            ui.weak("원래 분석에 누락된 항목이 있습니다. 모든 파일을 검사한 결과는 아닙니다.");
        }
        ui.weak(format!(
            "읽은 용량 {} · 확인한 파일 {}개",
            crate::presentation::SizeDisplay::new(report.progress.bytes).text,
            report.progress.files
        ));
        if !report.issues.is_empty() {
            ui.collapsing("확인하지 못한 파일", |ui| {
                egui::ScrollArea::vertical()
                    .id_salt("duplicate_issues")
                    .max_height(120.0)
                    .show_rows(ui, 30.0, report.issues.len(), |ui, range| {
                        for index in range {
                            let issue = &report.issues[index];
                            ui.add(
                                egui::Label::new(format!(
                                    "{} · {}",
                                    issue.path.display(),
                                    issue.reason
                                ))
                                .truncate(),
                            )
                            .on_hover_text(format!(
                                "{}\n{}",
                                issue.path.display(),
                                issue.reason
                            ));
                        }
                    });
            });
        }
        ui.separator();
        if self.rows.is_empty() {
            ui.weak("내용이 같은 별도 파일이 없습니다");
        }
        egui::ScrollArea::vertical()
            .id_salt("duplicate_groups")
            .auto_shrink([false, false])
            .show_rows(ui, 32.0, self.rows.len(), |ui, range| {
                for index in range {
                    let row = self.rows[index];
                    let (links, g) = match row {
                        Row::Group(l, g) | Row::Member(l, g, _) => (l, g),
                    };
                    let group = &if links { &report.links } else { &report.groups }[g];
                    match row {
                        Row::Group(_, _) => {
                            let expanded = self.expanded.contains(&(links, g));
                            let label = format!(
                                "{} {} {} · {}개 경로 · 파일당 {}",
                                if expanded { "▾" } else { "▸" },
                                if links {
                                    "Hard Link 객체"
                                } else {
                                    "동일 내용 그룹"
                                },
                                g + 1,
                                group.len(),
                                crate::presentation::SizeDisplay::new(group[0].bytes).text
                            );
                            if ui
                                .add_sized(
                                    [ui.available_width(), 30.0],
                                    egui::Button::new("")
                                        .left_text(egui::RichText::new(label).strong())
                                        .selected(expanded)
                                        .truncate(),
                                )
                                .clicked()
                            {
                                if expanded {
                                    self.expanded.remove(&(links, g));
                                } else {
                                    self.expanded.insert((links, g));
                                }
                                self.dirty = true;
                            }
                        }
                        Row::Member(_, _, m) => {
                            let member = &group[m];
                            let target = self
                                .target(report, (member.source, member.file), job.stale)
                                .ok();
                            let response = ui
                                .add_sized(
                                    [ui.available_width(), 30.0],
                                    egui::Button::new("")
                                        .left_text(member.path.display().to_string())
                                        .frame(false)
                                        .selected(target.is_some() && self.picked == target)
                                        .truncate(),
                                )
                                .on_hover_text(member.path.display().to_string());
                            if response.clicked() {
                                self.picked = target;
                            }
                            if response.double_clicked() && target.is_some() && !job.running() {
                                opened = self
                                    .open(
                                        report,
                                        (member.source, member.file),
                                        job.stale,
                                        OpenAction::OpenFile,
                                    )
                                    .ok();
                            }
                            response.context_menu(|ui| {
                                ui.spacing_mut().item_spacing.y = 8.0;
                                ui.add_enabled_ui(target.is_some() && !job.running(), |ui| {
                                    for (text, action) in [
                                        ("파일 열기", OpenAction::OpenFile),
                                        ("파일 위치 열기", OpenAction::RevealFile),
                                    ] {
                                        if ui.button(text).clicked() {
                                            opened = self
                                                .open(
                                                    report,
                                                    (member.source, member.file),
                                                    job.stale,
                                                    action,
                                                )
                                                .ok();
                                            ui.close();
                                        }
                                    }
                                    if ui.button("분석에서 보기").clicked() {
                                        analyze = target;
                                        ui.close();
                                    }
                                });
                                if ui.button("경로 복사").clicked() {
                                    self.error = crate::path_copy::copy_to_clipboard(
                                        ui.ctx(),
                                        std::slice::from_ref(&member.path),
                                    )
                                    .err()
                                    .map(|e| e.to_string());
                                    ui.close();
                                }
                            });
                        }
                    }
                }
            });
        (opened, analyze)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn all_location_actions_keep_source_paths_and_reject_stale_or_blocked_requests() {
        let a = crate::duplicates::tests::Fixture::new();
        let b = crate::duplicates::tests::Fixture::new();
        std::fs::write(a.0.join("한글 공백.txt"), b"same").unwrap();
        let expected = b.0.join("another.bin");
        std::fs::write(&expected, b"same").unwrap();
        let mut second = b.snapshot();
        second.id = 2;
        second.generation = 7;
        let context = Context {
            sources: vec![a.snapshot(), second],
            selected: Some(1),
            folder: Some(0),
            blocked: false,
            archived: false,
            excluded: 1,
        };
        let view = DuplicateUi {
            scope: 2,
            ..Default::default()
        };
        let (sources, scope, _) = view.request(&context).unwrap();
        let report = crate::duplicates::inspect(sources, scope, &|| false, &mut |_| {}).unwrap();
        assert_eq!(report.groups.len(), 1);
        assert_eq!(
            view.target(&report, (1, 0), false).unwrap(),
            (2, 7, Item::File(0))
        );
        for action in [OpenAction::OpenFile, OpenAction::RevealFile] {
            assert_eq!(
                view.open(&report, (1, 0), false, action).unwrap().path(),
                expected
            );
        }
        assert!(view
            .open(&report, (1, 0), true, OpenAction::OpenFile)
            .is_err());
        assert!(view
            .open(&report, (1, 0), false, OpenAction::OpenFolder)
            .is_err());
        let mut copied = String::new();
        let member = report.groups[0].iter().find(|m| m.source == 1).unwrap();
        crate::path_copy::copy_with(std::slice::from_ref(&member.path), |text| {
            copied = text;
            Ok(())
        })
        .unwrap();
        assert_eq!(copied, expected.to_str().unwrap());
        assert!(view
            .request(&Context {
                blocked: true,
                ..context
            })
            .is_err());
    }
    #[test]
    fn scope_uses_live_sources_and_selection_routes_to_its_own_location() {
        let fixture = crate::duplicates::tests::Fixture::new();
        std::fs::write(fixture.0.join("a"), b"same").unwrap();
        let source = fixture.snapshot();
        let context = Context {
            sources: vec![source.clone()],
            selected: Some(1),
            folder: Some(0),
            blocked: false,
            archived: false,
            excluded: 0,
        };
        let view = DuplicateUi::default();
        let (sources, scope, _) = view.request(&context).unwrap();
        assert_eq!(sources.len(), 1);
        assert_eq!(
            scope,
            Scope::Folder {
                location: 1,
                folder: 0
            }
        );
        let report = crate::duplicates::inspect(sources, scope, &|| false, &mut |_| {}).unwrap();
        assert!(view.target(&report, (9, 0), false).is_err());
        assert_eq!(
            view.target(&report, (0, 0), false).unwrap(),
            (1, 1, crate::explorer::Item::File(0))
        );
        assert!(view.target(&report, (0, 0), true).is_err());
        let archived = Context {
            archived: true,
            ..context
        };
        assert!(view.request(&archived).is_err());
    }

    #[test]
    fn compact_results_keep_controls_and_scroll_area_inside_available_height() {
        let fixture = crate::duplicates::tests::Fixture::new();
        std::fs::write(fixture.0.join("a"), b"same").unwrap();
        std::fs::write(fixture.0.join("b"), b"same").unwrap();
        let sources = vec![fixture.snapshot()];
        let mut job = Controller::default();
        job.start(
            sources.clone(),
            Scope::All,
            "긴 결과 범위 표시".repeat(12),
            false,
        )
        .unwrap();
        let limit = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while job.running() {
            job.poll(&sources);
            assert!(std::time::Instant::now() < limit);
            std::thread::yield_now();
        }
        let context = Context {
            sources,
            selected: Some(1),
            folder: Some(0),
            blocked: false,
            archived: false,
            excluded: 0,
        };
        let ctx = egui::Context::default();
        let mut view = DuplicateUi::default();
        crate::ui_style::apply(&ctx);
        let mut bottom = 0.0;
        let mut output = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(540.0, 260.0),
                )),
                ..Default::default()
            },
            |ui| {
                view.show(ui, &mut job, &context);
                bottom = ui.min_rect().bottom();
            },
        );
        output.textures_delta.clear();
        assert!(
            bottom <= 261.0,
            "작은 결과 영역을 벗어난 레이아웃: {bottom}"
        );
        let heading = output
            .shapes
            .iter()
            .filter_map(|s| match &s.shape {
                egui::Shape::Text(t) if t.galley.text().contains("동일 내용 그룹") => {
                    Some(t.pos.y)
                }
                _ => None,
            })
            .next()
            .expect("그룹 제목 렌더링");
        assert!(
            heading + 96.0 <= 260.0,
            "그룹과 파일 행을 표시할 최소 공간 확보: {heading}"
        );
    }
}
