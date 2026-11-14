use std::fs;
use std::io;
use std::os::windows::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};

const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
const MAX_REPORTED_ERRORS: usize = 20;

pub struct Folder {
    pub path: PathBuf,
    pub parent: Option<usize>,
    pub logical_bytes: u64,
    pub file_count: u64,
    pub folder_count: u64,
}

pub struct ScanIssue {
    pub path: PathBuf,
    pub error: io::Error,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ScanProgress {
    pub folders_scanned: u64,
    pub files_scanned: u64,
}

pub struct ScanResult {
    pub folders: Vec<Folder>,
    pub children: Vec<Vec<usize>>,
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

    fn record_error(&mut self, path: PathBuf, error: io::Error) {
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
    mut on_progress: impl FnMut(ScanProgress),
) -> io::Result<ScanResult> {
    let metadata = fs::symlink_metadata(root)?;
    if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 || !metadata.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "분석 대상은 재분석 지점이 아닌 폴더여야 합니다",
        ));
    }

    let mut result = ScanResult {
        folders: vec![Folder {
            path: root.to_path_buf(),
            parent: None,
            logical_bytes: 0,
            file_count: 0,
            folder_count: 0,
        }],
        children: Vec::new(),
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
                    path: entry_path,
                    parent: Some(folder_index),
                    logical_bytes: 0,
                    file_count: 0,
                    folder_count: 0,
                });
                pending.push(child_index);
            } else if metadata.is_file() {
                let folder = &mut result.folders[folder_index];
                folder.logical_bytes += metadata.len();
                folder.file_count += 1;
                progress.files_scanned += 1;
                if progress.files_scanned.is_multiple_of(256) {
                    on_progress(progress);
                }
            }
        }
    }

    for child_index in (1..result.folders.len()).rev() {
        let (parents, child_and_later) = result.folders.split_at_mut(child_index);
        let child = &child_and_later[0];
        let parent = &mut parents[child.parent.expect("루트 외 폴더의 부모")];
        parent.logical_bytes += child.logical_bytes;
        parent.file_count += child.file_count;
        parent.folder_count += child.folder_count + 1;
    }

    result.children = vec![Vec::new(); result.folders.len()];
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

    on_progress(progress);

    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::time::{SystemTime, UNIX_EPOCH};

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
