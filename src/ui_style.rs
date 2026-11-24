use eframe::egui::{self, Color32, Response, Ui};

mod file_icons;

pub const BG: Color32 = Color32::from_rgb(12, 17, 24);
pub const SURFACE: Color32 = Color32::from_rgb(18, 26, 36);
pub const SIDEBAR: Color32 = Color32::from_rgb(16, 24, 34);
pub const RAISED: Color32 = Color32::from_rgb(25, 35, 48);
pub const LINE: Color32 = Color32::from_rgb(43, 57, 72);
pub const TEXT: Color32 = Color32::from_rgb(234, 240, 246);
pub const MUTED: Color32 = Color32::from_rgb(167, 181, 197);
pub const SOFT: Color32 = Color32::from_rgb(127, 145, 164);
pub const ACCENT: Color32 = Color32::from_rgb(104, 184, 255);
pub const SELECTED: Color32 = Color32::from_rgb(27, 48, 65);
pub const ROW: f32 = 36.0;
pub const HEADER: f32 = 36.0;
pub const FIELD_HEIGHT: f32 = 32.0;
pub const MAP_COLORS: [Color32; 7] = [
    Color32::from_rgb(57, 124, 118),
    Color32::from_rgb(61, 101, 147),
    Color32::from_rgb(143, 106, 61),
    Color32::from_rgb(113, 96, 149),
    Color32::from_rgb(70, 98, 117),
    Color32::from_rgb(83, 121, 141),
    Color32::from_rgb(74, 137, 142),
];

pub fn slot(ui: &mut Ui, size: egui::Vec2, layout: egui::Layout, contents: impl FnOnce(&mut Ui)) {
    let (rect, _) = ui.allocate_exact_size(size, egui::Sense::hover());
    let mut child = ui.new_child(egui::UiBuilder::new().max_rect(rect).layout(layout));
    child.set_clip_rect(rect.intersect(ui.clip_rect()));
    contents(&mut child);
}

pub fn text_field(text: &mut String, width: f32) -> egui::TextEdit<'_> {
    egui::TextEdit::singleline(text)
        .desired_width(width)
        .min_size(egui::vec2(0.0, FIELD_HEIGHT))
        .align(egui::Align2::LEFT_CENTER)
        .margin(egui::Margin {
            left: 10,
            right: 10,
            top: 3,
            bottom: 1,
        })
}

pub fn activity_indicator(ui: &mut Ui) -> Response {
    ui.add(egui::Spinner::new().size(14.0).color(ACCENT))
}

pub fn activity_bar(ui: &mut Ui) {
    let size = egui::vec2(ui.available_width(), 4.0);
    let (track, _) = ui.allocate_exact_size(size, egui::Sense::hover());
    let position = if ui.style().animation_time == 0.0 {
        0.5
    } else {
        let cycle = (ui.input(|input| input.time) / 1.5).rem_euclid(2.0) as f32;
        cycle.min(2.0 - cycle)
    };
    let width = track.width() * 0.18;
    let left = track.left() + (track.width() - width) * position;
    let segment = egui::Rect::from_min_size(
        egui::pos2(left, track.top()),
        egui::vec2(width, track.height()),
    );
    ui.painter().rect_filled(track, 2.0, LINE);
    ui.painter().rect_filled(segment, 2.0, ACCENT);
}

pub fn apply(ctx: &egui::Context) {
    ctx.set_theme(egui::Theme::Dark);
    let mut visuals = egui::Visuals::dark();
    visuals.panel_fill = BG;
    visuals.extreme_bg_color = Color32::from_rgb(14, 22, 32);
    visuals.override_text_color = Some(TEXT);
    visuals.weak_text_color = Some(SOFT);
    visuals.widgets.inactive.bg_fill = RAISED;
    visuals.widgets.inactive.weak_bg_fill = RAISED;
    visuals.widgets.inactive.bg_stroke = egui::Stroke::new(1.0, LINE);
    visuals.widgets.hovered.bg_fill = Color32::from_rgb(34, 47, 62);
    visuals.widgets.hovered.weak_bg_fill = Color32::from_rgb(34, 47, 62);
    visuals.widgets.hovered.bg_stroke = egui::Stroke::new(1.0, ACCENT);
    visuals.widgets.active.bg_fill = SELECTED;
    visuals.selection.bg_fill = Color32::from_rgb(33, 56, 76);
    visuals.selection.stroke = egui::Stroke::new(1.0, ACCENT);
    visuals.widgets.noninteractive.bg_stroke = egui::Stroke::new(1.0, LINE);
    for widget in [
        &mut visuals.widgets.inactive,
        &mut visuals.widgets.hovered,
        &mut visuals.widgets.active,
    ] {
        widget.corner_radius = egui::CornerRadius::same(6);
    }
    ctx.set_visuals(visuals);
    ctx.global_style_mut(|style| {
        style.spacing.item_spacing = egui::vec2(8.0, 0.0);
        style.spacing.button_padding = egui::vec2(12.0, 7.0);
        style.spacing.interact_size.y = 30.0;
        style
            .text_styles
            .insert(egui::TextStyle::Body, egui::FontId::proportional(13.0));
        style
            .text_styles
            .insert(egui::TextStyle::Button, egui::FontId::proportional(12.0));
        style
            .text_styles
            .insert(egui::TextStyle::Small, egui::FontId::proportional(11.0));
    });
    for (name, bytes, family) in [
        (
            "Pretendard",
            include_bytes!("../assets/fonts/Pretendard-Regular.otf").as_slice(),
            egui::FontFamily::Proportional,
        ),
        (
            "Pretendard Bold",
            include_bytes!("../assets/fonts/Pretendard-Bold.ttf").as_slice(),
            egui::FontFamily::Name("Pretendard Bold".into()),
        ),
    ] {
        ctx.add_font(egui::FontInsert {
            name: name.into(),
            data: egui::FontData::from_static(bytes),
            families: vec![egui::InsertFontFamily {
                family,
                priority: egui::FontPriority::Highest,
            }],
        });
    }
    ctx.global_style_mut(|style| {
        style.spacing.icon_width = 18.0;
        style.spacing.icon_width_inner = 12.0;
        style.text_styles.insert(
            egui::TextStyle::Heading,
            egui::FontId::new(18.0, egui::FontFamily::Name("Pretendard Bold".into())),
        );
    });
}

pub fn text(ui: &Ui, rect: egui::Rect, value: &str, right: bool, color: Color32, size: f32) {
    let mut job = egui::text::LayoutJob::simple(
        value.into(),
        egui::FontId::proportional(size),
        color,
        rect.width().max(0.0),
    );
    job.wrap.max_rows = 1;
    job.wrap.break_anywhere = true;
    let galley = ui.fonts_mut(|fonts| fonts.layout_job(job));
    let x = if right {
        rect.right() - galley.size().x
    } else {
        rect.left()
    };
    let y = rect.center().y - galley.size().y / 2.0;
    ui.painter()
        .with_clip_rect(rect.intersect(ui.clip_rect()))
        .galley(egui::pos2(x, y), galley, color);
}

pub fn cell(ui: &mut Ui, width: f32, value: &str, right: bool) -> Response {
    let (rect, response) = ui.allocate_exact_size(egui::vec2(width, ROW), egui::Sense::hover());
    text(
        ui,
        rect.shrink2(egui::vec2(12.0, 0.0)),
        value,
        right,
        TEXT,
        12.0,
    );
    response
}

pub fn header_cell(ui: &mut Ui, width: f32, value: &str, right: bool) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(width, HEADER), egui::Sense::hover());
    ui.painter()
        .rect_filled(rect, 0.0, Color32::from_rgb(21, 30, 40));
    ui.painter().line_segment(
        [rect.left_bottom(), rect.right_bottom()],
        egui::Stroke::new(1.0, LINE),
    );
    text(
        ui,
        rect.shrink2(egui::vec2(12.0, 0.0)),
        value,
        right,
        MUTED,
        12.0,
    );
}

pub fn property(ui: &mut Ui, label: &str, value: &str) -> Response {
    let (rect, response) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 21.0), egui::Sense::hover());
    let label_rect =
        egui::Rect::from_min_max(rect.min, egui::pos2(rect.left() + 80.0, rect.bottom()));
    text(ui, label_rect, label, false, MUTED, 11.0);
    text(
        ui,
        egui::Rect::from_min_max(egui::pos2(label_rect.right(), rect.top()), rect.max),
        value,
        true,
        TEXT,
        12.0,
    );
    response.on_hover_text(value)
}

pub fn icon(ui: &Ui, rect: egui::Rect, folder: bool) {
    let p = rect.left_center() + egui::vec2(0.0, -6.0);
    let points = if folder {
        vec![
            p,
            p + egui::vec2(5.0, 0.0),
            p + egui::vec2(7.0, 2.0),
            p + egui::vec2(14.0, 2.0),
            p + egui::vec2(14.0, 12.0),
            p + egui::vec2(0.0, 12.0),
            p,
        ]
    } else {
        vec![
            p,
            p + egui::vec2(8.0, 0.0),
            p + egui::vec2(12.0, 4.0),
            p + egui::vec2(12.0, 14.0),
            p + egui::vec2(0.0, 14.0),
            p,
        ]
    };
    ui.painter().add(egui::Shape::line(
        points,
        egui::Stroke::new(
            1.2,
            if folder {
                Color32::from_rgb(130, 189, 235)
            } else {
                MUTED
            },
        ),
    ));
}

pub fn entry_icon(ui: &Ui, rect: egui::Rect, name: &str, folder: bool) {
    if folder || !file_icons::paint(ui, rect.left_center(), name) {
        icon(ui, rect, folder);
    }
}

pub fn name_cell(ui: &mut Ui, width: f32, name: &str, folder: bool) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(width, ROW), egui::Sense::hover());
    entry_icon(ui, rect.shrink2(egui::vec2(12.0, 0.0)), name, folder);
    text(
        ui,
        egui::Rect::from_min_max(
            rect.min + egui::vec2(38.0, 0.0),
            rect.max - egui::vec2(12.0, 0.0),
        ),
        name,
        false,
        TEXT,
        12.0,
    );
}

pub fn row(
    ui: &mut Ui,
    id: usize,
    width: f32,
    selected: bool,
    contents: impl FnOnce(&mut Ui),
) -> Response {
    let background = ui.painter().add(egui::Shape::Noop);
    let output = ui.allocate_ui_with_layout(
        egui::vec2(width, ROW),
        egui::Layout::left_to_right(egui::Align::Center),
        |ui| {
            ui.spacing_mut().item_spacing = egui::Vec2::ZERO;
            contents(ui);
        },
    );
    let rect = egui::Rect::from_min_size(output.response.rect.min, egui::vec2(width, ROW));
    let response = ui.interact(
        rect,
        ui.scope_id().with(("table_row", id)),
        egui::Sense::click(),
    );
    if selected || response.hovered() {
        ui.painter().set(
            background,
            egui::Shape::rect_filled(
                rect,
                0.0,
                if selected {
                    SELECTED
                } else {
                    Color32::from_rgb(19, 35, 48)
                },
            ),
        );
    }
    ui.painter().line_segment(
        [rect.left_bottom(), rect.right_bottom()],
        egui::Stroke::new(1.0, Color32::from_rgb(29, 41, 53)),
    );
    response.on_hover_cursor(egui::CursorIcon::PointingHand)
}

pub fn count(value: u64) -> String {
    let digits = value.to_string();
    let mut result = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            result.push(',');
        }
        result.push(ch);
    }
    result
}

pub fn panel_title(ui: &mut Ui, title: &str, note: &str) {
    let (rect, _) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 48.0), egui::Sense::hover());
    text(
        ui,
        rect.shrink2(egui::vec2(17.0, 0.0)),
        title,
        false,
        TEXT,
        13.0,
    );
    text(
        ui,
        rect.shrink2(egui::vec2(17.0, 0.0)),
        note,
        true,
        SOFT,
        11.0,
    );
    ui.painter().line_segment(
        [rect.left_bottom(), rect.right_bottom()],
        egui::Stroke::new(1.0, LINE),
    );
}

/// 제목 표시줄의 고정 크기 아이콘 버튼 생성.
pub fn title_control(ui: &mut Ui, name: &str) -> Response {
    let (rect, response) = ui.allocate_exact_size(egui::vec2(38.0, 28.0), egui::Sense::click());
    if response.hovered() {
        ui.painter().rect_filled(
            rect,
            3.0,
            if name == "닫기" {
                Color32::from_rgb(174, 48, 60)
            } else {
                SELECTED
            },
        );
    }
    let c = rect.center();
    let stroke = egui::Stroke::new(1.3, TEXT);
    let p = ui.painter();
    match name {
        "닫기" => {
            p.line_segment(
                [c + egui::vec2(-4.5, -4.5), c + egui::vec2(4.5, 4.5)],
                stroke,
            );
            p.line_segment(
                [c + egui::vec2(-4.5, 4.5), c + egui::vec2(4.5, -4.5)],
                stroke,
            );
        }
        "최소화" => {
            p.line_segment(
                [c + egui::vec2(-5.0, 0.0), c + egui::vec2(5.0, 0.0)],
                stroke,
            );
        }
        "최대화" | "복원" => {
            p.rect_stroke(
                egui::Rect::from_center_size(c, egui::vec2(10.0, 9.0)),
                0.0,
                stroke,
                egui::StrokeKind::Inside,
            );
            if name == "복원" {
                p.line_segment(
                    [c + egui::vec2(-3.0, -7.0), c + egui::vec2(7.0, -7.0)],
                    stroke,
                );
            }
        }
        "더보기" => {
            for x in [-4.0, 0.0, 4.0] {
                p.circle_filled(c + egui::vec2(x, 0.0), 1.2, TEXT);
            }
        }
        _ => {
            p.circle_stroke(c, 4.0, stroke);
            p.circle_stroke(c, 1.5, stroke);
            for i in 0..8 {
                let a = i as f32 * std::f32::consts::FRAC_PI_4;
                let v = egui::vec2(a.cos(), a.sin());
                p.line_segment([c + v * 4.0, c + v * 6.0], stroke);
            }
        }
    }
    response.on_hover_text(name)
}

/// 폴더 펼침 상태를 글꼴에 의존하지 않는 12px 꺾쇠로 표시.
pub fn chevron(ui: &Ui, rect: egui::Rect, expanded: bool) {
    let c = rect.center();
    let points = if expanded {
        [
            c + egui::vec2(-5.0, -2.5),
            c + egui::vec2(0.0, 2.5),
            c + egui::vec2(5.0, -2.5),
        ]
    } else {
        [
            c + egui::vec2(-2.5, -5.0),
            c + egui::vec2(2.5, 0.0),
            c + egui::vec2(-2.5, 5.0),
        ]
    };
    ui.painter().add(egui::Shape::line(
        points.to_vec(),
        egui::Stroke::new(1.8, MUTED),
    ));
}

/// 이전 방향을 나타내는 고정 크기 왼쪽 화살표 버튼 생성.
pub fn navigation_button(ui: &mut Ui, enabled: bool, direction: i8) -> Response {
    let response = ui.add_enabled(
        enabled,
        egui::Button::new("").min_size(egui::vec2(34.0, 30.0)),
    );
    let c = response.rect.center();
    let stroke = egui::Stroke::new(1.8, if enabled { TEXT } else { SOFT });
    let point = |x: f32, y: f32| {
        c + match direction {
            1 => egui::vec2(-x, y),
            0 => egui::vec2(-y, x),
            _ => egui::vec2(x, y),
        }
    };
    ui.painter()
        .line_segment([point(-6.0, 0.0), point(6.0, 0.0)], stroke);
    ui.painter().add(egui::Shape::line(
        vec![point(-1.0, -5.0), point(-6.0, 0.0), point(-1.0, 5.0)],
        stroke,
    ));
    response.on_hover_text(match direction {
        1 => "앞으로",
        0 => "상위 폴더",
        _ => "뒤로",
    })
}

/// 우클릭 메뉴의 항목 높이와 그룹 간격 통일.
pub fn menu_spacing(ui: &mut Ui) {
    ui.set_min_width(220.0);
    ui.spacing_mut().item_spacing.y = 4.0;
    ui.spacing_mut().interact_size.y = 34.0;
    ui.spacing_mut().button_padding = egui::vec2(12.0, 8.0);
}

/// 설명과 독립적인 선형 아이콘을 함께 표시하는 메뉴 행 생성.
pub fn menu_action(ui: &mut Ui, label: &str, enabled: bool) -> Response {
    menu_spacing(ui);
    let response = ui.add_enabled(
        enabled,
        egui::Button::new(egui::RichText::new(label).color(Color32::TRANSPARENT))
            .min_size(egui::vec2(220.0, 36.0)),
    );
    let danger = label.contains("이름 변경")
        || label.contains("이동") && !label.contains("분석")
        || label.contains("휴지통");
    let color = if !enabled {
        SOFT
    } else if danger {
        Color32::from_rgb(255, 145, 140)
    } else {
        ACCENT
    };
    let p = ui.painter();
    p.text(
        egui::pos2(response.rect.left() + 38.0, response.rect.center().y),
        egui::Align2::LEFT_CENTER,
        label,
        egui::FontId::proportional(13.0),
        if !enabled {
            SOFT
        } else if danger {
            color
        } else {
            TEXT
        },
    );
    let c = egui::pos2(response.rect.left() + 18.0, response.rect.center().y);
    let stroke = egui::Stroke::new(1.5, color);
    if matches!(label, "새 탭에서 열기" | "오른쪽에 열기") {
        let frame = egui::Rect::from_center_size(c, egui::vec2(15.0, 11.0));
        p.rect_stroke(frame, 1.5, stroke, egui::StrokeKind::Inside);
        if label == "오른쪽에 열기" {
            p.vline(c.x, frame.y_range().shrink(1.0), stroke);
        } else {
            p.hline(frame.x_range().shrink(1.0), frame.top() + 3.5, stroke);
        }
    } else if label.contains("휴지통") {
        p.rect_stroke(
            egui::Rect::from_center_size(c + egui::vec2(0.0, 1.0), egui::vec2(10.0, 12.0)),
            1.0,
            stroke,
            egui::StrokeKind::Inside,
        );
        p.line_segment(
            [c + egui::vec2(-7.0, -6.0), c + egui::vec2(7.0, -6.0)],
            stroke,
        );
        p.line_segment(
            [c + egui::vec2(-2.0, -8.0), c + egui::vec2(2.0, -8.0)],
            stroke,
        );
    } else if label.contains("이름 변경") {
        p.line_segment(
            [c + egui::vec2(-5.0, 5.0), c + egui::vec2(5.0, -5.0)],
            egui::Stroke::new(3.0, color),
        );
        p.line_segment(
            [c + egui::vec2(-7.0, 8.0), c + egui::vec2(7.0, 8.0)],
            stroke,
        );
    } else if label.contains("이동") || label.contains("가져오기") {
        p.line_segment(
            [c + egui::vec2(-7.0, 0.0), c + egui::vec2(7.0, 0.0)],
            stroke,
        );
        p.add(egui::Shape::line(
            vec![
                c + egui::vec2(2.0, -5.0),
                c + egui::vec2(7.0, 0.0),
                c + egui::vec2(2.0, 5.0),
            ],
            stroke,
        ));
    } else if label.contains("경로") {
        for x in [-4.0, 4.0] {
            p.rect_stroke(
                egui::Rect::from_center_size(c + egui::vec2(x, 0.0), egui::vec2(10.0, 7.0)),
                3.0,
                stroke,
                egui::StrokeKind::Inside,
            );
        }
    } else if label.contains("복사") {
        for offset in [egui::vec2(-2.0, -2.0), egui::vec2(2.0, 2.0)] {
            p.rect_stroke(
                egui::Rect::from_center_size(c + offset, egui::vec2(9.0, 11.0)),
                1.0,
                stroke,
                egui::StrokeKind::Inside,
            );
        }
    } else {
        p.rect_stroke(
            egui::Rect::from_center_size(c, egui::vec2(15.0, 11.0)),
            1.0,
            stroke,
            egui::StrokeKind::Inside,
        );
        p.line_segment(
            [c + egui::vec2(-7.0, -7.0), c + egui::vec2(-1.0, -7.0)],
            stroke,
        );
        if label.contains("새 폴더") {
            p.line_segment(
                [c + egui::vec2(-3.0, 0.0), c + egui::vec2(3.0, 0.0)],
                stroke,
            );
            p.line_segment(
                [c + egui::vec2(0.0, -3.0), c + egui::vec2(0.0, 3.0)],
                stroke,
            );
        }
    }
    response
}

/// 12px 꺾쇠와 왼쪽 정렬 제목을 사용하는 고정 높이 선택 버튼 생성.
pub fn scope_button(ui: &mut Ui, label: &str, width: f32) -> Response {
    let response = ui.add_sized(
        [width, FIELD_HEIGHT],
        egui::Button::new(egui::RichText::new(label).color(Color32::TRANSPARENT)),
    );
    let rect = response.rect;
    text(
        ui,
        egui::Rect::from_min_max(
            rect.min + egui::vec2(10.0, 0.0),
            rect.max - egui::vec2(30.0, 0.0),
        ),
        label,
        false,
        TEXT,
        12.0,
    );
    chevron(
        ui,
        egui::Rect::from_center_size(
            egui::pos2(rect.right() - 16.0, rect.center().y),
            egui::vec2(18.0, 18.0),
        ),
        true,
    );
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_field_and_scope_share_a_centered_control_row() {
        let ctx = egui::Context::default();
        apply(&ctx);
        let mut output = ctx.run_ui(egui::RawInput::default(), |ui| {
            ui.horizontal(|ui| {
                let scope = scope_button(ui, "현재 폴더 안에서", 170.0);
                let mut value = String::from("한글 경로");
                let field = text_field(&mut value, 160.0).show(ui);
                let bounds = field.response.rect;
                assert!((bounds.width() - 160.0).abs() <= 1.0);
                assert!((bounds.height() - scope.rect.height()).abs() <= 1.0);
                assert!((bounds.center().y - scope.rect.center().y).abs() <= 1.0);
                let text_center = field.galley_pos.y + field.galley.size().y / 2.0;
                assert!((text_center - bounds.center().y).abs() <= 2.0);
            });
        });
        output.textures_delta.clear();
    }

    #[test]
    fn file_icons_distinguish_known_types_and_keep_a_generic_fallback() {
        let render = |name, folder| {
            let ctx = egui::Context::default();
            apply(&ctx);
            let mut output = ctx.run_ui(egui::RawInput::default(), |ui| {
                name_cell(ui, 220.0, name, folder);
            });
            output.textures_delta.clear();
            output
                .shapes
                .into_iter()
                .map(|shape| shape.shape)
                .filter(|shape| !matches!(shape, egui::Shape::Text(_)))
                .collect::<Vec<_>>()
        };
        let generic = render("파일.unknown", false);
        assert_eq!(generic, render("LICENSE", false));
        assert_eq!(generic, render("메모.txt", false));
        assert_eq!(render("파일.PDF", false), render("파일.pdf", false));
        assert_eq!(render("파일.pdf", true), render("폴더", true));
        let mut kinds = vec![generic, render("폴더", true)];
        for name in [
            "보고서.docx",
            "보고서.pdf",
            "표.csv",
            "발표.pptx",
            "사진.psd",
            "영상.mp4",
            "음성.m4a",
            "묶음.7z",
            "설정.json",
        ] {
            let icon = render(name, false);
            assert!(!kinds.contains(&icon), "{name}: 기존 유형과 아이콘이 같음");
            kinds.push(icon);
        }
    }

    #[test]
    fn activity_indicator_fits_in_a_compact_centered_status_row() {
        let ctx = egui::Context::default();
        apply(&ctx);
        let mut output = ctx.run_ui(egui::RawInput::default(), |ui| {
            ui.horizontal(|ui| {
                let indicator = activity_indicator(ui);
                let label = ui.small("스캔 중");
                assert_eq!(indicator.rect.size(), egui::vec2(14.0, 14.0));
                assert!((indicator.rect.center().y - label.rect.center().y).abs() <= 1.0);
            });
        });
        output.textures_delta.clear();
    }
}
