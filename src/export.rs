use crate::explorer::Item;
use crate::global_search::Hit;
use crate::locations::Snapshot;
use crate::metadata::{Allocation, Extra};
use serde::Serialize;
use std::io::{self, Write};
use std::path::Path;
use std::time::SystemTime;

#[derive(Clone)]
pub struct ExportSource {
    pub source: Snapshot,
    pub started: Option<SystemTime>,
    pub finished: Option<SystemTime>,
}
#[derive(Clone)]
pub enum ExportScope {
    Folder { source: usize, folder: usize },
    Location(usize),
    Matches(Vec<Hit>),
    All,
}
impl ExportScope {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Folder { .. } => "현재 폴더와 하위 항목",
            Self::Location(_) => "현재 분석 대상 전체",
            Self::Matches(_) => "현재 검색 결과",
            Self::All => "모든 분석 위치",
        }
    }
}
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Csv,
    Json,
}
#[derive(Serialize)]
struct SourceInfo {
    root: String,
    root_utf16: Vec<u16>,
    started: Option<crate::snapshot::Time>,
    finished: Option<crate::snapshot::Time>,
    traversal_errors: u64,
    metadata_errors: u64,
    skipped_reparse_points: u64,
}
#[derive(Serialize)]
struct Row {
    source: usize,
    name: String,
    path: String,
    path_utf16: Vec<u16>,
    kind: &'static str,
    logical_bytes: u64,
    allocation: Allocation,
    extra: Option<Extra>,
    created: Option<crate::snapshot::Time>,
    modified: Option<crate::snapshot::Time>,
    attributes: u32,
    files: Option<u64>,
    folders: Option<u64>,
}
fn rows(
    sources: &[ExportSource],
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
                Item::Folder(id) => id < source.source.scan.folders.len(),
                Item::File(id) => id < source.source.scan.files.len(),
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
        if folder >= s.source.scan.folders.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "내보낼 폴더가 없습니다",
            ));
        }
        let scan = &s.source.scan;
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
fn row(sources: &[ExportSource], hit: Hit) -> io::Result<Row> {
    let scan = &sources[hit.source].source.scan;
    let path = crate::search::item_path(scan, hit.item);
    let (created, modified, attributes, allocation, extra, files, folders) = match hit.item {
        Item::File(id) => {
            let f = &scan.files[id];
            let mut a = Allocation::default();
            a.add(f.extra.allocation)?;
            (
                f.created,
                f.modified,
                f.attributes,
                a,
                Some(f.extra),
                None,
                None,
            )
        }
        Item::Folder(id) => {
            let f = &scan.folders[id];
            (
                f.created,
                f.modified,
                f.attributes,
                f.allocation,
                None,
                Some(f.file_count),
                Some(f.folder_count),
            )
        }
    };
    Ok(Row {
        source: hit.source,
        name: crate::search::item_name(scan, hit.item)
            .to_string_lossy()
            .into_owned(),
        path: path.display().to_string(),
        path_utf16: crate::snapshot::wide(path.as_os_str()),
        kind: if matches!(hit.item, Item::File(_)) {
            "file"
        } else {
            "folder"
        },
        logical_bytes: crate::search::item_bytes(scan, hit.item),
        allocation,
        extra,
        created: created.map(Into::into),
        modified: modified.map(Into::into),
        attributes,
        files,
        folders,
    })
}
fn text(writer: &mut dyn Write, value: &str) -> io::Result<()> {
    let guarded = matches!(
        value.trim_start().chars().next(),
        Some('=' | '+' | '-' | '@')
    ) || value.starts_with(['\t', '\r', '\n']);
    write!(
        writer,
        "\"{}{}\"",
        if guarded { "'" } else { "" },
        value.replace('"', "\"\"")
    )
}
pub fn write(
    sources: &[ExportSource],
    scope: &ExportScope,
    format: Format,
    path: &Path,
    overwrite: bool,
    cancelled: impl Fn() -> bool,
) -> io::Result<()> {
    for s in sources {
        crate::output::protect(path, &s.source.scan, &cancelled)?;
    }
    let hits = rows(sources, scope, &cancelled)?;
    crate::output::write(path, overwrite, &cancelled, |writer| {
        if format == Format::Json {
            writer.write_all(b"{\"format\":\"DiskWeave export\",\"version\":1,\"rows_are_not_additive\":true,\"scope\":")?;
            serde_json::to_writer(&mut *writer, scope.label())
                .map_err(crate::output::json_error)?;
            writer.write_all(b",\"exported\":")?;
            serde_json::to_writer(
                &mut *writer,
                &crate::snapshot::Time::from(SystemTime::now()),
            )
            .map_err(crate::output::json_error)?;
            writer.write_all(b",\"sources\":[")?;
            for (i, s) in sources.iter().enumerate() {
                crate::output::check(&cancelled)?;
                if i > 0 {
                    writer.write_all(b",")?;
                }
                let scan = &s.source.scan;
                let root = &scan.folders[0].path;
                serde_json::to_writer(
                    &mut *writer,
                    &SourceInfo {
                        root: root.display().to_string(),
                        root_utf16: crate::snapshot::wide(root.as_os_str()),
                        started: s.started.map(Into::into),
                        finished: s.finished.map(Into::into),
                        traversal_errors: scan.error_count,
                        metadata_errors: scan.metadata_error_count,
                        skipped_reparse_points: scan.skipped_reparse_points,
                    },
                )
                .map_err(crate::output::json_error)?;
            }
            writer.write_all(b"],\"rows\":[")?;
            for (i, &hit) in hits.iter().enumerate() {
                crate::output::check(&cancelled)?;
                if i > 0 {
                    writer.write_all(b",")?;
                }
                serde_json::to_writer(&mut *writer, &row(sources, hit)?)
                    .map_err(crate::output::json_error)?;
            }
            writer.write_all(b"]}")?;
        } else {
            writer.write_all(b"\xef\xbb\xbfname,path,source_root,kind,logical_bytes,allocated_known_bytes,allocation_missing,created,modified,attributes,files,folders,path_utf16,scan_started,scan_finished,traversal_errors,metadata_errors,scope,file_id,hard_links\r\n")?;
            for hit in hits {
                crate::output::check(&cancelled)?;
                let r = row(sources, hit)?;
                let source = &sources[hit.source];
                let scan = &source.source.scan;
                let values = [
                    r.name,
                    r.path,
                    scan.folders[0].path.display().to_string(),
                    r.kind.into(),
                    r.logical_bytes.to_string(),
                    r.allocation.bytes.to_string(),
                    format!(
                        "not_collected:{};unsupported:{};failed:{}",
                        r.allocation.not_collected, r.allocation.unsupported, r.allocation.failed
                    ),
                    crate::local_date::display(match hit.item {
                        Item::File(id) => scan.files[id].created,
                        Item::Folder(id) => scan.folders[id].created,
                    }),
                    crate::local_date::display(crate::search::item_modified(scan, hit.item)),
                    r.attributes.to_string(),
                    r.files.map(|v| v.to_string()).unwrap_or_default(),
                    r.folders.map(|v| v.to_string()).unwrap_or_default(),
                    format!(
                        "u16:{}",
                        r.path_utf16
                            .iter()
                            .map(|v| format!("{v:04X}"))
                            .collect::<Vec<_>>()
                            .join(" ")
                    ),
                    crate::local_date::display(source.started),
                    crate::local_date::display(source.finished),
                    scan.error_count.to_string(),
                    scan.metadata_error_count.to_string(),
                    scope.label().into(),
                    r.extra
                        .map(|e| crate::presentation::field_display(e.identity))
                        .unwrap_or_else(|| "—".into()),
                    r.extra
                        .map(|e| crate::presentation::field_display(e.links))
                        .unwrap_or_else(|| "—".into()),
                ];
                for (i, value) in values.iter().enumerate() {
                    if i > 0 {
                        writer.write_all(b",")?;
                    }
                    text(writer, value)?;
                }
                writer.write_all(b"\r\n")?;
            }
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
    fn export_scopes_preserve_exact_rows_and_csv_text_without_readding_matched_folders() {
        let base = std::env::temp_dir().join(format!("diskweave-export-{}", std::process::id()));
        std::fs::create_dir_all(base.join("a/sub")).unwrap();
        std::fs::create_dir_all(base.join("b")).unwrap();
        std::fs::write(base.join("a/sub/data.bin"), [0; 7]).unwrap();
        std::fs::write(base.join("a/=SUM(1,2).txt"), [1; 3]).unwrap();
        std::fs::write(base.join("b/other.bin"), [2; 4]).unwrap();
        let unusual = std::ffi::OsString::from_wide(&[0xd800, 0x2e, 0x78]);
        std::fs::write(base.join("a").join(&unusual), []).unwrap();
        let sources: Vec<_> = ["a", "b"]
            .iter()
            .enumerate()
            .map(|(i, p)| ExportSource {
                source: crate::locations::Snapshot {
                    id: i as u64,
                    generation: 1,
                    scan: Arc::new(crate::scanner::scan(&base.join(p), || false).unwrap()),
                },
                started: None,
                finished: None,
            })
            .collect();
        let json = base.join("all.json");
        write(
            &sources,
            &ExportScope::All,
            Format::Json,
            &json,
            false,
            || false,
        )
        .unwrap();
        let data: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&json).unwrap()).unwrap();
        assert_eq!(data["rows"].as_array().unwrap().len(), 7);
        assert!(data["rows"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["path_utf16"]
                .as_array()
                .unwrap()
                .iter()
                .any(|v| v == 0xd800)));
        assert_eq!(data["sources"].as_array().unwrap().len(), 2);
        let child = sources[0]
            .source
            .scan
            .folder_index(std::path::Path::new("sub"))
            .unwrap();
        write(
            &sources,
            &ExportScope::Folder {
                source: 0,
                folder: child,
            },
            Format::Json,
            &json,
            true,
            || false,
        )
        .unwrap();
        let data: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&json).unwrap()).unwrap();
        assert_eq!(data["rows"].as_array().unwrap().len(), 2);
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
        write(&sources, &matches, Format::Json, &json, true, || false).unwrap();
        let data: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&json).unwrap()).unwrap();
        assert_eq!(data["rows"].as_array().unwrap().len(), 1);
        assert_eq!(data["rows"][0]["logical_bytes"], 7);
        let csv = base.join("rows.csv");
        write(
            &sources,
            &ExportScope::Location(0),
            Format::Csv,
            &csv,
            false,
            || false,
        )
        .unwrap();
        let bytes = std::fs::read(&csv).unwrap();
        assert!(bytes.starts_with(&[0xef, 0xbb, 0xbf]));
        let text = String::from_utf8(bytes).unwrap();
        assert!(text.contains("\"'=SUM(1,2).txt\""));
        assert!(text.contains("path_utf16"));
        assert_eq!(std::fs::read(base.join("a/=SUM(1,2).txt")).unwrap(), [1; 3]);
        std::fs::remove_dir_all(base).unwrap();
    }
}
