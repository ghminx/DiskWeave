#[path = "scanner/directory.rs"]
pub mod directory;
#[path = "scanner/parallel.rs"]
mod parallel;
#[path = "scanner/profile.rs"]
pub mod profile;
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

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Parallelism {
    #[default]
    Auto,
    Sequential,
    Two,
    Four,
    Eight,
    #[allow(dead_code, reason = "제품 기본 상한 8개를 유지하는 개발용 비교")]
    Twelve,
    #[allow(dead_code, reason = "제품 기본 상한 8개를 유지하는 개발용 비교")]
    Sixteen,
}
#[derive(Default)]
pub struct ScanMetrics {
    pub trace: profile::Trace,
    pub enumeration_ns: std::sync::atomic::AtomicU64,
    pub finish_ns: std::sync::atomic::AtomicU64,
    pub read_ns: [std::sync::atomic::AtomicU64; 6],
    pub read_calls: [std::sync::atomic::AtomicU64; 6],
}
/// 열거 결과의 출처를 유지하는 제한된 상세 조회 요청.
pub struct MetadataRequest {
    pub index: usize,
    pub path: PathBuf,
    #[allow(dead_code, reason = "개발용 파일 ID 조회 후보에 열거 출처 전달")]
    pub identity: Option<crate::metadata::Identity>,
    #[allow(dead_code, reason = "개발용 MFT 조합에서 현재 열거와 크기/할당량 대조")]
    pub logical_bytes: u64,
    #[allow(dead_code, reason = "개발용 MFT 조합에서 현재 열거와 크기/할당량 대조")]
    pub allocation: Option<u64>,
}
pub trait MetadataProbe: Sync {
    fn read_batch(
        &self,
        requests: &[MetadataRequest],
        stop: &std::sync::atomic::AtomicBool,
    ) -> Vec<crate::metadata::Report>;
}
impl<F: Fn(&Path) -> crate::metadata::Report + Sync> MetadataProbe for F {
    fn read_batch(
        &self,
        requests: &[MetadataRequest],
        stop: &std::sync::atomic::AtomicBool,
    ) -> Vec<crate::metadata::Report> {
        requests
            .iter()
            .take_while(|_| !stop.load(std::sync::atomic::Ordering::Relaxed))
            .map(|r| self(&r.path))
            .collect()
    }
}
/// 동일한 수집·집계·취소 계약에서 OS 상세 조회 후보만 대조하는 개발용 경계.
#[allow(dead_code, reason = "개발 도구에서만 후보 평가")]
pub fn scan_with_metadata_source(
    root: &Path,
    detailed: bool,
    parallelism: Parallelism,
    strategy: directory::Strategy,
    cancelled: impl Fn() -> bool,
    progress: impl FnMut(ScanProgress),
    probe: &impl MetadataProbe,
) -> io::Result<ScanResult> {
    parallel::scan_collected(
        root,
        detailed,
        parallelism.workers(),
        parallel::Collection {
            strategy,
            metrics: None,
        },
        cancelled,
        progress,
        probe,
    )
}
/// MFT 보완 조회와 기존 폴더 미리보기의 동일 수집 경계.
#[allow(
    clippy::too_many_arguments,
    reason = "기존 수집 계약과 선택적 미리보기 전달"
)]
pub fn scan_metadata_observed<'a>(
    root: &Path,
    detailed: bool,
    parallelism: Parallelism,
    cancelled: impl Fn() -> bool,
    progress: impl FnMut(ScanProgress),
    probe: &impl MetadataProbe,
    observer: Option<&'a mut preview::Observer<'a>>,
) -> io::Result<ScanResult> {
    parallel::scan_observed(
        root,
        detailed,
        parallelism.workers(),
        parallel::Collection {
            strategy: directory::Strategy::Bulk,
            metrics: None,
        },
        cancelled,
        progress,
        probe,
        observer,
    )
}
/// 비교용 수집 경로를 고정한 스캔. 제품 설정과 원본 파일 변경 없이 실험 경로 대조.
pub fn scan_with_strategy(
    root: &Path,
    detailed: bool,
    parallelism: Parallelism,
    strategy: directory::Strategy,
    cancelled: impl Fn() -> bool,
    progress: impl FnMut(ScanProgress),
    metrics: Option<&ScanMetrics>,
) -> io::Result<ScanResult> {
    let trace = metrics.map(|m| &m.trace);
    let _overall = profile::scope(trace, profile::Stage::Overall);
    let _cpu = profile::cpu_scope(trace, false, profile::thread_cpu);
    if strategy == directory::Strategy::Reference && metrics.is_none() {
        return scan_parallel_with_probe(
            root,
            detailed,
            parallelism,
            cancelled,
            progress,
            crate::metadata::read,
        );
    }
    let setup = profile::scope(trace, profile::Stage::RootSetup);
    let named = if detailed
        && matches!(
            strategy,
            directory::Strategy::Bulk
                | directory::Strategy::Combined
                | directory::Strategy::Experimental(_)
        ) {
        Some(crate::metadata::Named::for_root(root)?)
    } else {
        None
    };
    drop(setup);
    let read = |path: &Path| {
        let mut values = crate::metadata::ReadMetrics {
            trace,
            ..Default::default()
        };
        let measure = metrics.map(|_| &mut values);
        let report = if let Some(named) = &named {
            crate::metadata::read_combined(path, named, measure)
        } else if metrics.is_none() {
            crate::metadata::read(path)
        } else {
            crate::metadata::read_with_metrics(path, measure)
        };
        if let Some(metrics) = metrics {
            use std::sync::atomic::Ordering;
            for (a, b) in metrics.read_ns.iter().zip(values.nanoseconds) {
                a.fetch_add(b, Ordering::Relaxed);
            }
            for (a, b) in metrics.read_calls.iter().zip(values.calls) {
                a.fetch_add(b, Ordering::Relaxed);
            }
        }
        report
    };
    parallel::scan_collected(
        root,
        detailed,
        parallelism.workers(),
        parallel::Collection { strategy, metrics },
        cancelled,
        progress,
        &read,
    )
}
impl Parallelism {
    pub fn workers(self) -> usize {
        match self {
            Self::Sequential => 1,
            Self::Two => 2,
            Self::Four => 4,
            Self::Eight => 8,
            Self::Twelve => 12,
            Self::Sixteen => 16,
            Self::Auto => std::thread::available_parallelism().map_or(1, |n| n.get().min(8)),
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Self::Auto => "자동",
            Self::Sequential => "순차",
            Self::Two => "2개",
            Self::Four => "4개",
            Self::Eight => "8개",
            Self::Twelve => "12개",
            Self::Sixteen => "16개",
        }
    }
}
/// 제한된 작업 풀에서 폴더 열거와 상세 조회를 처리하고 완성된 결과만 반환.
///
/// Args:
///     parallelism: 제품 최대 8개, 개발용 비교 최대 16개의 동시 작업 설정.
///     cancelled: 조정 스레드에서 확인하는 취소 조건.
///     on_progress: 묶음 단위로 전달하는 완료 파일/폴더 수.
///     probe: 여러 작업 스레드에서 동시에 호출 가능한 OS 메타데이터 조회 경계.
///
/// Returns:
///     모든 작업 스레드 정리 후 완료 결과 또는 취소/수집 오류.
pub fn scan_parallel_with_probe(
    root: &Path,
    detailed: bool,
    parallelism: Parallelism,
    cancelled: impl Fn() -> bool,
    on_progress: impl FnMut(ScanProgress),
    probe: impl Fn(&Path) -> crate::metadata::Report + Sync,
) -> io::Result<ScanResult> {
    if parallelism.workers() == 1 {
        return scan_with_probe(root, detailed, cancelled, on_progress, probe);
    }
    parallel::scan(
        root,
        detailed,
        parallelism.workers(),
        cancelled,
        on_progress,
        &probe,
    )
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
    scan_configured(root, detailed, Parallelism::Auto, cancelled, on_progress)
}
/// 제품 공통 기본 수집. 병렬도와 완료/취소 계약을 유지한 검증된 수집 경로 선택.
pub fn scan_configured(
    root: &Path,
    detailed: bool,
    parallelism: Parallelism,
    cancelled: impl Fn() -> bool,
    progress: impl FnMut(ScanProgress),
) -> io::Result<ScanResult> {
    scan_with_strategy(
        root,
        detailed,
        parallelism,
        Default::default(),
        cancelled,
        progress,
        None,
    )
}
pub fn scan_with_probe(
    root: &Path,
    detailed: bool,
    cancelled: impl Fn() -> bool,
    mut on_progress: impl FnMut(ScanProgress),
    mut probe: impl FnMut(&Path) -> crate::metadata::Report,
) -> io::Result<ScanResult> {
    let mut result = empty_result(root, detailed)?;
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
            // Windows 열거 결과의 메타데이터 재사용. 링크를 따라가지 않으며 항목별 추가 시스템 호출 방지.
            let metadata = match entry.metadata() {
                Ok(metadata) => metadata,
                Err(error) => {
                    result.record_error(entry_path, error);
                    continue;
                }
            };
            // NTFS 디렉터리 엔트리의 시각은 하위 변경보다 늦게 갱신될 수 있어 폴더만 현재 값 재조회.
            let metadata = if metadata.is_dir()
                && metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT == 0
            {
                match fs::symlink_metadata(&entry_path) {
                    Ok(metadata) => metadata,
                    Err(error) => {
                        result.record_error(entry_path, error);
                        continue;
                    }
                }
            } else {
                metadata
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

fn empty_result(root: &Path, detailed: bool) -> io::Result<ScanResult> {
    let metadata = fs::symlink_metadata(root)?;
    if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 || !metadata.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "분석 대상은 재분석 지점이 아닌 폴더여야 합니다",
        ));
    }

    let result = ScanResult {
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
    #[test]
    fn incomplete_batch_is_failure_and_all_workers_leave_before_return() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        struct Broken(AtomicUsize);
        impl MetadataProbe for Broken {
            fn read_batch(
                &self,
                _: &[MetadataRequest],
                _: &std::sync::atomic::AtomicBool,
            ) -> Vec<crate::metadata::Report> {
                self.0.fetch_add(1, Ordering::SeqCst);
                let reports = vec![];
                self.0.fetch_sub(1, Ordering::SeqCst);
                reports
            }
        }
        let root =
            std::env::temp_dir().join(format!("diskweave-invalid-batch-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("a"), b"123").unwrap();
        let probe = Broken(AtomicUsize::new(0));
        let result = scan_with_metadata_source(
            &root,
            true,
            Parallelism::Eight,
            directory::Strategy::Bulk,
            || false,
            |_| {},
            &probe,
        );
        assert_eq!(result.err().unwrap().kind(), io::ErrorKind::Other);
        assert_eq!(probe.0.load(Ordering::SeqCst), 0);
        assert_eq!(scan(&root, || false).unwrap().folders[0].logical_bytes, 3);
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn metadata_batches_receive_enumerated_identity_without_changing_scan_totals() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        struct Probe {
            count: AtomicUsize,
            maximum: AtomicUsize,
        }
        impl MetadataProbe for Probe {
            fn read_batch(
                &self,
                requests: &[MetadataRequest],
                stop: &std::sync::atomic::AtomicBool,
            ) -> Vec<crate::metadata::Report> {
                assert!(requests.iter().all(|r| r.identity.is_some()));
                assert!(requests
                    .iter()
                    .all(|r| r.logical_bytes == 3 && r.allocation.is_some()));
                self.maximum.fetch_max(requests.len(), Ordering::SeqCst);
                requests
                    .iter()
                    .take_while(|_| !stop.load(Ordering::Relaxed))
                    .map(|r| {
                        self.count.fetch_add(1, Ordering::SeqCst);
                        crate::metadata::read(&r.path)
                    })
                    .collect()
            }
        }
        let root =
            std::env::temp_dir().join(format!("diskweave-batch-probe-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        for i in 0..300 {
            fs::write(root.join(format!("{i}.bin")), b"123").unwrap();
        }
        let probe = Probe {
            count: AtomicUsize::new(0),
            maximum: AtomicUsize::new(0),
        };
        let result = scan_with_metadata_source(
            &root,
            true,
            Parallelism::Eight,
            directory::Strategy::Bulk,
            || false,
            |_| {},
            &probe,
        )
        .unwrap();
        assert_eq!(result.folders[0].logical_bytes, 900);
        assert_eq!(result.files.len(), 300);
        assert_eq!(probe.count.load(Ordering::SeqCst), 300);
        assert!((1..=128).contains(&probe.maximum.load(Ordering::SeqCst)));
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn sixteen_workers_preserve_totals_and_finish_all_queries_before_cancellation_returns() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let root = std::env::temp_dir().join(format!("diskweave-sixteen-{}", std::process::id()));
        for group in 0..16 {
            let dir = root.join(group.to_string());
            fs::create_dir_all(&dir).unwrap();
            for file in 0..128 {
                fs::write(dir.join(file.to_string()), b"12").unwrap();
            }
        }
        let active = AtomicUsize::new(0);
        let peak = AtomicUsize::new(0);
        let visited = AtomicUsize::new(0);
        let result = scan_parallel_with_probe(
            &root,
            true,
            Parallelism::Sixteen,
            || false,
            |_| {},
            |p| {
                let n = active.fetch_add(1, Ordering::SeqCst) + 1;
                peak.fetch_max(n, Ordering::SeqCst);
                std::thread::sleep(std::time::Duration::from_millis(1));
                let report = crate::metadata::read(p);
                active.fetch_sub(1, Ordering::SeqCst);
                report
            },
        )
        .unwrap();
        assert_eq!(
            (
                result.files.len(),
                result.folders.len(),
                result.folders[0].logical_bytes
            ),
            (2048, 17, 4096)
        );
        assert_eq!(active.load(Ordering::SeqCst), 0);
        assert!((2..=16).contains(&peak.load(Ordering::SeqCst)));
        let cancelled = scan_parallel_with_probe(
            &root,
            true,
            Parallelism::Sixteen,
            || visited.load(Ordering::SeqCst) > 50,
            |_| {},
            |p| {
                active.fetch_add(1, Ordering::SeqCst);
                std::thread::sleep(std::time::Duration::from_millis(1));
                let report = crate::metadata::read(p);
                visited.fetch_add(1, Ordering::SeqCst);
                active.fetch_sub(1, Ordering::SeqCst);
                report
            },
        );
        assert!(matches!(cancelled,Err(e) if e.kind()==io::ErrorKind::Interrupted));
        assert_eq!(active.load(Ordering::SeqCst), 0);
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn profiling_does_not_change_scan_fields_or_completed_worker_results() {
        let root =
            std::env::temp_dir().join(format!("diskweave-profile-fields-{}", std::process::id()));
        fs::create_dir_all(root.join("nested")).unwrap();
        fs::write(root.join("file"), b"12345").unwrap();
        fs::hard_link(root.join("file"), root.join("nested/alias")).unwrap();
        let plain = scan_with_strategy(
            &root,
            true,
            Parallelism::Four,
            directory::Strategy::Combined,
            || false,
            |_| {},
            None,
        )
        .unwrap();
        let metrics = ScanMetrics::default();
        let profiled = scan_with_strategy(
            &root,
            true,
            Parallelism::Four,
            directory::Strategy::Combined,
            || false,
            |_| {},
            Some(&metrics),
        )
        .unwrap();
        assert_eq!(
            (
                profiled.files.len(),
                profiled.folders[0].logical_bytes,
                profiled.folders[0].allocation
            ),
            (2, 10, plain.folders[0].allocation)
        );
        for f in &plain.files {
            let path = plain.folders[f.parent].path.join(&f.name);
            let found = profiled
                .files
                .iter()
                .find(|n| profiled.folders[n.parent].path.join(&n.name) == path)
                .unwrap();
            assert_eq!(
                (found.extra, found.modified, found.created),
                (f.extra, f.modified, f.created)
            );
        }
        let report = metrics.trace.snapshot();
        assert_eq!(report.stages[profile::Stage::Overall as usize].calls, 1);
        assert!(report.stages[profile::Stage::MetadataTask as usize].calls > 0);
        let cpu = report.worker_cpu.unwrap();
        assert_eq!(cpu.measured_samples + cpu.missing_samples, 4);
        fs::remove_dir_all(root).unwrap();
    }
    fn collection_strategies() -> Vec<directory::Strategy> {
        let mut modes = vec![
            directory::Strategy::LargeFetch,
            directory::Strategy::Bulk,
            directory::Strategy::Combined,
        ];
        for shared_handle in [false, true] {
            for reuse_buffer in [false, true] {
                for buffer_bytes in [65536, 1048576, 4194304] {
                    modes.push(directory::Strategy::Experimental(directory::Experiment {
                        shared_handle,
                        reuse_buffer,
                        buffer_bytes,
                    }));
                }
            }
        }
        modes
    }
    #[test]
    fn experimental_scans_exclude_junction_cycles_and_keep_basic_missing_fields() {
        use std::os::windows::process::CommandExt;
        let root =
            std::env::temp_dir().join(format!("diskweave-native-cycle-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("data"), b"12").unwrap();
        let cycle = root.join("cycle");
        let status = std::process::Command::new("cmd.exe")
            .args(["/d", "/c", "mklink", "/J"])
            .arg(&cycle)
            .arg(&root)
            .creation_flags(0x08000000)
            .output()
            .unwrap();
        assert!(status.status.success());
        for strategy in collection_strategies() {
            for detailed in [false, true] {
                let result = scan_with_strategy(
                    &root,
                    detailed,
                    Parallelism::Four,
                    strategy,
                    || false,
                    |_| {},
                    None,
                )
                .unwrap();
                assert_eq!(
                    (
                        result.files.len(),
                        result.folders.len(),
                        result.folders[0].logical_bytes,
                        result.skipped_reparse_points,
                        result.error_count
                    ),
                    (1, 1, 2, 1, 0)
                );
                if !detailed {
                    assert_eq!(result.files[0].extra, Default::default());
                    assert_eq!(result.folders[0].allocation.not_collected, 1);
                }
            }
        }
        fs::remove_dir(cycle).unwrap();
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn cancelling_native_candidates_releases_folder_handles_and_allows_another_scan() {
        use std::{cell::Cell, os::windows::fs::OpenOptionsExt};
        let root =
            std::env::temp_dir().join(format!("diskweave-native-cancel-{}", std::process::id()));
        for group in 0..16 {
            let folder = root.join(group.to_string());
            fs::create_dir_all(&folder).unwrap();
            for file in 0..128 {
                fs::write(folder.join(file.to_string()), b"12").unwrap();
            }
        }
        for strategy in collection_strategies() {
            let polls = Cell::new(0);
            let cancelled = scan_with_strategy(
                &root,
                true,
                Parallelism::Four,
                strategy,
                || {
                    polls.set(polls.get() + 1);
                    polls.get() > 8
                },
                |_| {},
                None,
            );
            assert!(matches!(cancelled,Err(e) if e.kind()==io::ErrorKind::Interrupted));
            for path in
                std::iter::once(root.clone()).chain((0..16).map(|n| root.join(n.to_string())))
            {
                let exclusive = fs::File::options()
                    .access_mode(0x80)
                    .share_mode(0)
                    .custom_flags(0x02200000)
                    .open(path)
                    .unwrap();
                drop(exclusive);
            }
            let complete = scan_with_strategy(
                &root,
                true,
                Parallelism::Four,
                strategy,
                || false,
                |_| {},
                None,
            )
            .unwrap();
            assert_eq!(
                (
                    complete.files.len(),
                    complete.folders.len(),
                    complete.folders[0].logical_bytes,
                    complete.metadata_error_count
                ),
                (2048, 17, 4096, 0)
            );
        }
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn collected_scan_keeps_reference_totals_fields_and_parent_relations() {
        let root = std::env::temp_dir().join(format!("diskweave-collected-{}", std::process::id()));
        fs::create_dir_all(root.join("nested")).unwrap();
        fs::write(root.join("original"), b"12345").unwrap();
        fs::hard_link(root.join("original"), root.join("nested/alias")).unwrap();
        let old = scan_with_probe(&root, true, || false, |_| {}, crate::metadata::read).unwrap();
        for strategy in collection_strategies() {
            let new = scan_with_strategy(
                &root,
                true,
                Parallelism::Four,
                strategy,
                || false,
                |_| {},
                None,
            )
            .unwrap();
            assert_eq!(
                (
                    new.folders[0].logical_bytes,
                    new.folders[0].file_count,
                    new.folders[0].folder_count
                ),
                (10, 2, 1)
            );
            for f in &old.files {
                let path = old.folders[f.parent].path.join(&f.name);
                let actual = new
                    .files
                    .iter()
                    .find(|n| new.folders[n.parent].path.join(&n.name) == path)
                    .unwrap();
                assert_eq!(
                    (
                        actual.extra,
                        actual.created,
                        actual.modified,
                        actual.attributes
                    ),
                    (f.extra, f.created, f.modified, f.attributes)
                );
            }
            assert_eq!(new.folders[0].allocation, old.folders[0].allocation);
        }
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn collected_modes_keep_long_paths_nonunicode_names_and_external_link_counts() {
        use std::os::windows::ffi::OsStringExt;
        let owned =
            std::env::temp_dir().join(format!("diskweave-collected-paths-{}", std::process::id()));
        let root = owned.join("scope");
        let mut deep = root.clone();
        for _ in 0..3 {
            deep = deep.join("long".repeat(20));
        }
        fs::create_dir_all(&deep).unwrap();
        fs::create_dir(root.join("empty")).unwrap();
        fs::write(root.join("data"), b"12345").unwrap();
        fs::hard_link(root.join("data"), owned.join("outside-link")).unwrap();
        let odd = std::ffi::OsString::from_wide(&[0xD800, 0x0061]);
        fs::write(root.join(&odd), b"").unwrap();
        fs::write(deep.join("file"), b"123").unwrap();
        let reference =
            scan_with_probe(&root, true, || false, |_| {}, crate::metadata::read).unwrap();
        for workers in [
            Parallelism::Sequential,
            Parallelism::Two,
            Parallelism::Four,
            Parallelism::Eight,
            Parallelism::Twelve,
            Parallelism::Sixteen,
        ] {
            for strategy in collection_strategies() {
                let result =
                    scan_with_strategy(&root, true, workers, strategy, || false, |_| {}, None)
                        .unwrap();
                assert_eq!(
                    (
                        result.files.len(),
                        result.folders.len(),
                        result.folders[0].logical_bytes,
                        result.error_count,
                        result.metadata_error_count
                    ),
                    (3, 5, 8, 0, 0)
                );
                for f in &reference.files {
                    let path = reference.folders[f.parent].path.join(&f.name);
                    let actual = result
                        .files
                        .iter()
                        .find(|f| result.folders[f.parent].path.join(&f.name) == path)
                        .unwrap();
                    assert_eq!(
                        (
                            actual.extra,
                            actual.modified,
                            actual.created,
                            actual.attributes
                        ),
                        (f.extra, f.modified, f.created, f.attributes)
                    );
                }
                assert_eq!(
                    result
                        .files
                        .iter()
                        .find(|f| f.name == "data")
                        .unwrap()
                        .extra
                        .links
                        .value(),
                    Some(2)
                );
                for folder in &result.folders {
                    let current = fs::metadata(&folder.path).unwrap();
                    assert_eq!(folder.modified, current.modified().ok());
                    assert_eq!(folder.created, current.created().ok());
                }
                assert_eq!(
                    scan_with_strategy(&root, true, workers, strategy, || true, |_| {}, None)
                        .err()
                        .unwrap()
                        .kind(),
                    io::ErrorKind::Interrupted
                );
            }
        }
        fs::remove_dir_all(owned).unwrap();
    }
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
    fn worker_modes_preserve_hard_link_paths_and_missing_metadata() {
        let root = std::env::temp_dir().join(format!(
            "diskweave-parallel-fields-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&root).unwrap();
        fs::write(root.join("original"), [0; 8193]).unwrap();
        fs::hard_link(root.join("original"), root.join("alias")).unwrap();
        fs::write(root.join("unavailable"), [1, 2, 3]).unwrap();
        let mut progress = Vec::new();
        for mode in [
            Parallelism::Sequential,
            Parallelism::Two,
            Parallelism::Four,
            Parallelism::Eight,
            Parallelism::Twelve,
            Parallelism::Sixteen,
        ] {
            progress.clear();
            let result = scan_parallel_with_probe(
                &root,
                true,
                mode,
                || false,
                |p| progress.push(p),
                |path| {
                    if path.file_name().unwrap() == "unavailable" {
                        crate::metadata::Report::unavailable(io::Error::from_raw_os_error(50))
                    } else {
                        crate::metadata::read(path)
                    }
                },
            )
            .unwrap();
            assert_eq!(
                (
                    result.files.len(),
                    result.folders[0].logical_bytes,
                    result.metadata_error_count
                ),
                (3, 16389, 1)
            );
            let original = result.files.iter().find(|f| f.name == "original").unwrap();
            let alias = result.files.iter().find(|f| f.name == "alias").unwrap();
            assert_eq!(original.extra.identity, alias.extra.identity);
            assert_eq!(original.extra.links.value(), Some(2));
            assert_eq!(result.folders[0].allocation.unsupported, 1);
            assert_eq!(result.folders[0].allocation.known, 2);
            assert_eq!(progress.last().unwrap().files_scanned, 3);
            assert!(progress
                .windows(2)
                .all(|p| p[0].files_scanned <= p[1].files_scanned));
        }
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn parallel_cancel_and_panicked_probe_leave_no_active_work_or_partial_result() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let root = std::env::temp_dir().join(format!(
            "diskweave-parallel-stop-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&root).unwrap();
        for file in 0..512 {
            fs::write(root.join(format!("item-{file}")), b"data").unwrap();
        }
        let active = AtomicUsize::new(0);
        let visited = AtomicUsize::new(0);
        let cancelled = scan_parallel_with_probe(
            &root,
            true,
            Parallelism::Four,
            || visited.load(Ordering::SeqCst) > 16,
            |_| {},
            |path| {
                active.fetch_add(1, Ordering::SeqCst);
                std::thread::sleep(std::time::Duration::from_millis(1));
                let report = crate::metadata::read(path);
                visited.fetch_add(1, Ordering::SeqCst);
                active.fetch_sub(1, Ordering::SeqCst);
                report
            },
        );
        assert!(matches!(cancelled,Err(e) if e.kind()==io::ErrorKind::Interrupted));
        assert_eq!(active.load(Ordering::SeqCst), 0);
        let before = visited.load(Ordering::SeqCst);
        std::thread::sleep(std::time::Duration::from_millis(20));
        assert_eq!(visited.load(Ordering::SeqCst), before);
        let failed = scan_parallel_with_probe(
            &root,
            true,
            Parallelism::Two,
            || false,
            |_| {},
            |_| panic!("독립 시험용 조회 실패"),
        );
        assert!(failed.is_err());
        let recovered = scan_parallel_with_probe(
            &root,
            false,
            Parallelism::Eight,
            || false,
            |_| {},
            crate::metadata::read,
        )
        .unwrap();
        assert_eq!(
            (recovered.files.len(), recovered.folders[0].logical_bytes),
            (512, 2048)
        );
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn parallel_scan_preserves_results_and_limits_concurrent_metadata_reads() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let root = std::env::temp_dir().join(format!(
            "diskweave-parallel-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        for group in 0..16 {
            let folder = root.join(format!("group-{group}"));
            fs::create_dir_all(&folder).unwrap();
            for file in 0..16 {
                fs::write(folder.join(format!("file-{file}")), [1, 2, 3]).unwrap();
            }
        }
        let sequential =
            scan_with_probe(&root, true, || false, |_| {}, crate::metadata::read).unwrap();
        let active = AtomicUsize::new(0);
        let peak = AtomicUsize::new(0);
        let parallel = scan_parallel_with_probe(
            &root,
            true,
            Parallelism::Four,
            || false,
            |_| {},
            |path| {
                let n = active.fetch_add(1, Ordering::SeqCst) + 1;
                peak.fetch_max(n, Ordering::SeqCst);
                std::thread::sleep(std::time::Duration::from_millis(1));
                let report = crate::metadata::read(path);
                active.fetch_sub(1, Ordering::SeqCst);
                report
            },
        )
        .unwrap();
        assert_eq!(active.load(Ordering::SeqCst), 0);
        assert!((2..=4).contains(&peak.load(Ordering::SeqCst)));
        assert_eq!(
            (
                parallel.files.len(),
                parallel.folders.len(),
                parallel.folders[0].logical_bytes,
                parallel.error_count,
                parallel.metadata_error_count
            ),
            (256, 17, 768, 0, 0)
        );
        let rows = |scan: &ScanResult| {
            scan.files
                .iter()
                .map(|f| {
                    (
                        scan.folders[f.parent].path.join(&f.name),
                        (
                            f.logical_bytes,
                            f.created,
                            f.modified,
                            f.attributes,
                            f.extra,
                        ),
                    )
                })
                .collect::<std::collections::BTreeMap<_, _>>()
        };
        let folders = |scan: &ScanResult| {
            scan.folders
                .iter()
                .map(|f| {
                    (
                        f.path.clone(),
                        (
                            f.parent.map(|id| scan.folders[id].path.clone()),
                            f.logical_bytes,
                            f.file_count,
                            f.folder_count,
                            f.created,
                            f.modified,
                            f.attributes,
                            f.allocation,
                        ),
                    )
                })
                .collect::<std::collections::BTreeMap<_, _>>()
        };
        assert_eq!(folders(&parallel), folders(&sequential));
        assert_eq!(rows(&parallel), rows(&sequential));
        assert_eq!(
            parallel.folders[0].allocation,
            sequential.folders[0].allocation
        );
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    #[ignore = "독립 임시 1만 파일의 기본/상세 스캔 정확도·Release 시간 측정"]
    fn benchmark_basic_and_detailed_scan_modes() {
        let root = std::env::temp_dir().join(format!(
            "diskweave-scan-benchmark-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        for group in 0..100 {
            let folder = root.join(format!("group-{group}"));
            fs::create_dir_all(&folder).unwrap();
            for file in 0..100 {
                fs::write(folder.join(format!("file-{file}.bin")), [1, 2, 3, 4]).unwrap();
            }
        }
        for detailed in [false, true] {
            let started = std::time::Instant::now();
            let result = scan_with_options(&root, detailed, || false, |_| {}).unwrap();
            assert_eq!(
                (
                    result.files.len(),
                    result.folders.len(),
                    result.folders[0].logical_bytes,
                    result.error_count,
                    result.metadata_error_count
                ),
                (10000, 101, 40000, 0, 0)
            );
            assert_eq!(
                result.folders[0].allocation.not_collected,
                if detailed { 0 } else { 10000 }
            );
            println!(
                "detailed={detailed} files=10000 folders=101 bytes=40000 elapsed_ms={:.3}",
                started.elapsed().as_secs_f64() * 1000.0
            );
        }
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn folder_times_are_refreshed_after_directory_enumeration() {
        use std::os::windows::fs::OpenOptionsExt;
        let root = std::env::temp_dir().join(format!(
            "diskweave-folder-time-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(root.join("nested")).unwrap();
        fs::write(root.join("entry"), b"123").unwrap();
        let expected = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_600_000_123);
        // 열거 직후 콜백에서 시각을 바꾸는 순차 수집의 회귀 조건 유지.
        let result = scan_parallel_with_probe(
            &root,
            false,
            Parallelism::Sequential,
            || false,
            |progress| {
                if progress.folders_scanned == 1 {
                    let handle = fs::File::options()
                        .access_mode(0x100)
                        .share_mode(7)
                        .custom_flags(0x02000000)
                        .open(root.join("nested"))
                        .unwrap();
                    handle.set_modified(expected).unwrap();
                }
            },
            crate::metadata::read,
        )
        .unwrap();
        let folder = result.folder_index(Path::new("nested")).unwrap();
        assert_eq!(result.folders[folder].modified, Some(expected));
        assert_eq!(
            (
                result.folders[0].logical_bytes,
                result.folders[0].file_count
            ),
            (3, 1)
        );
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
#[path = "scanner/preview.rs"]
pub mod preview;

/// 중간 폴더 갱신과 완료 결과를 분리하여 전달하는 GUI 수집 경계.
pub fn scan_preview<'a>(
    root: &Path,
    detailed: bool,
    parallelism: Parallelism,
    cancelled: impl Fn() -> bool,
    progress: impl FnMut(ScanProgress),
    preview: &'a mut preview::Observer<'a>,
) -> io::Result<ScanResult> {
    parallel::scan_observed(
        root,
        detailed,
        parallelism.workers(),
        parallel::Collection {
            strategy: Default::default(),
            metrics: None,
        },
        cancelled,
        progress,
        &crate::metadata::read,
        Some(preview),
    )
}
