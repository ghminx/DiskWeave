use crate::explorer::Item;
use crate::scanner::ScanResult;
use std::hash::{Hash, Hasher};

pub fn category_color(key: &std::ffi::OsStr) -> eframe::egui::Color32 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    key.hash(&mut hasher);
    let palette = [
        [57, 124, 118],
        [61, 101, 147],
        [143, 106, 61],
        [113, 96, 149],
        [70, 98, 117],
        [83, 121, 141],
        [74, 137, 142],
    ];
    let rgb = palette[hasher.finish() as usize % palette.len()];
    eframe::egui::Color32::from_rgb(rgb[0], rgb[1], rgb[2])
}

pub struct SizeDisplay {
    pub text: String,
    pub exact: String,
}
pub fn missing_display(missing: crate::metadata::Missing) -> &'static str {
    match missing {
        crate::metadata::Missing::NotCollected => "확인하지 않음",
        crate::metadata::Missing::Unsupported => "지원하지 않음",
        crate::metadata::Missing::Failed => "조회 실패",
    }
}
pub fn field_display<T: std::fmt::Display>(value: crate::metadata::Field<T>) -> String {
    match value {
        crate::metadata::Field::Value(v) => v.to_string(),
        crate::metadata::Field::Missing(reason) => missing_display(reason).into(),
    }
}
pub fn allocation_display(total: crate::metadata::Allocation) -> SizeDisplay {
    let text = if total.missing() == 0 {
        SizeDisplay::text_for(total.bytes)
    } else if total.known > 0 {
        format!("{} (일부)", SizeDisplay::text_for(total.bytes))
    } else if total.not_collected > 0 && total.unsupported == 0 && total.failed == 0 {
        "확인하지 않음".into()
    } else if total.unsupported > 0 && total.not_collected == 0 && total.failed == 0 {
        "지원하지 않음".into()
    } else if total.failed > 0 && total.unsupported == 0 && total.not_collected == 0 {
        "조회 실패".into()
    } else {
        "확인 불가".into()
    };
    SizeDisplay {
        text,
        exact: format!(
            "확인된 합계 {} B · 확인 {}개 · 확인하지 않음 {}개 · 지원하지 않음 {}개 · 조회 실패 {}개 · 기본 데이터 스트림 기준이며 추가 데이터 스트림(ADS)은 포함하지 않음",
            total.bytes, total.known, total.not_collected, total.unsupported, total.failed
        ),
    }
}
pub fn attributes_display(attributes: u32) -> String {
    let labels: Vec<_> = [
        (1, "읽기 전용"),
        (2, "숨김"),
        (4, "시스템"),
        (0x10, "폴더"),
        (0x20, "보관"),
        (0x200, "Sparse"),
        (0x800, "압축"),
        (0x4000, "암호화"),
    ]
    .into_iter()
    .filter_map(|(bit, label)| (attributes & bit != 0).then_some(label))
    .collect();
    format!(
        "{} (0x{attributes:08X})",
        if labels.is_empty() {
            "일반".into()
        } else {
            labels.join(" · ")
        }
    )
}
impl SizeDisplay {
    pub fn new(bytes: u64) -> Self {
        Self {
            text: Self::text_for(bytes),
            exact: format!("{bytes} B"),
        }
    }
    fn text_for(bytes: u64) -> String {
        let units = ["B", "KiB", "MiB", "GiB", "TiB"];
        let mut unit = 0;
        let mut value = bytes as f64;
        while value >= 1024.0 && unit < 4 {
            value /= 1024.0;
            unit += 1;
        }
        if (value * 10.0).round() >= 10240.0 && unit < 4 {
            value /= 1024.0;
            unit += 1;
        }
        if unit == 0 {
            format!("{bytes} B")
        } else {
            format!("{value:.1} {}", units[unit])
        }
    }
}
pub fn percentage(bytes: u64, parent: u64) -> String {
    if parent == 0 {
        "—".into()
    } else {
        format!("{:.1}%", bytes as f64 / parent as f64 * 100.0)
    }
}

#[derive(Clone)]
pub struct ColumnWidths {
    pub result_mode: bool,
    values: [f32; 8],
    manual: bool,
}
impl Default for ColumnWidths {
    fn default() -> Self {
        Self {
            values: [280.0, 260.0, 150.0, 145.0, 100.0, 90.0, 80.0, 130.0],
            manual: false,
            result_mode: false,
        }
    }
}
impl ColumnWidths {
    pub fn saved(&self) -> Option<[f32; 8]> {
        self.manual
            .then(|| std::array::from_fn(|i| self.values[i].clamp(80.0, 800.0)))
    }
    pub fn restore(&mut self, values: Option<[f32; 8]>) {
        if let Some(values) = values {
            for (i, v) in values.into_iter().enumerate() {
                self.set(i, v);
            }
        }
    }
    pub fn cells(
        &self,
        ui: &mut eframe::egui::Ui,
        scan: &ScanResult,
        item: Item,
        path: Option<bool>,
        modified: bool,
        allocated: bool,
    ) {
        let values = ItemSizes::read(scan, item);
        let size = values.logical();
        let width = self.get(2);
        let (rect, response) = ui.allocate_exact_size(
            eframe::egui::vec2(width, crate::ui_style::ROW),
            eframe::egui::Sense::hover(),
        );
        crate::ui_style::text(
            ui,
            rect.shrink2(eframe::egui::vec2(12.0, 0.0)),
            &size.text,
            true,
            crate::ui_style::TEXT,
            12.0,
        );
        response.on_hover_text(size.exact);
        if allocated {
            let allocation = values.allocated(scan, item);
            crate::ui_style::cell(ui, self.get(7), &allocation.text, true)
                .on_hover_text(allocation.exact);
        }
        if !self.result_mode {
            let (rect, response) = ui.allocate_exact_size(
                eframe::egui::vec2(self.get(4), crate::ui_style::ROW),
                eframe::egui::Sense::hover(),
            );
            let area = rect.shrink2(eframe::egui::vec2(8.0, 0.0));
            let label = values.parent_percentage();
            if let Some(parent) = values
                .parent_bytes
                .filter(|&parent| parent > 0 && !values.access.unreadable())
            {
                let track = eframe::egui::Rect::from_min_size(
                    area.left_center() + eframe::egui::vec2(0.0, 8.0),
                    eframe::egui::vec2(area.width(), 3.0),
                );
                let fraction = (values.bytes as f64 / parent as f64).min(1.0) as f32;
                ui.painter().rect_filled(track, 1.5, crate::ui_style::LINE);
                if fraction > 0.0 {
                    let fill = eframe::egui::Rect::from_min_size(
                        track.min,
                        eframe::egui::vec2(track.width() * fraction, track.height()),
                    );
                    ui.painter().rect_filled(fill, 1.5, crate::ui_style::ACCENT);
                }
                crate::ui_style::text(
                    ui,
                    area.translate(eframe::egui::vec2(0.0, -4.0)),
                    &label,
                    true,
                    crate::ui_style::TEXT,
                    12.0,
                );
            } else {
                crate::ui_style::text(ui, area, &label, true, crate::ui_style::TEXT, 12.0);
            }
            response.on_hover_text("상위 폴더 전체 용량에서 이 항목이 차지하는 비율");
        }
        if modified {
            let date = crate::local_date::display(crate::search::item_modified(scan, item));
            crate::ui_style::cell(ui, self.get(3), &date, false);
        }
        if !self.result_mode {
            for (column, count) in [(5, values.files), (6, values.folders)] {
                crate::ui_style::cell(
                    ui,
                    self.get(column),
                    &count
                        .map(crate::ui_style::count)
                        .unwrap_or_else(|| "—".into()),
                    true,
                );
            }
        }
        if let Some(source) = path {
            let path = crate::search::item_path(scan, item);
            if source {
                let (rect, response) = ui.allocate_exact_size(
                    eframe::egui::vec2(self.get(1), crate::ui_style::ROW),
                    eframe::egui::Sense::hover(),
                );
                let rect = rect.shrink2(eframe::egui::vec2(12.0, 0.0));
                let line = eframe::egui::Rect::from_min_size(
                    rect.min + eframe::egui::vec2(0.0, 3.0),
                    eframe::egui::vec2(rect.width(), 17.0),
                );
                let parent = path
                    .parent()
                    .filter(|p| !p.as_os_str().is_empty())
                    .unwrap_or(&path);
                crate::ui_style::text(
                    ui,
                    line,
                    &parent.display().to_string(),
                    false,
                    crate::ui_style::TEXT,
                    11.0,
                );
                let origin = format!("분석 위치: {}", scan.folders[0].path.display());
                crate::ui_style::text(
                    ui,
                    line.translate(eframe::egui::vec2(0.0, 17.0)),
                    &origin,
                    false,
                    crate::ui_style::SOFT,
                    10.0,
                );
                response.on_hover_text(format!("{}\n{origin}", path.display()));
            } else {
                let path = path.display().to_string();
                crate::ui_style::cell(ui, self.get(1), &path, false).on_hover_text(path);
            }
        }
    }
    pub fn fit(&mut self, available: f32, path: bool, modified: bool, allocated: bool) {
        if self.manual {
            return;
        }
        if self.result_mode && path {
            let extra = if modified { self.get(3) } else { 0.0 }
                + if allocated { self.get(7) } else { 0.0 };
            let base = (available - extra).max(480.0);
            self.values[0] = (base * 0.30).max(160.0);
            self.values[2] = 120.0;
            self.values[1] = (base - self.values[0] - self.values[2]).max(180.0);
            return;
        }
        if path {
            self.values[3] = 156.0;
            let extra = if modified { self.get(3) } else { 0.0 }
                + if allocated { self.get(7) } else { 0.0 };
            let space = (available - extra).max(700.0);
            self.values[1] = (space * 0.30).max(180.0);
            let base = space - self.values[1];
            for (column, fraction) in [(0, 0.38), (2, 0.22), (4, 0.16), (5, 0.12), (6, 0.12)] {
                self.values[column] = (base * fraction).max(if column == 0 { 140.0 } else { 60.0 });
            }
            return;
        }
        let extras = if path { self.get(1) } else { 0.0 }
            + if modified { self.get(3) } else { 0.0 }
            + if allocated { self.get(7) } else { 0.0 };
        let base = (available - extras).max(620.0);
        for (column, fraction) in [(0, 0.44), (2, 0.20), (4, 0.14), (5, 0.12), (6, 0.10)] {
            self.values[column] = (base * fraction).max(if column == 0 { 180.0 } else { 64.0 });
        }
    }
    pub fn total(&self, path: bool, modified: bool, allocated: bool) -> f32 {
        [0, 2, 4, 5, 6]
            .into_iter()
            .filter(|&i| !self.result_mode || i == 0 || i == 2)
            .map(|i| self.get(i))
            .sum::<f32>()
            + if path { self.get(1) } else { 0.0 }
            + if modified { self.get(3) } else { 0.0 }
            + if allocated { self.get(7) } else { 0.0 }
    }
    pub fn get(&self, column: usize) -> f32 {
        self.values[column]
    }
    pub fn set(&mut self, column: usize, width: f32) {
        self.manual = true;
        self.values[column] = width.clamp(80.0, 800.0);
    }
}

struct ItemSizes {
    bytes: u64,
    parent_bytes: Option<u64>,
    files: Option<u64>,
    folders: Option<u64>,
    access: crate::scanner::FolderAccess,
}

impl ItemSizes {
    fn read(scan: &ScanResult, item: Item) -> Self {
        use crate::scanner::FolderAccess;
        match item {
            Item::Folder(id) => {
                let folder = &scan.folders[id];
                Self {
                    bytes: folder.logical_bytes,
                    parent_bytes: folder.parent.map(|id| scan.folders[id].logical_bytes),
                    files: (!folder.access.unreadable()).then_some(folder.file_count),
                    folders: (!folder.access.unreadable()).then_some(folder.folder_count),
                    access: folder.access,
                }
            }
            Item::File(id) => {
                let file = &scan.files[id];
                Self {
                    bytes: file.logical_bytes,
                    parent_bytes: Some(scan.folders[file.parent].logical_bytes),
                    files: None,
                    folders: None,
                    access: FolderAccess::Complete,
                }
            }
        }
    }

    fn logical(&self) -> SizeDisplay {
        if self.access.unreadable() {
            return SizeDisplay {
                text: "확인 불가".into(),
                exact: self.access.label().into(),
            };
        }
        let mut size = SizeDisplay::new(self.bytes);
        if self.access == crate::scanner::FolderAccess::Partial {
            size.text.push_str(" (일부)");
            size.exact.push_str(" · 일부 누락 · 확인된 항목의 합계");
        }
        size
    }

    fn allocated(&self, scan: &ScanResult, item: Item) -> SizeDisplay {
        if self.access.unreadable() {
            return self.logical();
        }
        let total = match item {
            Item::Folder(id) => scan.folders[id].allocation,
            Item::File(id) => {
                let mut total = crate::metadata::Allocation::default();
                total
                    .add(scan.files[id].extra.allocation)
                    .expect("단일 파일 할당 크기");
                total
            }
        };
        let mut size = allocation_display(total);
        if self.access == crate::scanner::FolderAccess::Partial {
            size.text.push_str(" (일부)");
        }
        size
    }

    fn parent_percentage(&self) -> String {
        match self.parent_bytes {
            Some(parent) if !self.access.unreadable() => percentage(self.bytes, parent),
            _ => "—".into(),
        }
    }
}

pub struct Detail {
    pub allocated: SizeDisplay,
    pub created: String,
    pub attributes: String,
    pub identity: String,
    pub links: String,
    pub path: std::path::PathBuf,
    pub kind: &'static str,
    pub size: SizeDisplay,
    pub modified: String,
    pub files: Option<u64>,
    pub folders: Option<u64>,
    pub parent_percentage: String,
}
pub fn detail(scan: &ScanResult, item: Item) -> Detail {
    let values = ItemSizes::read(scan, item);
    let (kind, created, attributes, identity, links) = match item {
        Item::Folder(id) => {
            let folder = &scan.folders[id];
            (
                if values.access == crate::scanner::FolderAccess::Complete {
                    "폴더"
                } else {
                    values.access.label()
                },
                folder.created,
                folder.attributes,
                "파일에만 제공".into(),
                "—".into(),
            )
        }
        Item::File(id) => {
            let file = &scan.files[id];
            (
                "파일",
                file.created,
                file.attributes,
                field_display(file.extra.identity),
                field_display(file.extra.links),
            )
        }
    };
    Detail {
        allocated: values.allocated(scan, item),
        created: crate::local_date::display(created),
        attributes: attributes_display(attributes),
        identity,
        links,
        path: crate::search::item_path(scan, item),
        kind,
        size: values.logical(),
        modified: crate::local_date::display(crate::search::item_modified(scan, item)),
        files: values.files,
        folders: values.folders,
        parent_percentage: values.parent_percentage(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn denied_directory_is_visible_as_unknown_but_excluded_from_search() {
        use crate::scanner::{scan_unified, Parallelism};
        use std::{
            fs,
            path::{Path, PathBuf},
            time::SystemTime,
        };
        let stamp = SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root =
            std::env::temp_dir().join(format!("diskweave-access-{}-{stamp}", std::process::id()));
        let blocked = root.join("blocked");
        fs::create_dir_all(&blocked).unwrap();
        fs::write(root.join("visible.bin"), [1, 2, 3]).unwrap();
        fs::write(blocked.join("hidden.bin"), [4, 5]).unwrap();
        let user = format!(
            "{}\\{}",
            std::env::var("USERDOMAIN").unwrap(),
            std::env::var("USERNAME").unwrap()
        );
        struct Cleanup(PathBuf, String);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = std::process::Command::new("icacls")
                    .arg(self.0.join("blocked"))
                    .args(["/remove:d", &self.1])
                    .output();
                let _ = fs::remove_dir_all(&self.0);
            }
        }
        let _cleanup = Cleanup(root.clone(), user.clone());
        let denied = std::process::Command::new("icacls")
            .arg(&blocked)
            .arg("/deny")
            .arg(format!("{user}:(RD)"))
            .output()
            .unwrap();
        assert!(denied.status.success(), "{:?}", denied);
        for workers in [Parallelism::Sequential, Parallelism::Two] {
            let result = scan_unified(&root, workers, || false, |_| {}, None).unwrap();
            let id = result.folder_index(Path::new("blocked")).unwrap();
            assert_eq!(result.folders[0].logical_bytes, 3);
            assert_eq!(
                crate::presentation::detail(&result, crate::explorer::Item::Folder(id))
                    .size
                    .text,
                "확인 불가"
            );
            let input = crate::search::SearchInput {
                text: "blocked".into(),
                ..Default::default()
            };
            assert!(
                crate::search::search(&result, &input.compile(0).unwrap(), || false)
                    .unwrap()
                    .items
                    .is_empty()
            );
            assert!(
                crate::presentation::detail(&result, crate::explorer::Item::Folder(0))
                    .size
                    .exact
                    .contains("일부 누락")
            );
            assert_eq!(result.error_count, 1);
            assert!(crate::search::search(
                &result,
                &crate::search::SearchInput::default().compile(0).unwrap(),
                || false
            )
            .unwrap()
            .items
            .contains(&crate::explorer::Item::Folder(id)));
            let saved = crate::snapshot::SavedScan::new(
                std::sync::Arc::new(result),
                SystemTime::now(),
                SystemTime::now(),
                std::time::Duration::ZERO,
            );
            let mut encoded = Vec::new();
            crate::snapshot::write_to(&saved, &mut encoded, || false).unwrap();
            let restored = crate::snapshot::read_from(encoded.as_slice(), || false).unwrap();
            assert_eq!(
                crate::presentation::detail(&restored.scan, crate::explorer::Item::Folder(id))
                    .size
                    .text,
                "확인 불가"
            );
        }
    }
    #[test]
    fn allocation_display_distinguishes_zero_uncollected_unsupported_and_partial_totals() {
        use crate::metadata::{Allocation, Field, Missing};
        assert_eq!(allocation_display(Allocation::default()).text, "0 B");
        let mut total = Allocation::default();
        total.add(Field::Missing(Missing::NotCollected)).unwrap();
        assert_eq!(allocation_display(total).text, "확인하지 않음");
        let mut total = Allocation::default();
        total.add(Field::Missing(Missing::Unsupported)).unwrap();
        assert_eq!(allocation_display(total).text, "지원하지 않음");
        total.add(Field::Value(4096)).unwrap();
        let shown = allocation_display(total);
        assert_eq!(shown.text, "4.0 KiB (일부)");
        assert!(shown.exact.contains("4096 B") && shown.exact.contains("지원하지 않음 1개"));
        assert_eq!(
            attributes_display(0xA03),
            "읽기 전용 · 숨김 · Sparse · 압축 (0x00000A03)"
        );
    }
    #[test]
    fn details_distinguish_recursive_folder_counts_and_unknown_file_time() {
        let root = std::env::temp_dir().join(format!("diskweave-details-{}", std::process::id()));
        std::fs::create_dir_all(root.join("child")).unwrap();
        std::fs::write(root.join("child/model.bin"), [0; 3]).unwrap();
        std::fs::write(root.join("other.bin"), [0; 9]).unwrap();
        let mut scan = crate::scanner::scan(&root, || false).unwrap();
        let child = scan.folder_index(std::path::Path::new("child")).unwrap();
        let folder = detail(&scan, Item::Folder(child));
        assert_eq!((folder.files, folder.folders), (Some(1), Some(0)));
        assert_eq!(folder.parent_percentage, "25.0%");
        assert_eq!(detail(&scan, Item::Folder(0)).parent_percentage, "—");
        let file = scan
            .files
            .iter()
            .position(|f| f.name == "model.bin")
            .unwrap();
        scan.files[file].modified = None;
        let info = detail(&scan, Item::File(file));
        assert_eq!(info.path, root.join("child/model.bin"));
        assert_eq!(info.kind, "파일");
        assert_eq!(info.size.exact, "3 B");
        assert_eq!(info.modified, "확인 불가");
        assert_eq!((info.files, info.folders), (None, None));
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn readable_sizes_keep_exact_bytes_and_zero_parent_has_no_percentage() {
        for (bytes, expected) in [
            (0, "0 B"),
            (1023, "1023 B"),
            (1024, "1.0 KiB"),
            (1_048_576, "1.0 MiB"),
            (1_073_741_824, "1.0 GiB"),
            (1_099_511_627_776, "1.0 TiB"),
        ] {
            let size = SizeDisplay::new(bytes);
            assert_eq!(size.text, expected);
            assert_eq!(size.exact, format!("{bytes} B"));
        }
        assert_eq!(SizeDisplay::new(u64::MAX).exact, "18446744073709551615 B");
        assert_eq!(percentage(1, 4), "25.0%");
        assert_eq!(percentage(0, 0), "—");
        let mut widths = ColumnWidths::default();
        widths.set(0, -50.0);
        assert_eq!(widths.get(0), 80.0);
        widths.set(0, 9000.0);
        assert_eq!(widths.get(0), 800.0);
    }
}
