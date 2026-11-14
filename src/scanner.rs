use std::fs;
use std::io;
use std::os::windows::fs::MetadataExt;
use std::path::{Path, PathBuf};

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

pub struct ScanResult {
    pub folders: Vec<Folder>,
    pub error_count: u64,
    pub issues: Vec<ScanIssue>,
    pub skipped_reparse_points: u64,
}

impl ScanResult {
    fn record_error(&mut self, path: PathBuf, error: io::Error) {
        self.error_count += 1;
        if self.issues.len() < MAX_REPORTED_ERRORS {
            self.issues.push(ScanIssue { path, error });
        }
    }
}

pub fn scan(root: &Path, cancelled: impl Fn() -> bool) -> io::Result<ScanResult> {
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
        error_count: 0,
        issues: Vec::new(),
        skipped_reparse_points: 0,
    };
    let mut pending = vec![0];

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
}
