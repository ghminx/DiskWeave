use eframe::egui::{self, Color32, Pos2, Shape, Stroke, Ui};

#[derive(Clone, Copy)]
enum FileType {
    Document,
    Pdf,
    Table,
    Presentation,
    Image,
    Video,
    Audio,
    Archive,
    Code,
}

const TYPES: &[(&[&str], FileType)] = &[
    (
        &["doc", "docx", "hwp", "hwpx", "odt", "rtf"],
        FileType::Document,
    ),
    (&["pdf"], FileType::Pdf),
    (&["csv", "tsv", "xls", "xlsx", "ods"], FileType::Table),
    (&["ppt", "pptx", "odp"], FileType::Presentation),
    (
        &["png", "jpg", "jpeg", "gif", "webp", "bmp", "svg", "psd"],
        FileType::Image,
    ),
    (&["mp4", "mkv", "mov", "avi", "webm"], FileType::Video),
    (&["mp3", "m4a", "wav", "flac", "ogg"], FileType::Audio),
    (&["zip", "7z", "rar", "tar", "gz"], FileType::Archive),
    (
        &[
            "rs", "py", "js", "mjs", "cjs", "ts", "tsx", "json", "ps1", "c", "cpp", "html", "css",
        ],
        FileType::Code,
    ),
];

pub(super) fn paint(ui: &Ui, left_center: Pos2, name: &str) -> bool {
    let Some(extension) = std::path::Path::new(name)
        .extension()
        .and_then(|ext| ext.to_str())
    else {
        return false;
    };
    let Some((_, kind)) = TYPES.iter().find(|(suffixes, _)| {
        suffixes
            .iter()
            .any(|suffix| extension.eq_ignore_ascii_case(suffix))
    }) else {
        return false;
    };

    let origin = left_center - egui::vec2(0.0, 7.0);
    let pos = |x, y| origin + egui::vec2(x, y);
    let rect =
        |left, top, right, bottom| egui::Rect::from_min_max(pos(left, top), pos(right, bottom));
    let painter = ui.painter();
    let fill = match kind {
        FileType::Document => (70, 129, 186),
        FileType::Pdf => (172, 71, 80),
        FileType::Table => (45, 128, 98),
        FileType::Presentation => (165, 89, 61),
        FileType::Image => (119, 93, 171),
        FileType::Video => (60, 115, 159),
        FileType::Audio => (69, 116, 150),
        FileType::Archive => (147, 106, 40),
        FileType::Code => (105, 106, 138),
    };
    painter.rect_filled(
        rect(0.0, 0.0, 14.0, 14.0),
        3.0,
        Color32::from_rgb(fill.0, fill.1, fill.2),
    );
    let ink = super::TEXT;
    let line = |x1, y1, x2, y2| {
        painter.line_segment([pos(x1, y1), pos(x2, y2)], Stroke::new(1.1, ink));
    };
    match kind {
        FileType::Document => {
            line(3.5, 4.0, 10.5, 4.0);
            line(3.5, 7.0, 10.5, 7.0);
            line(3.5, 10.0, 8.0, 10.0);
        }
        FileType::Pdf => {
            painter.text(
                pos(7.0, 7.0),
                egui::Align2::CENTER_CENTER,
                "PDF",
                egui::FontId::monospace(6.0),
                ink,
            );
        }
        FileType::Table => {
            painter.rect_stroke(
                rect(3.0, 3.0, 11.0, 11.0),
                0.0,
                Stroke::new(1.0, ink),
                egui::StrokeKind::Inside,
            );
            line(7.0, 3.0, 7.0, 11.0);
            line(3.0, 7.0, 11.0, 7.0);
        }
        FileType::Presentation => {
            painter.rect_stroke(
                rect(3.0, 3.0, 11.0, 9.0),
                0.0,
                Stroke::new(1.0, ink),
                egui::StrokeKind::Inside,
            );
            line(7.0, 9.0, 7.0, 11.5);
            line(5.0, 11.5, 9.0, 11.5);
        }
        FileType::Image => {
            painter.circle_filled(pos(9.5, 4.5), 1.2, ink);
            painter.add(Shape::convex_polygon(
                vec![pos(2.5, 11.0), pos(6.0, 6.0), pos(11.5, 11.0)],
                ink,
                Stroke::NONE,
            ));
        }
        FileType::Video => {
            painter.add(Shape::convex_polygon(
                vec![pos(5.0, 3.0), pos(11.0, 7.0), pos(5.0, 11.0)],
                ink,
                Stroke::NONE,
            ));
        }
        FileType::Audio => {
            for (x, height) in [(3.0, 2.0), (5.5, 6.0), (8.0, 9.0), (10.5, 4.0)] {
                line(x, 7.0 - height / 2.0, x, 7.0 + height / 2.0);
            }
        }
        FileType::Archive => {
            for y in [3.0, 5.0, 7.0] {
                line(6.0, y, 8.0, y);
            }
            painter.rect_filled(rect(6.0, 8.5, 9.0, 11.5), 1.0, ink);
        }
        FileType::Code => {
            for (outer, inner) in [(2.5, 5.5), (11.5, 8.5)] {
                line(inner, 4.0, outer, 7.0);
                line(outer, 7.0, inner, 10.0);
            }
        }
    }
    true
}
