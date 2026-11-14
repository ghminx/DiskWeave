use eframe::egui::{self, Color32, Response, Ui};

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
pub const ROW: f32 = 39.0;
pub const HEADER: f32 = 36.0;
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
    // Windows 설치 글꼴만 읽고 배포 파일에는 포함하지 않는 시안 글꼴 선택. 없으면 기존 시스템 대체 글꼴 유지.
    if let Some(windows) = std::env::var_os("WINDIR") {
        if let Ok(data) = std::fs::read(std::path::PathBuf::from(windows).join("Fonts/segoeui.ttf"))
        {
            ctx.add_font(egui::FontInsert {
                name: "DiskWeave Segoe UI".into(),
                data: egui::FontData::from_owned(data),
                families: vec![egui::InsertFontFamily {
                    family: egui::FontFamily::Proportional,
                    priority: egui::FontPriority::Highest,
                }],
            });
        }
    }
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

pub fn name_cell(ui: &mut Ui, width: f32, name: &str, folder: bool) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(width, ROW), egui::Sense::hover());
    icon(ui, rect.shrink2(egui::vec2(12.0, 0.0)), folder);
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
