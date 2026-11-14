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
        crate::metadata::Missing::NotCollected => "미수집",
        crate::metadata::Missing::Unsupported => "미지원",
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
    let size = SizeDisplay::new(total.bytes);
    let text = if total.missing() == 0 {
        size.text
    } else if total.known > 0 {
        format!("{} (일부)", size.text)
    } else if total.not_collected > 0 && total.unsupported == 0 && total.failed == 0 {
        "미수집".into()
    } else if total.unsupported > 0 && total.not_collected == 0 && total.failed == 0 {
        "미지원".into()
    } else if total.failed > 0 && total.unsupported == 0 && total.not_collected == 0 {
        "조회 실패".into()
    } else {
        "확인 불가".into()
    };
    SizeDisplay {
        text,
        exact: format!(
            "{} B 확인분 · 확인 {} · 미수집 {} · 미지원 {} · 조회 실패 {} · 기본 데이터 스트림 기준, ADS 미수집",
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
        Self {
            text: if unit == 0 {
                format!("{bytes} B")
            } else {
                format!("{value:.1} {}", units[unit])
            },
            exact: format!("{bytes} B"),
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
    values: [f32; 8],
}
impl Default for ColumnWidths {
    fn default() -> Self {
        Self {
            values: [180.0, 260.0, 120.0, 145.0, 85.0, 80.0, 80.0, 130.0],
        }
    }
}
impl ColumnWidths {
    pub fn cells(
        &self,
        ui: &mut eframe::egui::Ui,
        scan: &ScanResult,
        item: Item,
        path: bool,
        modified: bool,
        allocated: bool,
    ) {
        use eframe::egui;
        let info = detail(scan, item);
        ui.add_sized(
            [self.get(2), 26.0],
            egui::Label::new(info.size.text).truncate(),
        )
        .on_hover_text(info.size.exact);
        if allocated {
            ui.add_sized(
                [self.get(7), 26.0],
                egui::Label::new(info.allocated.text).truncate(),
            )
            .on_hover_text(info.allocated.exact);
        }
        ui.add_sized(
            [self.get(4), 26.0],
            egui::Label::new(info.parent_percentage),
        );
        for (column, count) in [(5, info.files), (6, info.folders)] {
            ui.add_sized(
                [self.get(column), 26.0],
                egui::Label::new(count.map(|v| v.to_string()).unwrap_or_else(|| "—".into()))
                    .truncate(),
            );
        }
        if path {
            ui.add_sized(
                [self.get(1), 26.0],
                egui::Label::new(info.path.display().to_string()).truncate(),
            )
            .on_hover_text(info.path.display().to_string());
        }
        if modified {
            ui.add_sized(
                [self.get(3), 26.0],
                egui::Label::new(info.modified).truncate(),
            );
        }
    }
    pub fn get(&self, column: usize) -> f32 {
        self.values[column]
    }
    pub fn set(&mut self, column: usize, width: f32) {
        self.values[column] = width.clamp(80.0, 800.0);
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
    let (allocated, created, attributes, identity, links) = match item {
        Item::Folder(id) => {
            let folder = &scan.folders[id];
            (
                folder.allocation,
                folder.created,
                folder.attributes,
                "파일에만 제공".into(),
                "—".into(),
            )
        }
        Item::File(id) => {
            let file = &scan.files[id];
            let mut allocated = crate::metadata::Allocation::default();
            allocated
                .add(file.extra.allocation)
                .expect("단일 파일 할당 크기");
            (
                allocated,
                file.created,
                file.attributes,
                field_display(file.extra.identity),
                field_display(file.extra.links),
            )
        }
    };
    let (kind, files, folders, parent) = match item {
        Item::File(id) => ("파일", None, None, Some(scan.files[id].parent)),
        Item::Folder(id) => (
            "폴더",
            Some(scan.folders[id].file_count),
            Some(scan.folders[id].folder_count),
            scan.folders[id].parent,
        ),
    };
    let bytes = crate::search::item_bytes(scan, item);
    Detail {
        allocated: allocation_display(allocated),
        created: crate::local_date::display(created),
        attributes: attributes_display(attributes),
        identity,
        links,
        path: crate::search::item_path(scan, item),
        kind,
        size: SizeDisplay::new(bytes),
        modified: crate::local_date::display(crate::search::item_modified(scan, item)),
        files,
        folders,
        parent_percentage: parent
            .map(|p| percentage(bytes, scan.folders[p].logical_bytes))
            .unwrap_or_else(|| "—".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn allocation_display_distinguishes_zero_uncollected_unsupported_and_partial_totals() {
        use crate::metadata::{Allocation, Field, Missing};
        assert_eq!(allocation_display(Allocation::default()).text, "0 B");
        let mut total = Allocation::default();
        total.add(Field::Missing(Missing::NotCollected)).unwrap();
        assert_eq!(allocation_display(total).text, "미수집");
        let mut total = Allocation::default();
        total.add(Field::Missing(Missing::Unsupported)).unwrap();
        assert_eq!(allocation_display(total).text, "미지원");
        total.add(Field::Value(4096)).unwrap();
        let shown = allocation_display(total);
        assert_eq!(shown.text, "4.0 KiB (일부)");
        assert!(shown.exact.contains("4096 B") && shown.exact.contains("미지원 1"));
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
        assert_eq!(info.modified, "미수집");
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
