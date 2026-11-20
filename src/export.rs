use crate::explorer::Item;
use crate::global_search::Hit;
use crate::locations::Snapshot;
use std::borrow::Cow;
use std::io::{self, Write};
use std::path::Path;

#[derive(Clone)]
pub enum ExportScope {
    Folder { source: usize, folder: usize },
    Location(usize),
    Matches(Vec<Hit>),
    All,
}
fn rows(
    sources: &[Snapshot],
    scope: &ExportScope,
    cancelled: &impl Fn() -> bool,
) -> io::Result<Vec<Hit>> {
    let mut hits = Vec::new();
    if let ExportScope::Matches(items) = scope {
        let mut seen = std::collections::HashSet::new();
        for hit in items {
            crate::output::check(cancelled)?;
            let source = sources.get(hit.source).ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "내보낼 분석 위치가 없습니다")
            })?;
            let valid = match hit.item {
                Item::Folder(id) => id < source.scan.folders.len(),
                Item::File(id) => id < source.scan.files.len(),
            };
            if !valid {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "내보낼 항목이 현재 결과에 없습니다",
                ));
            }
            if seen.insert((
                hit.source,
                matches!(hit.item, Item::File(_)),
                match hit.item {
                    Item::File(id) | Item::Folder(id) => id,
                },
            )) {
                hits.push(*hit);
            }
        }
        return Ok(hits);
    }
    for (source, s) in sources.iter().enumerate() {
        let folder = match scope {
            ExportScope::Folder {
                source: wanted,
                folder,
            } if *wanted == source => *folder,
            ExportScope::Location(wanted) if *wanted == source => 0,
            ExportScope::All => 0,
            _ => continue,
        };
        if folder >= s.scan.folders.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "내보낼 폴더가 없습니다",
            ));
        }
        let scan = &s.scan;
        let mut pending = vec![folder];
        while let Some(id) = pending.pop() {
            crate::output::check(cancelled)?;
            hits.push(Hit {
                source,
                item: Item::Folder(id),
            });
            hits.extend(scan.file_children[id].iter().map(|&id| Hit {
                source,
                item: Item::File(id),
            }));
            pending.extend(scan.children[id].iter().rev());
        }
    }
    if hits.is_empty() && !matches!(scope, ExportScope::All) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "내보낼 분석 위치가 없습니다",
        ));
    }
    Ok(hits)
}
fn text(writer: &mut dyn Write, value: &str) -> io::Result<()> {
    let guarded = matches!(
        value.trim_start().chars().next(),
        Some('=' | '+' | '-' | '@')
    ) || value.starts_with(['\t', '\r', '\n']);
    let value = if value.contains('"') {
        Cow::Owned(value.replace('"', "\"\""))
    } else {
        Cow::Borrowed(value)
    };
    write!(writer, "\"{}{}\"", if guarded { "'" } else { "" }, value)
}
pub fn write(
    sources: &[Snapshot],
    scope: &ExportScope,
    path: &Path,
    overwrite: bool,
    cancelled: impl Fn() -> bool,
) -> io::Result<()> {
    for s in sources {
        crate::output::protect(path, &s.scan, &cancelled)?;
    }
    let hits = rows(sources, scope, &cancelled)?;
    crate::output::write(path, overwrite, &cancelled, |writer| {
        writer.write_all(
            "\u{feff}이름,전체 경로,종류,크기(Byte),할당 크기(Byte),수정 날짜\r\n".as_bytes(),
        )?;
        for hit in hits {
            crate::output::check(&cancelled)?;
            let scan = &sources[hit.source].scan;
            let path = crate::search::item_path(scan, hit.item);
            let invalid_text = || {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "텍스트로 정확히 표현할 수 없는 경로가 있습니다. CSV 저장을 중단했습니다",
                )
            };
            let name = crate::search::item_name(scan, hit.item);
            let name = name.to_str().ok_or_else(invalid_text)?;
            let path = path.to_str().ok_or_else(invalid_text)?;
            let allocated = match hit.item {
                Item::File(id) => scan.files[id].extra.allocation.value(),
                Item::Folder(id) => {
                    let a = scan.folders[id].allocation;
                    (scan.folders[id].access == crate::scanner::FolderAccess::Complete
                        && (id != 0 || scan.error_count == 0)
                        && a.missing() == 0)
                        .then_some(a.bytes)
                }
            };
            let access = match hit.item {
                Item::Folder(id) => scan.folders[id].access,
                Item::File(_) => crate::scanner::FolderAccess::Complete,
            };
            let size = if access.unreadable() {
                String::new()
            } else {
                crate::search::item_bytes(scan, hit.item).to_string()
            };
            let allocated = allocated.map(|n| n.to_string()).unwrap_or_default();
            let modified = crate::search::item_modified(scan, hit.item)
                .map(|t| crate::local_date::display(Some(t)))
                .unwrap_or_default();
            let values = [
                name,
                path,
                if matches!(hit.item, Item::File(_)) {
                    "파일"
                } else {
                    match access {
                        crate::scanner::FolderAccess::Denied => "폴더 · 접근 불가",
                        crate::scanner::FolderAccess::Unavailable => "폴더 · 조회 실패",
                        crate::scanner::FolderAccess::Partial => "폴더 · 일부 누락",
                        _ => "폴더",
                    }
                },
                &size,
                &allocated,
                &modified,
            ];
            for (i, value) in values.iter().enumerate() {
                if i > 0 {
                    writer.write_all(b",")?;
                }
                text(writer, value)?;
            }
            writer.write_all(b"\r\n")?;
        }
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::explorer::Item;
    use crate::global_search::Hit;
    use std::os::windows::ffi::OsStringExt;
    use std::sync::Arc;
    #[test]
    fn complete_folder_allocation_is_exported_despite_an_unrelated_scan_error() {
        let base =
            std::env::temp_dir().join(format!("diskweave-csv-sibling-{}", std::process::id()));
        let root = base.join("source");
        std::fs::create_dir_all(root.join("good")).unwrap();
        std::fs::create_dir(root.join("denied")).unwrap();
        std::fs::write(root.join("good/payload"), [1u8; 8192]).unwrap();
        let mut scan = crate::scanner::scan_with_options(&root, true, || false, |_| {}).unwrap();
        let good = scan.folder_index(Path::new("good")).unwrap();
        let denied = scan.folder_index(Path::new("denied")).unwrap();
        let allocated = scan.folders[good].allocation.bytes;
        assert!(allocated > 0);
        scan.error_count = 1;
        scan.folders[0].access = crate::scanner::FolderAccess::Partial;
        scan.folders[denied].access = crate::scanner::FolderAccess::Denied;
        let sources = [Snapshot {
            entries: None,
            id: 1,
            generation: 1,
            scan: Arc::new(scan),
        }];
        let path = base.join("good.csv");
        write(
            &sources,
            &ExportScope::Folder {
                source: 0,
                folder: good,
            },
            &path,
            false,
            || false,
        )
        .unwrap();
        let csv = std::fs::read_to_string(&path).unwrap();
        let folder = csv
            .lines()
            .find(|line| line.starts_with("\"good\","))
            .unwrap();
        assert_eq!(
            folder.split(',').nth(4).unwrap(),
            format!("\"{allocated}\"")
        );
        let partial = base.join("partial.csv");
        write(&sources, &ExportScope::Location(0), &partial, false, || {
            false
        })
        .unwrap();
        let csv = std::fs::read_to_string(partial).unwrap();
        let row = csv
            .lines()
            .find(|line| line.starts_with("\"source\","))
            .unwrap();
        assert_eq!(row.split(',').nth(4).unwrap(), "\"\"");
        std::fs::remove_dir_all(base).unwrap();
    }
    #[test]
    fn csv_contains_only_six_required_columns_and_preserves_exact_bytes() {
        let base = std::env::temp_dir().join(format!("diskweave-small-csv-{}", std::process::id()));
        std::fs::create_dir_all(base.join("자료")).unwrap();
        std::fs::write(base.join("자료/한글.txt"), b"private content").unwrap();
        let mut scan = crate::scanner::scan(&base.join("자료"), || false).unwrap();
        scan.files[0].modified = None;
        scan.files[0].extra.allocation = crate::metadata::Field::Value(4096);
        let sources = [Snapshot {
            entries: None,
            id: 1,
            generation: 1,
            scan: Arc::new(scan),
        }];
        let output = base.join("list.csv");
        write(
            &sources,
            &ExportScope::Matches(vec![Hit {
                source: 0,
                item: Item::File(0),
            }]),
            &output,
            false,
            || false,
        )
        .unwrap();
        let csv = std::fs::read_to_string(&output).unwrap();
        let expected=format!("\u{feff}이름,전체 경로,종류,크기(Byte),할당 크기(Byte),수정 날짜\r\n\"한글.txt\",\"{}\",\"파일\",\"15\",\"4096\",\"\"\r\n",base.join("자료").join("한글.txt").display());
        assert_eq!(csv, expected);
        assert_eq!(
            std::fs::read(base.join("자료/한글.txt")).unwrap(),
            b"private content"
        );
        std::fs::remove_dir_all(base).unwrap();
    }
    #[test]
    fn csv_keeps_unknown_allocation_blank_and_escapes_metadata_without_reading_contents() {
        let root =
            std::env::temp_dir().join(format!("diskweave-csv-escaping-{}", std::process::id()));
        std::fs::create_dir_all(root.join("source")).unwrap();
        std::fs::write(root.join("source/original"), b"123").unwrap();
        let mut scan = crate::scanner::scan(&root.join("source"), || false).unwrap();
        scan.files[0].name = "a,\"b\r\n.csv".into();
        scan.files[0].modified = None;
        let sources = [Snapshot {
            entries: None,
            id: 1,
            generation: 1,
            scan: Arc::new(scan),
        }];
        let output = root.join("list.csv");
        write(
            &sources,
            &ExportScope::Matches(vec![Hit {
                source: 0,
                item: Item::File(0),
            }]),
            &output,
            false,
            || false,
        )
        .unwrap();
        let csv = std::fs::read_to_string(&output).unwrap();
        assert!(csv.contains("\"a,\"\"b\r\n.csv\","));
        assert!(csv.ends_with("\"파일\",\"3\",\"\",\"\"\r\n"));
        assert_eq!(std::fs::read(root.join("source/original")).unwrap(), b"123");
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn csv_does_not_present_a_partial_folder_allocation_as_a_complete_size() {
        let root =
            std::env::temp_dir().join(format!("diskweave-csv-partial-{}", std::process::id()));
        std::fs::create_dir_all(root.join("data")).unwrap();
        std::fs::write(root.join("data/file"), b"123").unwrap();
        let mut scan = crate::scanner::scan_unified(
            &root.join("data"),
            Default::default(),
            || false,
            |_| {},
            None,
        )
        .unwrap();
        scan.error_count = 1;
        scan.folders[0].modified = None;
        let source = [Snapshot {
            entries: None,
            id: 1,
            generation: 1,
            scan: Arc::new(scan),
        }];
        let output = root.join("partial.csv");
        write(
            &source,
            &ExportScope::Matches(vec![Hit {
                source: 0,
                item: Item::Folder(0),
            }]),
            &output,
            false,
            || false,
        )
        .unwrap();
        assert!(std::fs::read_to_string(output)
            .unwrap()
            .ends_with("\"폴더\",\"3\",\"\",\"\"\r\n"));
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn export_scopes_preserve_exact_rows_and_csv_text_without_readding_matched_folders() {
        let base = std::env::temp_dir().join(format!("diskweave-export-{}", std::process::id()));
        std::fs::create_dir_all(base.join("a/sub")).unwrap();
        std::fs::create_dir_all(base.join("b")).unwrap();
        std::fs::write(base.join("a/sub/data.bin"), [0; 7]).unwrap();
        std::fs::write(base.join("a/=SUM(1,2).txt"), [1; 3]).unwrap();
        std::fs::write(base.join("b/other.bin"), [2; 4]).unwrap();
        let unusual = std::ffi::OsString::from_wide(&[0xd800, 0x2e, 0x78]);

        let mut sources: Vec<_> = ["a", "b"]
            .iter()
            .enumerate()
            .map(|(i, p)| Snapshot {
                entries: None,
                id: i as u64,
                generation: 1,
                scan: Arc::new(crate::scanner::scan(&base.join(p), || false).unwrap()),
            })
            .collect();
        let csv = base.join("rows.csv");
        write(&sources, &ExportScope::All, &csv, false, || false).unwrap();
        assert_eq!(std::fs::read_to_string(&csv).unwrap().lines().count(), 7);
        let child = sources[0]
            .scan
            .folder_index(std::path::Path::new("sub"))
            .unwrap();
        write(
            &sources,
            &ExportScope::Folder {
                source: 0,
                folder: child,
            },
            &csv,
            true,
            || false,
        )
        .unwrap();
        assert_eq!(std::fs::read_to_string(&csv).unwrap().lines().count(), 3);
        let matches = ExportScope::Matches(vec![
            Hit {
                source: 0,
                item: Item::Folder(child),
            },
            Hit {
                source: 0,
                item: Item::Folder(child),
            },
        ]);
        write(&sources, &matches, &csv, true, || false).unwrap();
        assert_eq!(std::fs::read_to_string(&csv).unwrap().lines().count(), 2);
        write(&sources, &ExportScope::Location(0), &csv, true, || false).unwrap();
        let bytes = std::fs::read(&csv).unwrap();
        assert!(bytes.starts_with(&[0xef, 0xbb, 0xbf]));
        let text = String::from_utf8(bytes).unwrap();
        assert!(text.contains("\"'=SUM(1,2).txt\""));
        assert!(!text.contains("path_utf16"));
        let previous = std::fs::read(&csv).unwrap();
        assert!(write(&sources, &ExportScope::All, &csv, true, || true).is_err());
        assert_eq!(std::fs::read(&csv).unwrap(), previous);
        Arc::get_mut(&mut sources[0].scan).unwrap().files[0].name = unusual;
        assert_eq!(
            write(&sources, &ExportScope::All, &csv, true, || false)
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
        assert_eq!(std::fs::read(&csv).unwrap(), previous);
        assert_eq!(std::fs::read(base.join("a/=SUM(1,2).txt")).unwrap(), [1; 3]);
        std::fs::remove_dir_all(base).unwrap();
    }
}
