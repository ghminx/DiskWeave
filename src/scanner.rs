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
use std::sync::Arc;
use std::time::SystemTime;

const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
const MAX_REPORTED_ERRORS: usize = 20;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum FolderAccess {
    #[default]
    Complete,
    Partial,
    Denied,
    Unavailable,
}
impl FolderAccess {
    pub fn unreadable(self) -> bool {
        matches!(self, Self::Denied | Self::Unavailable)
    }
    pub fn label(self) -> &'static str {
        match self {
            Self::Complete => "",
            Self::Partial => "일부 누락",
            Self::Denied => "접근 불가",
            Self::Unavailable => "조회 실패",
        }
    }
}

#[derive(Clone)]
pub struct Folder {
    pub access: FolderAccess,
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

impl Clone for ScanIssue {
    fn clone(&self) -> Self {
        let error = self.error.raw_os_error().map_or_else(
            || io::Error::new(self.error.kind(), self.error.to_string()),
            io::Error::from_raw_os_error,
        );
        Self {
            path: self.path.clone(),
            error,
        }
    }
}

pub(crate) struct PartialUpdate {
    pub previous: std::sync::Weak<ScanResult>,
    pub affected: std::collections::HashSet<usize>,
    pub search: Option<SearchDelta>,
}

pub(crate) struct SearchDelta {
    pub before: Vec<crate::explorer::Item>,
    pub after: Vec<crate::explorer::Item>,
    pub file_rows: Option<RowRemap>,
    pub folder_rows: Option<RowRemap>,
}

impl SearchDelta {
    pub(crate) fn remap(&self, item: crate::explorer::Item) -> Option<crate::explorer::Item> {
        use crate::explorer::Item;
        match item {
            Item::File(row) => self
                .file_rows
                .as_ref()
                .map_or(Some(item), |rows| rows.get(row).map(Item::File)),
            Item::Folder(row) => self
                .folder_rows
                .as_ref()
                .map_or(Some(item), |rows| rows.get(row).map(Item::Folder)),
        }
    }
}

#[derive(Default)]
pub(crate) struct RowRemap {
    runs: Vec<(usize, usize, usize)>,
}
impl RowRemap {
    pub(crate) fn push_ordered(&mut self, old: usize, new: usize) {
        if let Some((start, end, first)) = self.runs.last_mut() {
            if old == *end && new == *first + (old - *start) {
                *end += 1;
                return;
            }
        }
        self.runs.push((old, old + 1, new));
    }

    pub(crate) fn get(&self, old: usize) -> Option<usize> {
        let at = self
            .runs
            .partition_point(|&(start, _, _)| start <= old)
            .checked_sub(1)?;
        let (start, end, first) = self.runs[at];
        (old < end).then(|| first + old - start)
    }
}

#[derive(Clone)]
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

// 이전 화면과 행 묶음을 공유하고 수정이 필요한 묶음만 복사.
#[derive(Clone)]
pub struct Rows<T> {
    chunks: Vec<Arc<Vec<T>>>,
    len: usize,
}

impl<T> Default for Rows<T> {
    fn default() -> Self {
        Self {
            chunks: Vec::new(),
            len: 0,
        }
    }
}

impl<T> Rows<T> {
    const CHUNK: usize = 256;

    pub fn len(&self) -> usize {
        self.len
    }
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
    pub fn get(&self, index: usize) -> Option<&T> {
        self.chunks
            .get(index / Self::CHUNK)?
            .get(index % Self::CHUNK)
    }
    pub fn push(&mut self, file: T)
    where
        T: Clone,
    {
        if self.len.is_multiple_of(Self::CHUNK) {
            self.chunks.push(Arc::new(Vec::with_capacity(Self::CHUNK)));
        }
        Arc::make_mut(self.chunks.last_mut().unwrap()).push(file);
        self.len += 1;
    }
    pub fn first(&self) -> Option<&T> {
        self.get(0)
    }
    pub fn clear(&mut self) {
        *self = Self::default();
    }
    pub(crate) fn append_reused_chunk(&mut self, previous: &Self, len: usize) -> bool {
        if !self.len.is_multiple_of(Self::CHUNK) {
            return false;
        }
        let Some(chunk) = previous
            .chunks
            .get(self.len / Self::CHUNK)
            .filter(|chunk| chunk.len() == len)
        else {
            return false;
        };
        self.chunks.push(Arc::clone(chunk));
        self.len += len;
        true
    }
    #[cfg(test)]
    pub(crate) fn shares_row_with(&self, previous: &Self, row: usize) -> bool {
        self.chunks
            .get(row / Self::CHUNK)
            .zip(previous.chunks.get(row / Self::CHUNK))
            .is_some_and(|(a, b)| Arc::ptr_eq(a, b))
    }
    pub(crate) const fn chunk_size() -> usize {
        Self::CHUNK
    }
    pub fn iter(&self) -> impl DoubleEndedIterator<Item = &T> {
        self.into_iter()
    }
}

impl<T> std::ops::Index<usize> for Rows<T> {
    type Output = T;
    fn index(&self, index: usize) -> &T {
        &self.chunks[index / Self::CHUNK][index % Self::CHUNK]
    }
}

impl<T: Clone> std::ops::IndexMut<usize> for Rows<T> {
    fn index_mut(&mut self, index: usize) -> &mut T {
        &mut Arc::make_mut(&mut self.chunks[index / Self::CHUNK])[index % Self::CHUNK]
    }
}

impl<'a, T> IntoIterator for &'a Rows<T> {
    type Item = &'a T;
    type IntoIter = std::iter::FlatMap<
        std::slice::Iter<'a, Arc<Vec<T>>>,
        std::slice::Iter<'a, T>,
        fn(&'a Arc<Vec<T>>) -> std::slice::Iter<'a, T>,
    >;
    fn into_iter(self) -> Self::IntoIter {
        self.chunks.iter().flat_map(|chunk| chunk.iter())
    }
}

impl<T: Clone> FromIterator<T> for Rows<T> {
    fn from_iter<I: IntoIterator<Item = T>>(iter: I) -> Self {
        let mut files = Self::default();
        for file in iter {
            files.push(file);
        }
        files
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ScanProgress {
    pub folders_scanned: u64,
    pub files_scanned: u64,
}

#[derive(Clone)]
pub struct ScanResult {
    pub(crate) partial: Option<std::sync::Arc<PartialUpdate>>,
    pub detailed: bool,
    pub metadata_error_count: u64,
    pub metadata_issues: Vec<ScanIssue>,
    pub folders: Rows<Folder>,
    pub files: Rows<FileEntry>,
    pub children: Vec<Arc<Vec<usize>>>,
    pub file_children: Vec<Arc<Vec<usize>>>,
    pub error_count: u64,
    pub issues: Vec<ScanIssue>,
    pub skipped_reparse_points: u64,
}

impl ScanResult {
    pub(crate) fn copy_for_update(&self, keep_files: bool) -> Self {
        Self {
            partial: None,
            detailed: self.detailed,
            metadata_error_count: self.metadata_error_count,
            metadata_issues: self.metadata_issues.clone(),
            folders: self.folders.clone(),
            files: if keep_files {
                self.files.clone()
            } else {
                Rows::default()
            },
            children: self.children.clone(),
            file_children: self.file_children.clone(),
            error_count: self.error_count,
            issues: self.issues.clone(),
            skipped_reparse_points: self.skipped_reparse_points,
        }
    }

    pub(crate) fn changes_from(&self, previous: &Self) -> Option<&SearchDelta> {
        let update = self.partial.as_ref()?;
        if !std::ptr::eq(update.previous.as_ptr(), previous) {
            return None;
        }
        update.search.as_ref()
    }

    pub fn subtree_unchanged_from(&self, previous: &Self, folder: usize) -> bool {
        self.partial.as_ref().is_some_and(|update| {
            std::ptr::eq(update.previous.as_ptr(), previous)
                && folder < self.folders.len()
                && !update.affected.contains(&folder)
        })
    }

    #[cfg(test)]
    pub fn has_partial_update_from(&self, previous: &Self) -> bool {
        self.partial
            .as_ref()
            .is_some_and(|update| std::ptr::eq(update.previous.as_ptr(), previous))
    }

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
                !folder.access.unreadable()
                    && folder
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
    pub attributes: u32,
    pub index: usize,
    pub path: PathBuf,
    pub identity: Option<crate::metadata::Identity>,
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
#[allow(dead_code, reason = "이전 수집 기준·개발 도구·회귀 대조 유지")]
pub fn scan(root: &Path, cancelled: impl Fn() -> bool) -> io::Result<ScanResult> {
    scan_with_progress(root, cancelled, |_| {})
}

#[allow(dead_code, reason = "이전 수집 기준·개발 도구·회귀 대조 유지")]
pub fn scan_with_progress(
    root: &Path,
    cancelled: impl Fn() -> bool,
    on_progress: impl FnMut(ScanProgress),
) -> io::Result<ScanResult> {
    scan_with_options(root, false, cancelled, on_progress)
}
#[allow(dead_code, reason = "이전 수집 기준·개발 도구·회귀 대조 유지")]
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
                result.folders[folder_index].access =
                    if error.kind() == io::ErrorKind::PermissionDenied {
                        FolderAccess::Denied
                    } else {
                        FolderAccess::Unavailable
                    };
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
                    result.folders[folder_index].access = FolderAccess::Partial;
                    result.record_error(path.clone(), error);
                    break;
                }
            };
            let entry_path = entry.path();
            // Windows 열거 결과의 메타데이터 재사용. 링크를 따라가지 않으며 항목별 추가 시스템 호출 방지.
            let metadata = match entry.metadata() {
                Ok(metadata) => metadata,
                Err(error) => {
                    result.folders[folder_index].access = FolderAccess::Partial;
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
                        result.folders[folder_index].access = FolderAccess::Partial;
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
                    access: Default::default(),
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
        partial: None,
        detailed,
        metadata_error_count: 0,
        metadata_issues: Vec::new(),
        folders: [Folder {
            access: Default::default(),
            allocation: Default::default(),
            created: metadata.created().ok(),
            attributes: metadata.file_attributes(),
            path: root.to_path_buf(),
            parent: None,
            logical_bytes: 0,
            file_count: 0,
            folder_count: 0,
            modified: metadata.modified().ok(),
        }]
        .into_iter()
        .collect(),
        children: Vec::new(),
        files: Default::default(),
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
        let child = &result.folders[child_index];
        let (parent, access, allocation, bytes, files, folders) = (
            child.parent.expect("루트 외 폴더의 부모"),
            child.access,
            child.allocation,
            child.logical_bytes,
            child.file_count,
            child.folder_count,
        );
        let parent = &mut result.folders[parent];
        if access != FolderAccess::Complete {
            parent.access = FolderAccess::Partial;
        }
        parent.allocation.merge(allocation)?;
        parent.logical_bytes = parent.logical_bytes.checked_add(bytes).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "논리 크기 합계가 표현 범위를 넘습니다",
            )
        })?;
        parent.file_count += files;
        parent.folder_count += folders + 1;
    }

    for row in 0..result.folders.len() {
        let folder = &mut result.folders[row];
        if folder.access.unreadable() && (folder.file_count > 0 || folder.folder_count > 0) {
            folder.access = FolderAccess::Partial;
        }
    }
    finish_lists(result);
    Ok(())
}

pub(crate) fn finish_lists(result: &mut ScanResult) {
    result.children = vec![Arc::default(); result.folders.len()];
    result.file_children = vec![Arc::default(); result.folders.len()];
    for (index, file) in result.files.iter().enumerate() {
        Arc::make_mut(&mut result.file_children[file.parent]).push(index);
    }
    for siblings in &mut result.file_children {
        if siblings.len() < 2 {
            continue;
        }
        Arc::make_mut(siblings).sort_unstable_by(|&a, &b| {
            result.files[b]
                .logical_bytes
                .cmp(&result.files[a].logical_bytes)
                .then_with(|| result.files[a].name.cmp(&result.files[b].name))
        });
    }
    for (index, folder) in result.folders.iter().enumerate().skip(1) {
        Arc::make_mut(&mut result.children[folder.parent.expect("루트 외 폴더의 부모")])
            .push(index);
    }
    for siblings in &mut result.children {
        if siblings.len() < 2 {
            continue;
        }
        Arc::make_mut(siblings).sort_unstable_by(|&a, &b| {
            result.folders[b]
                .logical_bytes
                .cmp(&result.folders[a].logical_bytes)
                .then_with(|| result.folders[a].path.cmp(&result.folders[b].path))
        });
    }
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
                assert!(requests.iter().all(|r| r.allocation.is_some()));
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

/// 과거 수집 모드의 미리보기 계약 검증용 수집.
#[cfg(test)]
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

/// 필수 정보의 일괄 열거 값을 재사용하고 부족한 항목만 OS 조회로 보완.
pub fn scan_unified<'a>(
    root: &Path,
    parallelism: Parallelism,
    cancelled: impl Fn() -> bool,
    progress: impl FnMut(ScanProgress),
    observer: Option<&'a mut preview::Observer<'a>>,
) -> io::Result<ScanResult> {
    let source = crate::metadata::Named::for_root(root)?;
    struct Probe<'a>(&'a crate::metadata::Named);
    impl MetadataProbe for Probe<'_> {
        fn read_batch(
            &self,
            requests: &[MetadataRequest],
            stop: &std::sync::atomic::AtomicBool,
        ) -> Vec<crate::metadata::Report> {
            requests
                .iter()
                .take_while(|_| !stop.load(std::sync::atomic::Ordering::Relaxed))
                .map(|r| {
                    if r.attributes & 0xA00 == 0 {
                        if let (Some(identity), Some(allocation)) = (r.identity, r.allocation) {
                            return crate::metadata::Report {
                                values: crate::metadata::Extra {
                                    identity: crate::metadata::Field::Value(identity),
                                    allocation: crate::metadata::Field::Value(allocation),
                                    links: Default::default(),
                                },
                                issue: None,
                            };
                        }
                    }
                    let report = crate::metadata::read_combined(&r.path, self.0, None);
                    if report.issue.is_none()
                        && r.identity.is_some()
                        && report.values.identity.value() != r.identity
                    {
                        return crate::metadata::Report::unavailable(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "보완 조회 중 파일이 다른 객체로 변경됐습니다",
                        ));
                    }
                    report
                })
                .collect()
        }
    }
    scan_metadata_observed(
        root,
        true,
        parallelism,
        cancelled,
        progress,
        &Probe(&source),
        observer,
    )
}

#[cfg(test)]
mod unified_tests {
    #[test]
    fn unified_scan_keeps_required_allocation_identity_and_preview_without_requiring_link_counts() {
        use super::*;
        let root = std::env::temp_dir().join(format!("diskweave-unified-{}", std::process::id()));
        fs::create_dir_all(root.join("child")).unwrap();
        fs::write(root.join("known"), b"1234567890123").unwrap();
        fs::hard_link(root.join("known"), root.join("child/alias")).unwrap();
        let mut model = preview::Model::new(root.clone());
        let result = scan_unified(
            &root,
            Parallelism::Two,
            || false,
            |_| {},
            Some(&mut |batch| model.apply(batch)),
        )
        .unwrap();
        assert_eq!(
            (result.files.len(), result.folders[0].logical_bytes),
            (2, 26)
        );
        let first = &result.files[0];
        assert!(first.extra.identity.value().is_some());
        assert!(first.extra.allocation.value().is_some());
        assert_eq!(first.extra.identity, result.files[1].extra.identity);
        assert!(first.created.is_some() && first.modified.is_some());
        assert_eq!((model.folders[0].bytes, model.folders[0].files), (26, 2));
        assert_eq!(
            scan_unified(&root, Parallelism::Two, || true, |_| {}, None)
                .err()
                .unwrap()
                .kind(),
            io::ErrorKind::Interrupted
        );
        fs::remove_dir_all(root).unwrap();
    }
}
