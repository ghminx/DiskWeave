use crate::scanner::{FileEntry, Folder, ScanIssue, ScanProgress, ScanResult};
use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::io;
use std::os::windows::ffi::OsStringExt;
#[cfg(test)]
use std::os::windows::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};
use std::time::SystemTime;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum Id {
    V2(u64),
}
pub struct Info {
    pub link_count: Option<u32>,
    pub reference: Option<Id>,
    pub bytes: u64,
    pub attributes: u32,
    pub created: Option<SystemTime>,
    pub modified: Option<SystemTime>,
    pub extra: crate::metadata::Report,
}
pub trait Source {
    fn root_id(&mut self) -> io::Result<Id>;
    fn page(&mut self) -> io::Result<Option<Vec<u8>>>;
    fn inspect(&mut self, path: &Path, detailed: bool) -> io::Result<Info>;
    fn links(&mut self, path: &Path) -> io::Result<Vec<PathBuf>>;
}
#[cfg(test)]
pub fn inspect(path: &Path, detailed: bool) -> io::Result<Info> {
    let m = std::fs::symlink_metadata(path)?;
    Ok(Info {
        link_count: None,
        reference: None,
        bytes: m.len(),
        attributes: m.file_attributes(),
        created: m.created().ok(),
        modified: m.modified().ok(),
        extra: if detailed && m.is_file() {
            crate::metadata::read(path)
        } else {
            crate::metadata::Report {
                values: Default::default(),
                issue: None,
            }
        },
    })
}
fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}
struct Record {
    id: Id,
    parent: Id,
    name: OsString,
    attributes: u32,
}
fn decode(page: &[u8]) -> io::Result<Vec<Record>> {
    if page.len() < 8 {
        return Err(invalid("잘린 NTFS 페이지"));
    }
    let mut at = 8;
    let mut records = Vec::new();
    while at < page.len() {
        let b = &page[at..];
        if b.len() < 60 {
            return Err(invalid("잘린 NTFS 레코드"));
        }
        let len = u32::from_le_bytes(b[..4].try_into().unwrap()) as usize;
        if len < 60 || len > b.len() || !len.is_multiple_of(8) {
            return Err(invalid("잘못된 NTFS 레코드 길이"));
        }
        if u16::from_le_bytes(b[4..6].try_into().unwrap()) != 2 {
            return Err(invalid("지원하지 않는 NTFS 레코드 버전"));
        }
        let name_len = u16::from_le_bytes(b[56..58].try_into().unwrap()) as usize;
        let offset = u16::from_le_bytes(b[58..60].try_into().unwrap()) as usize;
        if name_len == 0 || !name_len.is_multiple_of(2) || offset < 60 || offset + name_len > len {
            return Err(invalid("잘못된 NTFS 이름 범위"));
        }
        let name: Vec<_> = b[offset..offset + name_len]
            .as_chunks::<2>()
            .0
            .iter()
            .map(|n| u16::from_le_bytes(*n))
            .collect();
        if name.contains(&0) || name.iter().any(|n| [47, 92, 58].contains(n)) {
            return Err(invalid("잘못된 NTFS 이름"));
        }
        records.push(Record {
            id: Id::V2(u64::from_le_bytes(b[8..16].try_into().unwrap())),
            parent: Id::V2(u64::from_le_bytes(b[16..24].try_into().unwrap())),
            name: OsString::from_wide(&name),
            attributes: u32::from_le_bytes(b[52..56].try_into().unwrap()),
        });
        at += len;
    }
    Ok(records)
}
/// 공개 NTFS 레코드와 경로별 메타데이터를 동일한 분석 결과로 조립.
///
/// Args:
///     root: 선택한 볼륨 루트.
///     detailed: 할당량·128비트 ID·링크 수의 추가 수집 여부.
///     source: 공개 Windows API 또는 시험용 OS 경계.
///     cancelled: 부분 결과를 게시하지 않는 취소 조건.
///     progress: 열거 중 항목 수 또는 메타데이터 확인 중 파일/폴더 수 전달.
///
/// Returns:
///     부모가 자식보다 앞선 경로별 집계 결과. 구조 불일치 시 오류 반환.
pub fn collect(
    root: &Path,
    detailed: bool,
    source: &mut impl Source,
    cancelled: impl Fn() -> bool,
    mut progress: impl FnMut(ScanProgress),
) -> io::Result<ScanResult> {
    crate::output::check(&cancelled)?;
    let root_id = source.root_id()?;
    let mut records = HashMap::new();
    while let Some(page) = source.page()? {
        crate::output::check(&cancelled)?;
        for r in decode(&page)? {
            if records.insert(r.id, r).is_some() {
                return Err(invalid("중복 NTFS 객체 ID"));
            }
        }
        progress(ScanProgress {
            files_scanned: records.len() as u64,
            folders_scanned: 0,
        });
    }
    if records.is_empty() {
        return Err(invalid("NTFS 열거 결과가 비어 있습니다"));
    }
    if records
        .keys()
        .any(|id| matches!(id,Id::V2(n) if n & 0x0000_ffff_ffff_ffff ==5) && *id != root_id)
    {
        return Err(invalid("NTFS 루트 참조가 핸들과 일치하지 않습니다"));
    }
    let m = source.inspect(root, false)?;
    if m.reference.is_some_and(|id| id != root_id) {
        return Err(invalid("NTFS 루트 객체가 변경됐습니다"));
    }
    let mut result = ScanResult {
        detailed,
        metadata_error_count: 0,
        metadata_issues: Vec::new(),
        folders: vec![Folder {
            allocation: Default::default(),
            created: m.created,
            attributes: m.attributes,
            path: root.to_owned(),
            parent: None,
            logical_bytes: 0,
            file_count: 0,
            folder_count: 0,
            modified: m.modified,
        }],
        files: Vec::new(),
        children: Vec::new(),
        file_children: Vec::new(),
        error_count: 0,
        issues: Vec::new(),
        skipped_reparse_points: 0,
    };
    if m.attributes & 0x410 != 0x10 {
        return Err(invalid("NTFS 루트 종류 변경"));
    }
    let mut excluded = HashSet::new();
    let mut blocked = HashSet::<PathBuf>::new();
    let mut paths = HashMap::from([(root_id, root.to_owned())]);
    let mut pending: Vec<_> = records.values().filter(|r| r.id != root_id).collect();
    let mut folders = HashMap::from([(root.to_owned(), 0)]);
    let mut files = Vec::new();
    while !pending.is_empty() {
        let before = pending.len();
        let mut next = Vec::new();
        for r in pending {
            crate::output::check(&cancelled)?;
            if excluded.contains(&r.parent)
                || matches!(r.id,Id::V2(n) if n & 0x0000_ffff_ffff_ffff < 16)
                || matches!(r.parent,Id::V2(n) if n & 0x0000_ffff_ffff_ffff < 16 && n & 0x0000_ffff_ffff_ffff !=5)
            {
                excluded.insert(r.id);
                continue;
            }
            let Some(parent) = paths.get(&r.parent).cloned() else {
                next.push(r);
                continue;
            };
            if r.name == "." || r.name == ".." {
                return Err(invalid("잘못된 NTFS 경로 구성요소"));
            }
            let path = parent.join(&r.name);
            if r.attributes & 0x400 != 0 {
                result.skipped_reparse_points += 1;
                excluded.insert(r.id);
                blocked.insert(path);
                continue;
            }
            paths.insert(r.id, path.clone());
            if r.attributes & 0x10 != 0 {
                let p = *folders
                    .get(&parent)
                    .ok_or_else(|| invalid("NTFS 부모가 폴더가 아님"))?;
                let m = match source.inspect(&path, false) {
                    Ok(m) => m,
                    Err(e) => {
                        result.record_error(path.clone(), e);
                        let index = result.folders.len();
                        folders.insert(path.clone(), index);
                        result.folders.push(Folder {
                            allocation: Default::default(),
                            created: None,
                            attributes: r.attributes,
                            path: path.clone(),
                            parent: Some(p),
                            logical_bytes: 0,
                            file_count: 0,
                            folder_count: 0,
                            modified: None,
                        });
                        excluded.insert(r.id);
                        blocked.insert(path);
                        continue;
                    }
                };
                if m.reference.is_some_and(|id| id != r.id) || m.attributes & 0x410 != 0x10 {
                    return Err(invalid("NTFS 폴더 종류 변경"));
                }
                let index = result.folders.len();
                folders.insert(path.clone(), index);
                result.folders.push(Folder {
                    allocation: Default::default(),
                    created: m.created,
                    attributes: m.attributes,
                    path,
                    parent: Some(p),
                    logical_bytes: 0,
                    file_count: 0,
                    folder_count: 0,
                    modified: m.modified,
                });
            } else {
                files.push((path, r.id));
            }
        }
        if next.len() == before {
            return Err(invalid(&format!(
                "NTFS 부모 누락 또는 순환 관계 ({}개, 루트 {:?}, 항목 {:?}, 부모 {:?})",
                next.len(),
                root_id,
                next[0].id,
                next[0].parent
            )));
        }
        pending = next;
    }
    let mut seen = HashSet::new();
    for (path, id) in files {
        crate::output::check(&cancelled)?;
        let first = match source.inspect(&path, detailed) {
            Ok(info) => info,
            Err(error) => {
                result.record_error(path, error);
                continue;
            }
        };
        if first.reference.is_some_and(|reference| reference != id) || first.attributes & 0x410 != 0
        {
            return Err(invalid("NTFS 파일 종류 변경"));
        }
        let links = match if first.link_count == Some(1) {
            Ok(vec![path.clone()])
        } else {
            source.links(&path)
        } {
            Ok(v) => v,
            Err(e) => {
                result.record_error(path, e);
                continue;
            }
        };
        if links.is_empty() {
            return Err(invalid("빈 Hard Link 목록"));
        }
        let original = path;
        let mut original_info = Some(first);
        for path in links {
            if !path.starts_with(root)
                || path.components().any(|p| matches!(p, Component::ParentDir))
            {
                return Err(invalid("볼륨 외부 Hard Link 경로"));
            }
            if path.ancestors().any(|p| blocked.contains(p)) || !seen.insert(path.clone()) {
                continue;
            }
            let parent = *folders
                .get(path.parent().ok_or_else(|| invalid("파일 부모 누락"))?)
                .ok_or_else(|| invalid("Hard Link 부모 누락"))?;
            let m = match if path == original {
                Ok(original_info
                    .take()
                    .expect("중복 경로를 제외한 원본 메타데이터"))
            } else {
                source.inspect(&path, detailed)
            } {
                Ok(m) => m,
                Err(e) => {
                    result.record_error(path, e);
                    continue;
                }
            };
            if m.reference.is_some_and(|reference| reference != id) || m.attributes & 0x410 != 0 {
                return Err(invalid("NTFS 파일 종류 변경"));
            }
            if let Some(error) = m.extra.issue {
                result.metadata_error_count += 1;
                if result.metadata_issues.len() < 20 {
                    result.metadata_issues.push(ScanIssue {
                        path: path.clone(),
                        error,
                    });
                }
            }
            let folder = &mut result.folders[parent];
            folder.logical_bytes = folder
                .logical_bytes
                .checked_add(m.bytes)
                .ok_or_else(|| invalid("용량 합계 초과"))?;
            folder.file_count += 1;
            folder.allocation.add(m.extra.values.allocation)?;
            result.files.push(FileEntry {
                extra: m.extra.values,
                created: m.created,
                attributes: m.attributes,
                name: path
                    .file_name()
                    .ok_or_else(|| invalid("파일명 누락"))?
                    .to_owned(),
                parent,
                logical_bytes: m.bytes,
                modified: m.modified,
            });
        }
        progress(ScanProgress {
            folders_scanned: result.folders.len() as u64,
            files_scanned: result.files.len() as u64,
        });
    }
    crate::scanner::finish(&mut result)?;
    crate::output::check(&cancelled)?;
    progress(ScanProgress {
        folders_scanned: result.folders.len() as u64,
        files_scanned: result.files.len() as u64,
    });
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    struct Fixture {
        root: PathBuf,
        pages: Vec<Vec<u8>>,
        changed: bool,
        huge: bool,
        single: bool,
    }
    impl Source for Fixture {
        fn root_id(&mut self) -> io::Result<Id> {
            Ok(Id::V2(5 | (1 << 48)))
        }
        fn page(&mut self) -> io::Result<Option<Vec<u8>>> {
            Ok(self.pages.pop())
        }
        fn inspect(&mut self, path: &Path, detailed: bool) -> io::Result<Info> {
            let mut info = inspect(path, detailed)?;
            if self.single && path.ends_with("first.bin") {
                info.link_count = Some(1);
            }
            if self.huge && path.extension().is_some() {
                info.bytes = u64::MAX;
            }
            if self.changed && path.ends_with("second.bin") {
                info.reference = Some(Id::V2(999));
            }
            Ok(info)
        }
        fn links(&mut self, path: &Path) -> io::Result<Vec<PathBuf>> {
            if self.single {
                return Err(io::Error::other("단일 링크에 불필요한 경로 열거"));
            }
            if path.ends_with("first.bin") {
                Ok(vec![
                    self.root.join("first.bin"),
                    self.root.join("sub/second.bin"),
                ])
            } else {
                Ok(vec![path.to_owned()])
            }
        }
    }
    fn record(id: u64, parent: u64, name: &str, attrs: u32) -> Vec<u8> {
        use std::os::windows::ffi::OsStrExt;
        let name: Vec<_> = std::ffi::OsStr::new(name).encode_wide().collect();
        let len = (60 + name.len() * 2 + 7) & !7;
        let mut b = vec![0; len];
        b[..4].copy_from_slice(&(len as u32).to_le_bytes());
        b[4..6].copy_from_slice(&2u16.to_le_bytes());
        b[8..16].copy_from_slice(&id.to_le_bytes());
        b[16..24].copy_from_slice(&parent.to_le_bytes());
        b[52..56].copy_from_slice(&attrs.to_le_bytes());
        b[56..58].copy_from_slice(&((name.len() * 2) as u16).to_le_bytes());
        b[58..60].copy_from_slice(&60u16.to_le_bytes());
        for (i, n) in name.iter().enumerate() {
            b[60 + i * 2..62 + i * 2].copy_from_slice(&n.to_le_bytes());
        }
        b
    }
    fn fixture() -> Fixture {
        let root = std::env::temp_dir().join(format!(
            "diskweave-ntfs-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(root.join("sub")).unwrap();
        fs::write(root.join("first.bin"), b"1234567").unwrap();
        fs::hard_link(root.join("first.bin"), root.join("sub/second.bin")).unwrap();
        let r = 5 | (1 << 48);
        let mut page = 200u64.to_le_bytes().to_vec();
        // 자식이 부모보다 먼저 열거되는 경우와 동일 객체의 두 경로 검증
        page.extend(record(101, r, "first.bin", 0));
        page.extend(record(100, r, "sub", 0x10));
        page.extend(record(r, r, ".", 0x10));
        Fixture {
            root,
            pages: vec![page],
            changed: false,
            huge: false,
            single: false,
        }
    }
    #[test]
    fn single_link_files_keep_metadata_without_an_extra_name_enumeration() {
        let mut source = fixture();
        source.single = true;
        let root = source.root.clone();
        fs::remove_file(root.join("sub/second.bin")).unwrap();
        let result = collect(&root, true, &mut source, || false, |_| {}).unwrap();
        assert_eq!(
            (
                result.files.len(),
                result.folders[0].logical_bytes,
                result.error_count
            ),
            (1, 7, 0)
        );
        assert_eq!(result.files[0].extra.links.value(), Some(1));
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn reserved_metadata_parent_may_be_absent_without_hiding_user_dollar_names() {
        let mut source = fixture();
        let root = source.root.clone();
        let r = 5 | (1 << 48);
        fs::write(root.join("$user"), b"123").unwrap();
        source.pages = vec![[
            200u64.to_le_bytes().to_vec(),
            record(100, 11 | (1 << 48), "internal", 0x10),
            record(101, 100, "metadata", 0),
            record(102, r, "$user", 0),
            record(r, r, ".", 0x10),
        ]
        .concat()];
        let result = collect(&root, false, &mut source, || false, |_| {}).unwrap();
        assert_eq!(result.files.len(), 1);
        assert_eq!(result.files[0].name, "$user");
        assert_eq!(result.folders[0].logical_bytes, 3);
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn collector_reports_aggregate_overflow_instead_of_wrapping() {
        let mut source = fixture();
        source.huge = true;
        let root = source.root.clone();
        assert_eq!(
            collect(&root, false, &mut source, || false, |_| {})
                .err()
                .unwrap()
                .kind(),
            io::ErrorKind::InvalidData
        );
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn replacement_after_enumeration_is_not_accepted_as_the_original_file() {
        let mut source = fixture();
        source.changed = true;
        let root = source.root.clone();
        assert_eq!(
            collect(&root, true, &mut source, || false, |_| {})
                .err()
                .unwrap()
                .kind(),
            io::ErrorKind::InvalidData
        );
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn collector_rejects_corrupt_records_orphans_and_cancelled_work() {
        let mut source = fixture();
        let root = source.root.clone();
        let r = 5 | (1 << 48);
        let mut version = record(101, r, "first.bin", 0);
        version[4] = 3;
        let mut offset = record(101, r, "first.bin", 0);
        offset[58..60].copy_from_slice(&2u16.to_le_bytes());
        for bad in [
            record(5 | (2 << 48), 5 | (2 << 48), ".", 0x10),
            version,
            offset,
            record(101, r, "bad/child", 0),
            [
                record(101, r, "first.bin", 0),
                record(101, r, "first.bin", 0),
            ]
            .concat(),
        ] {
            source.pages =
                vec![[200u64.to_le_bytes().to_vec(), bad, record(r, r, ".", 0x10)].concat()];
            assert_eq!(
                collect(&root, false, &mut source, || false, |_| {})
                    .err()
                    .unwrap()
                    .kind(),
                io::ErrorKind::InvalidData
            );
        }
        source.pages = vec![vec![0; 9]];
        assert_eq!(
            collect(&root, false, &mut source, || false, |_| {})
                .err()
                .unwrap()
                .kind(),
            io::ErrorKind::InvalidData
        );
        source.pages =
            vec![[200u64.to_le_bytes().to_vec(), record(100, 999, "sub", 0x10)].concat()];
        assert_eq!(
            collect(&root, false, &mut source, || false, |_| {})
                .err()
                .unwrap()
                .kind(),
            io::ErrorKind::InvalidData
        );
        source.pages =
            vec![[200u64.to_le_bytes().to_vec(), record(100, 100, "sub", 0x10)].concat()];
        assert_eq!(
            collect(&root, false, &mut source, || false, |_| {})
                .err()
                .unwrap()
                .kind(),
            io::ErrorKind::InvalidData
        );
        assert_eq!(
            collect(&root, false, &mut source, || true, |_| {})
                .err()
                .unwrap()
                .kind(),
            io::ErrorKind::Interrupted
        );
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn collector_excludes_reparse_subtrees_and_reports_missing_sizes() {
        let mut source = fixture();
        let root = source.root.clone();
        let r = 5 | (1 << 48);
        source.pages = vec![[
            200u64.to_le_bytes().to_vec(),
            record(100, r, "sub", 0x410),
            record(101, 100, "second.bin", 0),
            record(102, r, "gone.bin", 0),
            record(r, r, ".", 0x10),
        ]
        .concat()];
        let result = collect(&root, true, &mut source, || false, |_| {}).unwrap();
        assert_eq!(result.files.len(), 0);
        assert_eq!(result.folders.len(), 1);
        assert_eq!(result.skipped_reparse_points, 1);
        assert_eq!(result.error_count, 1);
        assert_eq!(result.folders[0].logical_bytes, 0);
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn collector_preserves_hard_link_paths_and_builds_parent_first_tree() {
        let mut source = fixture();
        // MFT 대표 이름은 first.bin으로 변경하며 실제 링크 목록은 OS 경계에서 보완
        let r = 5 | (1 << 48);
        source.pages = vec![[
            200u64.to_le_bytes().to_vec(),
            record(101, r, "first.bin", 0),
            record(100, r, "sub", 0x10),
            record(r, r, ".", 0x10),
        ]
        .concat()];
        let root = source.root.clone();
        let result = collect(&root, true, &mut source, || false, |_| {}).unwrap();
        assert_eq!(
            (
                result.folders[0].logical_bytes,
                result.folders[0].file_count,
                result.folders[0].folder_count
            ),
            (14, 2, 1)
        );
        assert_eq!(result.folders[1].parent, Some(0));
        assert_eq!(result.files.len(), 2);
        assert_eq!(result.files[0].extra.links.value(), Some(2));
        fs::remove_dir_all(root).unwrap();
    }
}
