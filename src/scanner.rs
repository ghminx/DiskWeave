use std::ffi::OsString;
use std::fs;
use std::io;
use std::os::windows::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};
use std::time::SystemTime;

const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
const MAX_REPORTED_ERRORS: usize = 20;

pub struct Folder {
    pub allocation: crate::metadata::Allocation,
    pub created: Option<SystemTime>,
    pub attributes: u32,
    pub path: PathBuf,
    pub parent: Option<usize>,
    pub logical_bytes: u64,
    pub file_count: u64,
    pub folder_count: u64,
    pub modified: Option<SystemTime>,
}

pub struct ScanIssue {
    pub path: PathBuf,
    pub error: io::Error,
}

pub struct FileEntry {
    pub extra: crate::metadata::Extra,
    pub created: Option<SystemTime>,
    pub attributes: u32,
    pub name: OsString,
    pub parent: usize,
    pub logical_bytes: u64,
    /// 수정 시각 조회 실패 시 None 유지.
    pub modified: Option<SystemTime>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ScanProgress {
    pub folders_scanned: u64,
    pub files_scanned: u64,
}

pub struct ScanResult {
    pub detailed: bool,
    pub metadata_error_count: u64,
    pub metadata_issues: Vec<ScanIssue>,
    pub folders: Vec<Folder>,
    pub files: Vec<FileEntry>,
    pub children: Vec<Vec<usize>>,
    pub file_children: Vec<Vec<usize>>,
    pub error_count: u64,
    pub issues: Vec<ScanIssue>,
    pub skipped_reparse_points: u64,
}

impl ScanResult {
    pub fn folder_index(&self, relative_path: &Path) -> io::Result<usize> {
        let mut index = 0;
        for component in relative_path.components() {
            match component {
                Component::CurDir => {}
                Component::Normal(name) => {
                    index = self.children[index]
                        .iter()
                        .copied()
                        .find(|&child| self.folders[child].path.file_name() == Some(name))
                        .ok_or_else(|| {
                            io::Error::new(
                                io::ErrorKind::NotFound,
                                format!("분석 결과에 없는 폴더: {}", relative_path.display()),
                            )
                        })?;
                }
                _ => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "분석 대상 내부의 상대 경로를 입력해야 합니다",
                    ));
                }
            }
        }
        Ok(index)
    }

    pub fn search_folders(&self, query: &str) -> Vec<usize> {
        let query = query.to_lowercase();
        let mut matches: Vec<_> = self
            .folders
            .iter()
            .enumerate()
            .filter(|(_, folder)| {
                folder
                    .path
                    .file_name()
                    .unwrap_or(folder.path.as_os_str())
                    .to_string_lossy()
                    .to_lowercase()
                    .contains(&query)
            })
            .map(|(index, _)| index)
            .collect();
        matches.sort_unstable_by(|&a, &b| {
            self.folders[b]
                .logical_bytes
                .cmp(&self.folders[a].logical_bytes)
                .then_with(|| self.folders[a].path.cmp(&self.folders[b].path))
        });
        matches
    }

    pub(crate) fn record_error(&mut self, path: PathBuf, error: io::Error) {
        self.error_count += 1;
        if self.issues.len() < MAX_REPORTED_ERRORS {
            self.issues.push(ScanIssue { path, error });
        }
    }
}

pub fn scan(root: &Path, cancelled: impl Fn() -> bool) -> io::Result<ScanResult> {
    scan_with_progress(root, cancelled, |_| {})
}

pub fn scan_with_progress(
    root: &Path,
    cancelled: impl Fn() -> bool,
    on_progress: impl FnMut(ScanProgress),
) -> io::Result<ScanResult> {
    scan_with_options(root, false, cancelled, on_progress)
}
pub fn scan_with_options(
    root: &Path,
    detailed: bool,
    cancelled: impl Fn() -> bool,
    on_progress: impl FnMut(ScanProgress),
) -> io::Result<ScanResult> {
    scan_with_probe(
        root,
        detailed,
        cancelled,
        on_progress,
        crate::metadata::read,
    )
}
pub fn scan_with_probe(
    root: &Path,
    detailed: bool,
    cancelled: impl Fn() -> bool,
    mut on_progress: impl FnMut(ScanProgress),
    mut probe: impl FnMut(&Path) -> crate::metadata::Report,
) -> io::Result<ScanResult> {
    let metadata = fs::symlink_metadata(root)?;
    if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 || !metadata.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "분석 대상은 재분석 지점이 아닌 폴더여야 합니다",
        ));
    }

    let mut result = ScanResult {
        detailed,
        metadata_error_count: 0,
        metadata_issues: Vec::new(),
        folders: vec![Folder {
            allocation: Default::default(),
            created: metadata.created().ok(),
            attributes: metadata.file_attributes(),
            path: root.to_path_buf(),
            parent: None,
            logical_bytes: 0,
            file_count: 0,
            folder_count: 0,
            modified: metadata.modified().ok(),
        }],
        children: Vec::new(),
        files: Vec::new(),
        file_children: Vec::new(),
        error_count: 0,
        issues: Vec::new(),
        skipped_reparse_points: 0,
    };
    let mut pending = vec![0];
    let mut progress = ScanProgress::default();

    while let Some(folder_index) = pending.pop() {
        if cancelled() {
            return Err(io::Error::new(io::ErrorKind::Interrupted, "스캔 취소"));
        }

        let path = result.folders[folder_index].path.clone();
        let entries = match fs::read_dir(&path) {
            Ok(entries) => entries,
            Err(error) => {
                if folder_index == 0 {
                    return Err(error);
                }
                result.record_error(path, error);
                continue;
            }
        };
        progress.folders_scanned += 1;
        on_progress(progress);

        for entry in entries {
            if cancelled() {
                return Err(io::Error::new(io::ErrorKind::Interrupted, "스캔 취소"));
            }

            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    result.record_error(path.clone(), error);
                    break;
                }
            };
            let entry_path = entry.path();
            let metadata = match fs::symlink_metadata(&entry_path) {
                Ok(metadata) => metadata,
                Err(error) => {
                    result.record_error(entry_path, error);
                    continue;
                }
            };

            // Junction과 심볼릭 링크 등의 대상 중복·순환 탐색 방지
            if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
                result.skipped_reparse_points += 1;
            } else if metadata.is_dir() {
                let child_index = result.folders.len();
                result.folders.push(Folder {
                    allocation: Default::default(),
                    created: metadata.created().ok(),
                    attributes: metadata.file_attributes(),
                    path: entry_path,
                    parent: Some(folder_index),
                    logical_bytes: 0,
                    file_count: 0,
                    folder_count: 0,
                    modified: metadata.modified().ok(),
                });
                pending.push(child_index);
            } else if metadata.is_file() {
                let extra = if detailed {
                    let report = probe(&entry_path);
                    if let Some(error) = report.issue {
                        result.metadata_error_count += 1;
                        if result.metadata_issues.len() < MAX_REPORTED_ERRORS {
                            result.metadata_issues.push(ScanIssue {
                                path: entry_path.clone(),
                                error,
                            });
                        }
                    }
                    report.values
                } else {
                    Default::default()
                };
                result.files.push(FileEntry {
                    extra,
                    created: metadata.created().ok(),
                    attributes: metadata.file_attributes(),
                    name: entry.file_name(),
                    parent: folder_index,
                    logical_bytes: metadata.len(),
                    modified: metadata.modified().ok(),
                });
                let folder = &mut result.folders[folder_index];
                folder.allocation.add(extra.allocation)?;
                folder.logical_bytes = folder
                    .logical_bytes
                    .checked_add(metadata.len())
                    .ok_or_else(|| {
                        io::Error::new(
                            io::ErrorKind::InvalidData,
                            "논리 크기 합계가 표현 범위를 넘습니다",
                        )
                    })?;
                folder.file_count += 1;
                progress.files_scanned += 1;
                if progress.files_scanned.is_multiple_of(256) {
                    on_progress(progress);
                }
            }
        }
    }

    finish(&mut result)?;

    on_progress(progress);

    Ok(result)
}

/// 기본·NTFS 수집 결과에 동일한 누적 집계와 정렬 기준 적용.
pub(crate) fn finish(result: &mut ScanResult) -> io::Result<()> {
    for child_index in (1..result.folders.len()).rev() {
        let (parents, child_and_later) = result.folders.split_at_mut(child_index);
        let child = &child_and_later[0];
        let parent = &mut parents[child.parent.expect("루트 외 폴더의 부모")];
        parent.allocation.merge(child.allocation)?;
        parent.logical_bytes = parent
            .logical_bytes
            .checked_add(child.logical_bytes)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "논리 크기 합계가 표현 범위를 넘습니다",
                )
            })?;
        parent.file_count += child.file_count;
        parent.folder_count += child.folder_count + 1;
    }

    result.children = vec![Vec::new(); result.folders.len()];
    result.file_children = vec![Vec::new(); result.folders.len()];
    for (index, file) in result.files.iter().enumerate() {
        result.file_children[file.parent].push(index);
    }
    for siblings in &mut result.file_children {
        siblings.sort_unstable_by(|&a, &b| {
            result.files[b]
                .logical_bytes
                .cmp(&result.files[a].logical_bytes)
                .then_with(|| result.files[a].name.cmp(&result.files[b].name))
        });
    }
    for (index, folder) in result.folders.iter().enumerate().skip(1) {
        result.children[folder.parent.expect("루트 외 폴더의 부모")].push(index);
    }
    for siblings in &mut result.children {
        siblings.sort_unstable_by(|&a, &b| {
            result.folders[b]
                .logical_bytes
                .cmp(&result.folders[a].logical_bytes)
                .then_with(|| result.folders[a].path.cmp(&result.folders[b].path))
        });
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::time::{SystemTime, UNIX_EPOCH};
    #[test]
    fn extra_lookup_failures_keep_logical_records_and_distinguish_uncollected_or_unsupported() {
        let root =
            std::env::temp_dir().join(format!("diskweave-extra-errors-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        for name in ["normal", "failed", "unsupported"] {
            fs::write(root.join(name), [0; 3]).unwrap();
        }
        let scan = scan_with_probe(
            &root,
            true,
            || false,
            |_| {},
            |path| match path.file_name().unwrap().to_str().unwrap() {
                "failed" => crate::metadata::Report::unavailable(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "시험용 추가 조회 실패",
                )),
                "unsupported" => {
                    crate::metadata::Report::unavailable(io::Error::from_raw_os_error(50))
                }
                _ => crate::metadata::read(path),
            },
        )
        .unwrap();
        assert_eq!(
            (
                scan.folders[0].logical_bytes,
                scan.folders[0].file_count,
                scan.error_count,
                scan.metadata_error_count
            ),
            (9, 3, 0, 2)
        );
        let allocation = scan.folders[0].allocation;
        assert_eq!(
            (
                allocation.known,
                allocation.failed,
                allocation.unsupported,
                allocation.not_collected
            ),
            (1, 1, 1, 0)
        );
        let fast = scan_with_probe(
            &root,
            false,
            || false,
            |_| {},
            |_| panic!("빠른 스캔에서 추가 조회 금지"),
        )
        .unwrap();
        assert_eq!(
            (
                fast.folders[0].logical_bytes,
                fast.folders[0].allocation.not_collected
            ),
            (9, 3)
        );
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn detailed_scan_retains_basic_totals_and_collects_creation_attributes_and_allocation() {
        let root =
            std::env::temp_dir().join(format!("diskweave-detailed-scan-{}", std::process::id()));
        fs::create_dir_all(root.join("child")).unwrap();
        fs::write(root.join("child/data.bin"), [1, 2, 3]).unwrap();
        fs::hard_link(root.join("child/data.bin"), root.join("alias.bin")).unwrap();
        fs::write(root.join("empty"), []).unwrap();
        let basic = scan(&root, || false).unwrap();
        assert_eq!(basic.folders[0].allocation.not_collected, 3);
        let detailed = scan_with_options(&root, true, || false, |_| {}).unwrap();
        assert_eq!(
            (
                detailed.folders[0].logical_bytes,
                detailed.folders[0].file_count,
                detailed.folders[0].folder_count
            ),
            (6, 3, 1)
        );
        assert!(detailed.detailed);
        assert_eq!(detailed.metadata_error_count, 0);
        let file = detailed
            .files
            .iter()
            .find(|f| f.name == "data.bin")
            .unwrap();
        assert_eq!(
            file.created,
            fs::metadata(root.join("child/data.bin"))
                .unwrap()
                .created()
                .ok()
        );
        assert_eq!(
            file.attributes,
            fs::metadata(root.join("child/data.bin"))
                .unwrap()
                .file_attributes()
        );
        assert_eq!(file.extra.links.value(), Some(2));
        assert_eq!(detailed.folders[0].allocation.known, 3);
        assert_eq!(detailed.folders[0].allocation.missing(), 0);
        assert_eq!(
            detailed.folders[0].allocation.bytes,
            detailed
                .files
                .iter()
                .map(|f| f.extra.allocation.value().unwrap())
                .sum::<u64>()
        );
        assert_eq!(
            detailed.folders[0].created,
            fs::metadata(&root).unwrap().created().ok()
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn preserves_non_unicode_names_and_counts_hard_links_by_path() {
        use std::os::windows::ffi::{OsStrExt, OsStringExt};
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root =
            std::env::temp_dir().join(format!("diskweave-paths-{}-{stamp}", std::process::id()));
        fs::create_dir(&root).unwrap();
        let units = [0xd800, 0x002e, 0x0062, 0x0069, 0x006e];
        let name = OsString::from_wide(&units);
        fs::write(root.join(&name), [1, 2, 3]).unwrap();
        fs::hard_link(root.join(&name), root.join("link.bin")).unwrap();
        let result = scan(&root, || false).unwrap();
        let original = result.files.iter().find(|file| file.name == name).unwrap();
        assert_eq!(original.name.encode_wide().collect::<Vec<_>>(), units);
        let detailed = scan_with_options(&root, true, || false, |_| {}).unwrap();
        let original_detail = detailed.files.iter().find(|f| f.name == name).unwrap();
        let alias = detailed
            .files
            .iter()
            .find(|f| f.name == "link.bin")
            .unwrap();
        assert!(original_detail.extra.identity.value().is_some());
        assert_eq!(
            original_detail.extra.identity.value(),
            alias.extra.identity.value()
        );
        assert_eq!(original_detail.extra.links.value(), Some(2));
        assert_eq!(
            (
                result.files.len(),
                result.folders[0].file_count,
                result.folders[0].logical_bytes
            ),
            (2, 2, 6)
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn direct_files_are_sorted_by_size_then_original_name() {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "diskweave-file-sort-{}-{stamp}",
            std::process::id()
        ));
        fs::create_dir(&root).unwrap();
        fs::write(root.join("a.bin"), [0]).unwrap();
        fs::write(root.join("z.bin"), [0; 3]).unwrap();
        fs::write(root.join("y.bin"), [0; 3]).unwrap();
        let result = scan(&root, || false).unwrap();
        let names: Vec<_> = result.file_children[0]
            .iter()
            .map(|&id| result.files[id].name.to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["y.bin", "z.bin", "a.bin"]);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn retains_file_metadata_without_changing_folder_totals() {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root =
            std::env::temp_dir().join(format!("diskweave-files-{}-{stamp}", std::process::id()));
        fs::create_dir_all(root.join("nested")).unwrap();
        fs::write(root.join("zero.bin"), []).unwrap();
        fs::write(root.join("nested").join("한글.bin"), [1, 2, 3]).unwrap();

        let result = scan(&root, || false).unwrap();
        let nested = result.folder_index(Path::new("nested")).unwrap();
        assert_eq!(
            result.folders[nested].modified.unwrap(),
            fs::metadata(root.join("nested"))
                .unwrap()
                .modified()
                .unwrap()
        );
        let file = result
            .files
            .iter()
            .find(|file| file.name == "한글.bin")
            .unwrap();
        assert_eq!((file.parent, file.logical_bytes), (nested, 3));
        assert_eq!(
            file.modified.unwrap(),
            fs::metadata(root.join("nested").join("한글.bin"))
                .unwrap()
                .modified()
                .unwrap()
        );
        let zero = &result.files[result.file_children[0][0]];
        assert_eq!(
            (&*zero.name, zero.logical_bytes),
            (std::ffi::OsStr::new("zero.bin"), 0)
        );
        assert_eq!(
            (
                result.files.len(),
                result.folders[0].logical_bytes,
                result.folders[0].file_count
            ),
            (2, 3, 2)
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn aggregates_nested_folders_and_files() {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root =
            std::env::temp_dir().join(format!("diskweave-test-{}-{stamp}", std::process::id()));
        fs::create_dir(&root).unwrap();
        fs::create_dir_all(root.join("a").join("b")).unwrap();
        fs::create_dir(root.join("empty")).unwrap();
        fs::File::create(root.join("root.bin"))
            .unwrap()
            .write_all(&[1, 2, 3])
            .unwrap();
        fs::File::create(root.join("a").join("child.bin"))
            .unwrap()
            .write_all(&[1, 2, 3, 4, 5])
            .unwrap();

        let result = scan(&root, || false).unwrap();
        let root_folder = &result.folders[0];
        assert_eq!(
            (
                root_folder.logical_bytes,
                root_folder.file_count,
                root_folder.folder_count
            ),
            (8, 2, 3)
        );
        assert_eq!(result.error_count, 0);
        assert_eq!(result.children[0].len(), 2);
        assert_eq!(result.folders[result.children[0][0]].path, root.join("a"));
        assert_eq!(
            result.folders[result.children[0][1]].path,
            root.join("empty")
        );
        let a_index = result.children[0][0];
        assert_eq!(
            result.folders[result.children[a_index][0]].path,
            root.join("a").join("b")
        );
        assert_eq!(result.folder_index(Path::new(".")).unwrap(), 0);
        assert_eq!(
            result.folder_index(Path::new("a\\b")).unwrap(),
            result.children[a_index][0]
        );
        assert_eq!(
            result
                .folder_index(Path::new("a\\missing"))
                .unwrap_err()
                .kind(),
            io::ErrorKind::NotFound
        );
        assert_eq!(
            result.folder_index(Path::new("..\\a")).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        assert_eq!(
            result.folder_index(&root).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        assert_eq!(result.search_folders("EMPTY"), vec![result.children[0][1]]);
        assert_eq!(
            result
                .folders
                .iter()
                .find(|folder| folder.path == root.join("a"))
                .unwrap()
                .logical_bytes,
            5
        );
        assert_eq!(
            result
                .folders
                .iter()
                .find(|folder| folder.path == root.join("empty"))
                .unwrap()
                .logical_bytes,
            0
        );

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn cancellation_discards_partial_result() {
        let error = scan(&std::env::temp_dir(), || true).err().unwrap();
        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
    }

    #[test]
    fn search_results_follow_size_order() {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root =
            std::env::temp_dir().join(format!("diskweave-search-{}-{stamp}", std::process::id()));
        fs::create_dir(&root).unwrap();
        fs::create_dir(root.join("model_small")).unwrap();
        fs::create_dir(root.join("model_big")).unwrap();
        fs::write(root.join("model_small").join("file.bin"), [1]).unwrap();
        fs::write(root.join("model_big").join("file.bin"), [1, 2, 3]).unwrap();

        let result = scan(&root, || false).unwrap();
        let matches = result.search_folders("MODEL");
        assert_eq!(matches.len(), 2);
        assert_eq!(result.folders[matches[0]].path, root.join("model_big"));
        assert_eq!(result.folders[matches[1]].path, root.join("model_small"));
        assert!(result.search_folders("absent").is_empty());

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn reports_scanned_files_and_folders() {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root =
            std::env::temp_dir().join(format!("diskweave-progress-{}-{stamp}", std::process::id()));
        fs::create_dir_all(root.join("child")).unwrap();
        fs::write(root.join("first.bin"), [1]).unwrap();
        fs::write(root.join("child").join("second.bin"), [2]).unwrap();

        let mut updates = Vec::new();
        let result =
            scan_with_progress(&root, || false, |progress| updates.push(progress)).unwrap();
        assert_eq!(result.folders[0].file_count, 2);
        assert_eq!(
            updates.last().copied(),
            Some(ScanProgress {
                folders_scanned: 2,
                files_scanned: 2,
            })
        );

        fs::remove_dir_all(root).unwrap();
    }
}
