use crate::explorer::Item;
use crate::scanner::ScanResult;
use std::ffi::OsStr;
use std::io;
use std::path::PathBuf;
use std::time::SystemTime;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SearchInput {
    pub text: String,
    pub recursive: bool,
    pub mode: SearchMode,
    pub kind: EntryKind,
    pub extensions: String,
    pub min_size: String,
    pub max_size: String,
    pub from_date: String,
    pub to_date: String,
    pub sort: SortField,
    pub descending: bool,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum SortField {
    Name,
    Path,
    #[default]
    Size,
    Modified,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SearchMode {
    #[default]
    Name,
    Path,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum EntryKind {
    #[default]
    All,
    Files,
    #[allow(dead_code)] // 필터 UI 제거 후 기존 검색 API 조건 유지
    Folders,
}
impl Default for SearchInput {
    fn default() -> Self {
        Self {
            text: String::new(),
            recursive: true,
            mode: SearchMode::Name,
            kind: EntryKind::All,
            extensions: String::new(),
            min_size: String::new(),
            max_size: String::new(),
            from_date: String::new(),
            to_date: String::new(),
            sort: SortField::Size,
            descending: true,
        }
    }
}
pub struct Query {
    pub scope: usize,
    pub recursive: bool,
    text: String,
    mode: SearchMode,
    kind: EntryKind,
    extensions: Vec<String>,
    min_size: Option<u64>,
    max_size: Option<u64>,
    from: Option<SystemTime>,
    to: Option<SystemTime>,
    active: bool,
    sort: SortField,
    descending: bool,
}
pub struct SearchResults {
    pub items: Vec<Item>,
    pub unknown_dates: usize,
}

impl SearchInput {
    pub fn is_active(&self) -> bool {
        !self.text.trim().is_empty()
            || self.kind != EntryKind::All
            || !self.extensions.trim().is_empty()
            || !self.min_size.trim().is_empty()
            || !self.max_size.trim().is_empty()
            || !self.from_date.trim().is_empty()
            || !self.to_date.trim().is_empty()
    }
    pub fn compile(&self, scope: usize) -> io::Result<Query> {
        let min_size = if self.min_size.trim().is_empty() {
            None
        } else {
            Some(self.min_size.trim().parse().map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "최소 크기에 0 이상의 정수를 입력하세요. 단위는 바이트(B)입니다.",
                )
            })?)
        };
        let max_size = if self.max_size.trim().is_empty() {
            None
        } else {
            Some(self.max_size.trim().parse().map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "최대 크기에 0 이상의 정수를 입력하세요. 단위는 바이트(B)입니다.",
                )
            })?)
        };
        if min_size.zip(max_size).is_some_and(|(min, max)| min > max) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "최소 크기가 최대 크기보다 큽니다",
            ));
        }
        let extensions: Vec<_> = self
            .extensions
            .split(';')
            .map(|s| s.trim().trim_start_matches('.').to_lowercase())
            .filter(|s| !s.is_empty())
            .collect();
        if (!self.extensions.trim().is_empty() && extensions.is_empty())
            || extensions
                .iter()
                .any(|ext| ext.contains(['*', '?', '\\', '/', ':', ' ']))
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "확장자는 npy;nii.gz 형식으로 입력하세요",
            ));
        }
        let (from, to) = crate::local_date::range(&self.from_date, &self.to_date)?;
        Ok(Query {
            scope,
            recursive: self.recursive,
            text: self.text.trim().to_lowercase(),
            mode: self.mode,
            kind: self.kind,
            extensions,
            min_size,
            max_size,
            from,
            to,
            active: self.is_active(),
            sort: self.sort,
            descending: self.descending,
        })
    }
}

pub fn item_name(result: &ScanResult, item: Item) -> &OsStr {
    match item {
        Item::Folder(id) => result.folders[id]
            .path
            .file_name()
            .unwrap_or(result.folders[id].path.as_os_str()),
        Item::File(id) => &result.files[id].name,
    }
}

pub fn item_path(result: &ScanResult, item: Item) -> PathBuf {
    match item {
        Item::Folder(id) => result.folders[id].path.clone(),
        Item::File(id) => result.folders[result.files[id].parent]
            .path
            .join(&result.files[id].name),
    }
}

pub fn item_bytes(result: &ScanResult, item: Item) -> u64 {
    match item {
        Item::Folder(id) => result.folders[id].logical_bytes,
        Item::File(id) => result.files[id].logical_bytes,
    }
}

// 위치별 검색과 전체 검색의 정렬 규칙 공유 및 비교마다 문자열 재변환 방지.
#[derive(PartialEq, Eq, PartialOrd, Ord)]
enum SortValue {
    Name(String),
    Path,
    Size(u64),
    Modified(Option<SystemTime>),
}
pub(crate) struct SortKey {
    path: PathBuf,
    value: SortValue,
    is_file: bool,
}
impl SortKey {
    pub(crate) fn new(result: &ScanResult, item: Item, field: SortField) -> Self {
        Self {
            path: item_path(result, item),
            value: match field {
                SortField::Name => {
                    SortValue::Name(item_name(result, item).to_string_lossy().to_lowercase())
                }
                SortField::Path => SortValue::Path,
                SortField::Size => SortValue::Size(item_bytes(result, item)),
                SortField::Modified => SortValue::Modified(item_modified(result, item)),
            },
            is_file: matches!(item, Item::File(_)),
        }
    }
    // 동일한 정렬 조건의 키끼리 비교. 미확인 날짜는 마지막, 동률 경로는 오름차순 유지.
    pub(crate) fn compare(
        &self,
        other: &Self,
        descending: bool,
        folders_first: bool,
    ) -> std::cmp::Ordering {
        let group = if folders_first {
            self.is_file.cmp(&other.is_file)
        } else {
            std::cmp::Ordering::Equal
        };
        let missing = matches!(self.value, SortValue::Modified(None))
            .cmp(&matches!(other.value, SortValue::Modified(None)));
        let order = if matches!(self.value, SortValue::Path) {
            self.path.cmp(&other.path)
        } else {
            self.value.cmp(&other.value)
        };
        group
            .then(missing)
            .then(if descending { order.reverse() } else { order })
            .then_with(|| self.path.cmp(&other.path))
    }
}

pub fn search(
    result: &ScanResult,
    query: &Query,
    cancelled: impl Fn() -> bool,
) -> io::Result<SearchResults> {
    let result_items = matching(result, query, &cancelled)?;
    let mut keyed = Vec::with_capacity(result_items.items.len());
    for item in result_items.items {
        crate::output::check(&cancelled)?;
        keyed.push((item, SortKey::new(result, item, query.sort)));
    }
    keyed.sort_unstable_by(|a, b| a.1.compare(&b.1, query.descending, !query.active));
    crate::output::check(&cancelled)?;
    Ok(SearchResults {
        items: keyed.into_iter().map(|(item, _)| item).collect(),
        unknown_dates: result_items.unknown_dates,
    })
}

pub(crate) fn matching(
    result: &ScanResult,
    query: &Query,
    cancelled: impl Fn() -> bool,
) -> io::Result<SearchResults> {
    if cancelled() {
        return Err(io::Error::new(io::ErrorKind::Interrupted, "검색 취소"));
    }
    if query.scope >= result.folders.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "검색 범위가 현재 스캔에 없습니다",
        ));
    }
    let mut items = Vec::new();
    let mut unknown_dates = 0;
    let mut pending = vec![query.scope];
    while let Some(folder) = pending.pop() {
        for item in result.children[folder]
            .iter()
            .copied()
            .map(Item::Folder)
            .chain(result.file_children[folder].iter().copied().map(Item::File))
        {
            if cancelled() {
                return Err(io::Error::new(io::ErrorKind::Interrupted, "검색 취소"));
            }
            if matches!(
                (query.kind, item),
                (EntryKind::Files, Item::Folder(_)) | (EntryKind::Folders, Item::File(_))
            ) {
                continue;
            }
            if query.active
                && matches!(item, Item::Folder(id) if result.folders[id].access.unreadable())
            {
                continue;
            }
            let bytes = item_bytes(result, item);
            if query.min_size.is_some_and(|min| bytes < min)
                || query.max_size.is_some_and(|max| bytes > max)
            {
                continue;
            }
            let name = item_name(result, item).to_string_lossy().to_lowercase();
            if !query.extensions.is_empty()
                && (!matches!(item, Item::File(_))
                    || !query
                        .extensions
                        .iter()
                        .any(|ext| name.ends_with(&format!(".{ext}"))))
            {
                continue;
            }
            let text = if query.mode == SearchMode::Path {
                std::path::absolute(item_path(result, item))?
                    .to_string_lossy()
                    .to_lowercase()
            } else {
                name
            };
            if !text.contains(&query.text) {
                continue;
            }
            if query.from.is_some() || query.to.is_some() {
                let Some(modified) = item_modified(result, item) else {
                    unknown_dates += 1;
                    continue;
                };
                if query.from.is_some_and(|from| modified < from)
                    || query.to.is_some_and(|to| modified >= to)
                {
                    continue;
                }
            }
            items.push(item);
        }
        if query.recursive && query.active {
            pending.extend(result.children[folder].iter().copied());
        }
    }
    Ok(SearchResults {
        items,
        unknown_dates,
    })
}

pub fn item_modified(result: &ScanResult, item: Item) -> Option<SystemTime> {
    match item {
        Item::Folder(id) => result.folders[id].modified,
        Item::File(id) => result.files[id].modified,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scanner;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};

    struct Fixture {
        root: PathBuf,
        scan: crate::scanner::ScanResult,
    }
    impl Fixture {
        fn new() -> Self {
            static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let root = std::env::temp_dir().join(format!(
                "diskweave-query-{}-{}-{}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_nanos(),
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            fs::create_dir_all(root.join("Data").join("deep")).unwrap();
            fs::create_dir(root.join("Database")).unwrap();
            fs::write(root.join("Data").join("MODEL.npy"), [0; 3]).unwrap();
            fs::write(root.join("Data").join("deep").join("model.nii.gz"), [0; 7]).unwrap();
            fs::write(root.join("Database").join("model.npy"), [0; 9]).unwrap();
            for (path, seconds) in [
                ("Data\\MODEL.npy", 1_600_000_000),
                ("Data\\deep\\model.nii.gz", 1_610_000_000),
                ("Database\\model.npy", 1_620_000_000),
            ] {
                fs::File::options()
                    .write(true)
                    .open(root.join(path))
                    .unwrap()
                    .set_times(
                        fs::FileTimes::new()
                            .set_modified(UNIX_EPOCH + std::time::Duration::from_secs(seconds)),
                    )
                    .unwrap();
            }
            let scan = scanner::scan(&root, || false).unwrap();
            Self { root, scan }
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.root).unwrap();
        }
    }

    #[test]
    #[ignore = "실제 임시 파일 1만 개를 생성하는 Release 검색 측정"]
    fn benchmark_ten_thousand_files_and_deep_scope() {
        use std::time::Instant;
        let mut fixture = Fixture::new();
        fs::create_dir(fixture.root.join("bulk")).unwrap();
        for index in 0..10_000 {
            fs::write(
                fixture
                    .root
                    .join("bulk")
                    .join(format!("bench_{index:05}.npy")),
                [1],
            )
            .unwrap();
        }
        let mut deep = fixture.root.clone();
        for _ in 0..120 {
            deep.push("d");
        }
        fs::create_dir_all(&deep).unwrap();
        fs::write(deep.join("bench_deep.npy"), [1]).unwrap();
        let started = Instant::now();
        fixture.scan = scanner::scan(&fixture.root, || false).unwrap();
        eprintln!(
            "BENCH scan_files={} scan_ms={:.3}",
            fixture.scan.files.len(),
            started.elapsed().as_secs_f64() * 1000.0
        );
        for (label, input, expected) in [
            (
                "name",
                SearchInput {
                    text: "bench_".into(),
                    kind: EntryKind::Files,
                    ..Default::default()
                },
                10_001,
            ),
            (
                "path",
                SearchInput {
                    text: "bulk\\".into(),
                    mode: SearchMode::Path,
                    kind: EntryKind::Files,
                    ..Default::default()
                },
                10_000,
            ),
            (
                "absent",
                SearchInput {
                    text: "no_such_benchmark_file".into(),
                    ..Default::default()
                },
                0,
            ),
            (
                "extension",
                SearchInput {
                    extensions: "nii.gz".into(),
                    ..Default::default()
                },
                1,
            ),
        ] {
            let query = input.compile(0).unwrap();
            assert_eq!(
                search(&fixture.scan, &query, || false).unwrap().items.len(),
                expected
            );
            let mut timings = Vec::new();
            for _ in 0..5 {
                let started = Instant::now();
                let hits = search(&fixture.scan, &query, || false).unwrap();
                timings.push(started.elapsed().as_secs_f64() * 1000.0);
                assert_eq!(hits.items.len(), expected);
            }
            timings.sort_by(f64::total_cmp);
            eprintln!(
                "BENCH query={label} hits={expected} median_ms={:.3} max_ms={:.3} samples=5",
                timings[2], timings[4]
            );
        }
    }

    #[test]
    fn columns_sort_in_both_directions_and_blank_query_keeps_direct_listing() {
        let fixture = Fixture::new();
        let input = SearchInput {
            kind: EntryKind::Files,
            sort: SortField::Size,
            descending: false,
            ..Default::default()
        };
        let hits = search(&fixture.scan, &input.compile(0).unwrap(), || false).unwrap();
        assert_eq!(
            hits.items
                .iter()
                .map(|&item| item_bytes(&fixture.scan, item))
                .collect::<Vec<_>>(),
            [3, 7, 9]
        );
        for (field, expected_ascending, expected_descending) in [
            (SortField::Name, [7, 3, 9], [3, 9, 7]),
            (SortField::Path, [3, 7, 9], [9, 7, 3]),
            (SortField::Modified, [3, 7, 9], [9, 7, 3]),
        ] {
            let mut input = input.clone();
            input.sort = field;
            let ascending = search(&fixture.scan, &input.compile(0).unwrap(), || false).unwrap();
            input.descending = true;
            let descending = search(&fixture.scan, &input.compile(0).unwrap(), || false).unwrap();
            assert_eq!(
                ascending
                    .items
                    .iter()
                    .map(|&item| item_bytes(&fixture.scan, item))
                    .collect::<Vec<_>>(),
                expected_ascending
            );
            assert_eq!(
                descending
                    .items
                    .iter()
                    .map(|&item| item_bytes(&fixture.scan, item))
                    .collect::<Vec<_>>(),
                expected_descending
            );
        }
        let hits = search(
            &fixture.scan,
            &SearchInput::default().compile(0).unwrap(),
            || false,
        )
        .unwrap();
        assert_eq!(hits.items.len(), 2);
        assert!(hits
            .items
            .iter()
            .all(|item| matches!(item, Item::Folder(_))));
        let input = SearchInput {
            kind: EntryKind::Folders,
            ..Default::default()
        };
        assert_eq!(
            search(&fixture.scan, &input.compile(0).unwrap(), || false)
                .unwrap()
                .items
                .len(),
            3
        );
    }

    #[test]
    fn date_range_filters_metadata_and_rejects_invalid_calendar_dates() {
        use std::time::Duration;
        let mut fixture = Fixture::new();
        let file = fs::File::options()
            .write(true)
            .open(fixture.root.join("Data").join("MODEL.npy"))
            .unwrap();
        // UTC 2024-02-29 12:00은 모든 Windows 시간대에서 지정 3일 범위 안에 위치.
        file.set_times(
            fs::FileTimes::new().set_modified(UNIX_EPOCH + Duration::from_secs(1_709_208_000)),
        )
        .unwrap();
        drop(file);
        fixture.scan = scanner::scan(&fixture.root, || false).unwrap();
        let input = SearchInput {
            from_date: "2024-02-28".into(),
            to_date: "2024-03-01".into(),
            kind: EntryKind::Files,
            ..Default::default()
        };
        let hits = search(&fixture.scan, &input.compile(0).unwrap(), || false).unwrap();
        assert_eq!(hits.items.len(), 1);
        assert_eq!(item_name(&fixture.scan, hits.items[0]), "MODEL.npy");
        for bad in ["2023-02-29", "1900-02-29", "2024-13-01", "2024-2-01"] {
            assert!(SearchInput {
                from_date: bad.into(),
                ..Default::default()
            }
            .compile(0)
            .is_err());
        }
        assert!(SearchInput {
            from_date: "2000-02-29".into(),
            to_date: "2000-02-29".into(),
            ..Default::default()
        }
        .compile(0)
        .is_ok());
        assert!(SearchInput {
            from_date: "2024-03-01".into(),
            to_date: "2024-02-29".into(),
            ..Default::default()
        }
        .compile(0)
        .is_err());
        let id =
            fixture.scan.file_children[fixture.scan.folder_index(Path::new("Data")).unwrap()][0];
        fixture.scan.files[id].modified = None;
        let hits = search(&fixture.scan, &input.compile(0).unwrap(), || false).unwrap();
        assert!(hits.items.is_empty());
        assert_eq!(hits.unknown_dates, 1);
    }

    #[test]
    fn invalid_inputs_and_cancellation_are_reported_as_errors() {
        let fixture = Fixture::new();
        let input = SearchInput {
            min_size: "10".into(),
            max_size: "2".into(),
            ..Default::default()
        };
        assert_eq!(
            input.compile(0).err().unwrap().kind(),
            io::ErrorKind::InvalidInput
        );
        let input = SearchInput {
            min_size: "-1".into(),
            ..Default::default()
        };
        assert_eq!(
            input.compile(0).err().unwrap().kind(),
            io::ErrorKind::InvalidInput
        );
        let input = SearchInput {
            extensions: "*.npy".into(),
            ..Default::default()
        };
        assert_eq!(
            input.compile(0).err().unwrap().kind(),
            io::ErrorKind::InvalidInput
        );
        let query = SearchInput::default().compile(0).unwrap();
        assert_eq!(
            search(&fixture.scan, &query, || true).err().unwrap().kind(),
            io::ErrorKind::Interrupted
        );
    }

    #[test]
    fn path_kind_extension_and_size_filters_are_combined() {
        let fixture = Fixture::new();
        let input = SearchInput {
            text: "data\\".into(),
            mode: SearchMode::Path,
            kind: EntryKind::Files,
            extensions: "NPY;nii.gz".into(),
            min_size: "4".into(),
            max_size: "8".into(),
            ..Default::default()
        };
        let hits = search(&fixture.scan, &input.compile(0).unwrap(), || false).unwrap();
        assert_eq!(hits.items.len(), 1);
        assert_eq!(item_name(&fixture.scan, hits.items[0]), "model.nii.gz");
    }

    #[test]
    fn name_search_respects_folder_identity_and_recursive_scope() {
        let fixture = Fixture::new();
        let scope = fixture.scan.folder_index(Path::new("Data")).unwrap();
        let input = SearchInput {
            text: "model".into(),
            ..Default::default()
        };
        let mut query = input.compile(scope).unwrap();
        let hits = search(&fixture.scan, &query, || false).unwrap();
        let names: Vec<_> = hits
            .items
            .iter()
            .map(|&item| {
                item_name(&fixture.scan, item)
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        assert_eq!(names, ["model.nii.gz", "MODEL.npy"]);
        query.recursive = false;
        let hits = search(&fixture.scan, &query, || false).unwrap();
        assert_eq!(hits.items.len(), 1);
        assert_eq!(item_name(&fixture.scan, hits.items[0]), "MODEL.npy");
    }
}
