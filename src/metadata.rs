use std::fs::File;
use std::io;
use std::os::windows::{
    fs::{MetadataExt, OpenOptionsExt},
    io::AsRawHandle,
};
use std::path::Path;

const FILE_READ_ATTRIBUTES: u32 = 0x80;
const SHARE_READ_WRITE_DELETE: u32 = 7;
const OPEN_REPARSE_WITH_BACKUP_SEMANTICS: u32 = 0x02200000;
const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
const COMPRESSED_OR_SPARSE: u32 = 0xA00;
const FILE_STANDARD_INFO: i32 = 1;
const FILE_COMPRESSION_INFO: i32 = 8;
const FILE_ID_INFO: i32 = 18;

#[derive(Clone, Copy)]
pub struct ScanSettings {
    pub parallelism: crate::scanner::Parallelism,
    pub method: crate::engine::Method,
    pub detailed: bool,
    pub fast_mode: crate::engine::FastMode,
}
impl Default for ScanSettings {
    fn default() -> Self {
        Self {
            parallelism: Default::default(),
            detailed: true,
            method: Default::default(),
            fast_mode: Default::default(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Missing {
    NotCollected,
    Unsupported,
    Failed,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Field<T> {
    Value(T),
    Missing(Missing),
}
impl<T> Default for Field<T> {
    fn default() -> Self {
        Self::Missing(Missing::NotCollected)
    }
}
impl<T: Copy> Field<T> {
    pub fn value(self) -> Option<T> {
        match self {
            Self::Value(v) => Some(v),
            Self::Missing(_) => None,
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct Identity {
    pub volume: u64,
    pub file: [u8; 16],
}
impl std::fmt::Display for Identity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:016X}:", self.volume)?;
        for byte in self.file {
            write!(f, "{byte:02X}")?;
        }
        Ok(())
    }
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Extra {
    pub allocation: Field<u64>,
    pub identity: Field<Identity>,
    pub links: Field<u32>,
}
pub struct Report {
    pub values: Extra,
    pub issue: Option<io::Error>,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct Allocation {
    pub bytes: u64,
    pub known: u64,
    pub not_collected: u64,
    pub unsupported: u64,
    pub failed: u64,
}
impl Allocation {
    pub fn missing(&self) -> u64 {
        self.not_collected + self.unsupported + self.failed
    }
    pub fn add(&mut self, value: Field<u64>) -> io::Result<()> {
        match value {
            Field::Value(bytes) => {
                self.bytes = self
                    .bytes
                    .checked_add(bytes)
                    .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "할당 합계 초과"))?;
                self.known += 1;
            }
            Field::Missing(Missing::NotCollected) => self.not_collected += 1,
            Field::Missing(Missing::Unsupported) => self.unsupported += 1,
            Field::Missing(Missing::Failed) => self.failed += 1,
        }
        Ok(())
    }
    pub fn merge(&mut self, other: Self) -> io::Result<()> {
        self.bytes = self
            .bytes
            .checked_add(other.bytes)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "할당 합계 초과"))?;
        self.known += other.known;
        self.not_collected += other.not_collected;
        self.unsupported += other.unsupported;
        self.failed += other.failed;
        Ok(())
    }
}
fn missing(error: &io::Error) -> Missing {
    match error.raw_os_error() {
        Some(1 | 50 | 87 | 120 | 124) => Missing::Unsupported,
        _ => Missing::Failed,
    }
}
impl Report {
    pub fn unavailable(error: io::Error) -> Self {
        let state = missing(&error);
        Self {
            values: Extra {
                allocation: Field::Missing(state),
                identity: Field::Missing(state),
                links: Field::Missing(state),
            },
            issue: Some(error),
        }
    }
}

#[repr(C)]
struct Standard {
    allocation: i64,
    eof: i64,
    links: u32,
    delete_pending: u8,
    directory: u8,
}
#[repr(C)]
struct Compression {
    bytes: i64,
    format: u16,
    unit: u8,
    chunk: u8,
    cluster: u8,
    reserved: [u8; 3],
}
#[repr(C)]
struct FileId {
    volume: u64,
    id: [u8; 16],
}
#[link(name = "kernel32")]
extern "system" {
    fn GetFileInformationByHandleEx(
        handle: *mut std::ffi::c_void,
        class: i32,
        buffer: *mut std::ffi::c_void,
        size: u32,
    ) -> i32;
}

fn query<T>(file: &File, class: i32) -> io::Result<T> {
    let mut value = std::mem::MaybeUninit::<T>::zeroed();
    // 고정 정수/바이트 배열로만 구성된 세 Win32 구조체에 한정한 버퍼. 성공 확인 뒤 초기화된 값 사용.
    let success = unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle(),
            class,
            value.as_mut_ptr().cast(),
            std::mem::size_of::<T>() as u32,
        )
    };
    if success == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(unsafe { value.assume_init() })
    }
}
fn field<T>(result: io::Result<T>, issue: &mut Option<io::Error>) -> Field<T> {
    match result {
        Ok(value) => Field::Value(value),
        Err(error) => {
            let state = missing(&error);
            if issue.is_none() {
                *issue = Some(error);
            }
            Field::Missing(state)
        }
    }
}
fn nonnegative(bytes: i64) -> io::Result<u64> {
    u64::try_from(bytes).map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "음수 할당 크기"))
}

/// 열기·속성·standard·ID·compression·닫기 순서의 개발용 측정값.
#[derive(Default)]
pub struct ReadMetrics<'a> {
    pub nanoseconds: [u64; 6],
    pub calls: [u64; 6],
    pub trace: Option<&'a crate::scanner::profile::Trace>,
}
fn measure<T>(metrics: &mut Option<&mut ReadMetrics>, stage: usize, run: impl FnOnce() -> T) -> T {
    if let Some(metrics) = metrics {
        let start = std::time::Instant::now();
        let value = run();
        metrics.nanoseconds[stage] += start.elapsed().as_nanos() as u64;
        metrics.calls[stage] += 1;
        value
    } else {
        run()
    }
}

/// 기본 데이터 스트림의 메타데이터 조회. 파일 내용·ADS·디렉터리 인덱스는 조회 대상에서 제외.
pub fn read(path: &Path) -> Report {
    read_with_metrics(path, None)
}
/// 선택한 경우에만 단계별 비용 측정. 일반 수집에는 시계 조회 생략.
pub fn read_with_metrics(path: &Path, mut metrics: Option<&mut ReadMetrics>) -> Report {
    let file = match measure(&mut metrics, 0, || {
        File::options()
            .access_mode(FILE_READ_ATTRIBUTES)
            .share_mode(SHARE_READ_WRITE_DELETE)
            .custom_flags(OPEN_REPARSE_WITH_BACKUP_SEMANTICS)
            .open(path)
    }) {
        Ok(file) => file,
        Err(e) => return Report::unavailable(e),
    };
    let report = read_file_inner(&file, &mut metrics);
    measure(&mut metrics, 5, || drop(file));
    report
}
/// 동일 핸들에서 기본 스트림의 할당량·ID·링크 수 조회.
pub fn read_file(file: &File) -> Report {
    read_file_inner(file, &mut None)
}

/// FileStatBasicByNameInfo의 공개 Windows ABI 구조체. 모든 ID는 원래 폭 유지.
#[repr(C)]
#[derive(Default)]
pub struct Stat {
    pub file_id: i64,
    pub created: i64,
    accessed: i64,
    pub modified: i64,
    changed: i64,
    pub allocation: i64,
    pub eof: i64,
    pub attributes: u32,
    reparse: u32,
    pub links: u32,
    device_type: u32,
    device_characteristics: u32,
    reserved: u32,
    pub volume: u64,
    pub id: [u8; 16],
}
/// 통합 조회의 OS 경계. None은 함수/정보 클래스 미지원만 의미.
pub trait StatSource {
    fn stat(&self, path: &Path) -> io::Result<Option<Stat>>;
    fn stat_profiled(
        &self,
        path: &Path,
        _trace: Option<&crate::scanner::profile::Trace>,
    ) -> io::Result<Option<Stat>> {
        self.stat(path)
    }
}
type StatFn = unsafe extern "system" fn(*const u16, i32, *mut std::ffi::c_void, u32) -> i32;
#[link(name = "kernel32")]
extern "system" {
    fn LoadLibraryExW(
        name: *const u16,
        file: *mut std::ffi::c_void,
        flags: u32,
    ) -> *mut std::ffi::c_void;
    fn GetProcAddress(module: *mut std::ffi::c_void, name: *const u8) -> *mut std::ffi::c_void;
    fn FreeLibrary(module: *mut std::ffi::c_void) -> i32;
}
fn stat_function() -> Option<StatFn> {
    static FUNCTION: std::sync::OnceLock<Option<StatFn>> = std::sync::OnceLock::new();
    *FUNCTION.get_or_init(|| {
        let name: Vec<u16> = "api-ms-win-core-file-l2-1-4.dll"
            .encode_utf16()
            .chain(Some(0))
            .collect();
        // 시스템 API-set만 로드. 찾은 함수의 수명을 위해 성공 모듈은 프로세스 종료까지 유지.
        unsafe {
            let module = LoadLibraryExW(name.as_ptr(), std::ptr::null_mut(), 0x800);
            if module.is_null() {
                return None;
            }
            let address = GetProcAddress(module, c"GetFileInformationByName".as_ptr().cast());
            if address.is_null() {
                FreeLibrary(module);
                None
            } else {
                Some(std::mem::transmute::<*mut std::ffi::c_void, StatFn>(
                    address,
                ))
            }
        }
    })
}
pub struct Named {
    enabled: bool,
    root: std::path::PathBuf,
    wide_root: Vec<u16>,
}
thread_local! {static WIDE_PATH:std::cell::RefCell<Vec<u16>>=const{std::cell::RefCell::new(Vec::new())};}
impl Named {
    fn absolute_wide(path: &Path) -> io::Result<Vec<u16>> {
        use std::os::windows::ffi::OsStrExt;
        let absolute = std::path::absolute(path)?;
        let mut units: Vec<u16> = absolute.as_os_str().encode_wide().collect();
        if units.contains(&0) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "경로에 NUL이 포함되어 있습니다",
            ));
        }
        if !units.starts_with(&[92, 92, 63, 92]) {
            let mut prefix: Vec<u16> = if units.starts_with(&[92, 92]) {
                "\\\\?\\UNC\\".encode_utf16().collect()
            } else {
                "\\\\?\\".encode_utf16().collect()
            };
            prefix.extend_from_slice(if units.starts_with(&[92, 92]) {
                &units[2..]
            } else {
                &units
            });
            units = prefix;
        }
        Ok(units)
    }
    fn prepare(&self, path: &Path, buffer: &mut Vec<u16>) -> io::Result<()> {
        use std::{os::windows::ffi::OsStrExt, path::Component};
        buffer.clear();
        if self.root.is_absolute() {
            if let Ok(relative) = path.strip_prefix(&self.root) {
                if relative
                    .components()
                    .all(|c| matches!(c, Component::Normal(_) | Component::CurDir))
                {
                    buffer.extend_from_slice(&self.wide_root);
                    for component in relative.components() {
                        if let Component::Normal(name) = component {
                            if buffer.last() != Some(&92) {
                                buffer.push(92);
                            }
                            buffer.extend(name.encode_wide());
                        }
                    }
                    if buffer.contains(&0) {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidInput,
                            "경로에 NUL이 포함되어 있습니다",
                        ));
                    }
                    buffer.push(0);
                    return Ok(());
                }
            }
        }
        // 상대 경로·상위 이동·범위 밖 요청은 기존 절대 경로 해석 유지.
        buffer.extend(Self::absolute_wide(path)?);
        buffer.push(0);
        Ok(())
    }
    /// 루트에서 정보 클래스 지원을 한 번 확인. 미지원 시 파일별 반복 실패 방지.
    pub fn for_root(root: &Path) -> io::Result<Self> {
        let mut source = Self {
            enabled: stat_function().is_some(),
            root: root.to_owned(),
            wide_root: Vec::new(),
        };
        if source.enabled && root.is_absolute() {
            source.wide_root = Self::absolute_wide(root)?;
        }
        if source.enabled {
            source.enabled = if let Some(stat) = source.stat(root)? {
                let file = File::options()
                    .access_mode(FILE_READ_ATTRIBUTES)
                    .share_mode(SHARE_READ_WRITE_DELETE)
                    .custom_flags(OPEN_REPARSE_WITH_BACKUP_SEMANTICS)
                    .open(root)?;
                match query::<FileId>(&file, FILE_ID_INFO) {
                    Ok(id) => stat.volume == id.volume && stat.id == id.id,
                    Err(e) if missing(&e) == Missing::Unsupported => false,
                    Err(e) => return Err(e),
                }
            } else {
                false
            };
        }
        Ok(source)
    }
}
impl StatSource for Named {
    fn stat(&self, path: &Path) -> io::Result<Option<Stat>> {
        self.stat_profiled(path, None)
    }
    fn stat_profiled(
        &self,
        path: &Path,
        trace: Option<&crate::scanner::profile::Trace>,
    ) -> io::Result<Option<Stat>> {
        use crate::scanner::profile::{scope, Stage};
        if !self.enabled {
            return Ok(None);
        }
        let function = stat_function().expect("지원 확인한 통합 조회");
        // 한 worker의 동기 조회 동안만 빌린 버퍼. 다음 요청에는 길이를 비워 이전 파일명 잔존 방지.
        WIDE_PATH.with(|storage| {
            let mut path_buffer = storage.borrow_mut();
            let preparation = scope(trace, Stage::PathPrepare);
            self.prepare(path, &mut path_buffer)?;
            drop(preparation);
            let mut stat = Stat::default();
            let _query = scope(trace, Stage::WindowsQuery);
            // 고정 ABI 구조체·NUL 종료 UTF-16 경로·해석한 공식 정보 클래스 3에 한정한 호출.
            if unsafe {
                function(
                    path_buffer.as_ptr(),
                    3,
                    (&mut stat as *mut Stat).cast(),
                    std::mem::size_of::<Stat>() as u32,
                )
            } == 0
            {
                let e = io::Error::last_os_error();
                if matches!(e.raw_os_error(), Some(1 | 50 | 87 | 120 | 124)) {
                    Ok(None)
                } else {
                    Err(e)
                }
            } else {
                Ok(Some(stat))
            }
        })
    }
}
/// 지원 환경에서는 파일 열기 없이 상세 필드 통합 조회. 압축/Sparse는 기존 핸들 경로 유지.
pub fn read_combined(
    path: &Path,
    source: &impl StatSource,
    mut metrics: Option<&mut ReadMetrics>,
) -> Report {
    let trace = metrics.as_ref().and_then(|m| m.trace);
    let stat = match measure(&mut metrics, 1, || source.stat_profiled(path, trace)) {
        Ok(Some(stat)) => stat,
        Ok(None) => return read_with_metrics(path, metrics),
        Err(e) => return Report::unavailable(e),
    };
    if stat.attributes & 0x410 != 0 {
        return Report::unavailable(io::Error::new(
            io::ErrorKind::InvalidInput,
            "추가 조회 대상의 종류가 변경됐거나 Reparse Point임",
        ));
    }
    if stat.attributes & COMPRESSED_OR_SPARSE != 0 || stat.device_type != 7 {
        return read_with_metrics(path, metrics);
    }
    if stat.eof < 0 {
        return Report::unavailable(io::Error::new(io::ErrorKind::InvalidData, "음수 논리 크기"));
    }
    let allocation = match nonnegative(stat.allocation) {
        Ok(value) => value,
        Err(e) => return Report::unavailable(e),
    };
    Report {
        values: Extra {
            allocation: Field::Value(allocation),
            identity: Field::Value(Identity {
                volume: stat.volume,
                file: stat.id,
            }),
            links: Field::Value(stat.links),
        },
        issue: None,
    }
}
fn read_file_inner(file: &File, metrics: &mut Option<&mut ReadMetrics>) -> Report {
    let metadata = match measure(metrics, 1, || file.metadata()) {
        Ok(m) => m,
        Err(e) => return Report::unavailable(e),
    };
    if !metadata.is_file() || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Report::unavailable(io::Error::new(
            io::ErrorKind::InvalidInput,
            "추가 조회 대상의 종류가 변경됐거나 Reparse Point임",
        ));
    }
    let mut issue = None;
    let standard = measure(metrics, 2, || query::<Standard>(file, FILE_STANDARD_INFO));
    let (links, ordinary) = match standard {
        Ok(s) => (
            Field::Value(s.links),
            field(nonnegative(s.allocation), &mut issue),
        ),
        Err(e) => {
            let state = missing(&e);
            issue = Some(e);
            (Field::Missing(state), Field::Missing(state))
        }
    };
    let allocation = if metadata.file_attributes() & COMPRESSED_OR_SPARSE != 0 {
        field(
            measure(metrics, 4, || {
                query::<Compression>(file, FILE_COMPRESSION_INFO)
            })
            .and_then(|c| nonnegative(c.bytes)),
            &mut issue,
        )
    } else {
        ordinary
    };
    let identity = field(
        measure(metrics, 3, || query::<FileId>(file, FILE_ID_INFO)).map(|id| Identity {
            volume: id.volume,
            file: id.id,
        }),
        &mut issue,
    );
    Report {
        values: Extra {
            allocation,
            identity,
            links,
        },
        issue,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn combined_metadata_rejects_malformed_sizes_and_changed_reparse_targets() {
        struct Invalid {
            size: i64,
            attributes: u32,
        }
        impl StatSource for Invalid {
            fn stat(&self, _: &Path) -> io::Result<Option<Stat>> {
                Ok(Some(Stat {
                    eof: self.size,
                    attributes: self.attributes,
                    allocation: 4096,
                    device_type: 7,
                    ..Default::default()
                }))
            }
        }
        let path = Path::new("not-accessed");
        assert_eq!(
            read_combined(
                path,
                &Invalid {
                    size: -1,
                    attributes: 0
                },
                None
            )
            .issue
            .unwrap()
            .kind(),
            io::ErrorKind::InvalidData
        );
        assert_eq!(
            read_combined(
                path,
                &Invalid {
                    size: 1,
                    attributes: 0x400
                },
                None
            )
            .issue
            .unwrap()
            .kind(),
            io::ErrorKind::InvalidInput
        );
    }
    #[test]
    fn combined_metadata_keeps_identity_links_and_only_falls_back_when_unsupported() {
        struct Denied;
        impl StatSource for Denied {
            fn stat(&self, _: &Path) -> io::Result<Option<Stat>> {
                Err(io::Error::from_raw_os_error(5))
            }
        }
        struct Unsupported;
        impl StatSource for Unsupported {
            fn stat(&self, _: &Path) -> io::Result<Option<Stat>> {
                Ok(None)
            }
        }
        let root = std::env::temp_dir().join(format!("diskweave-combined-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("file");
        std::fs::write(&path, b"12345").unwrap();
        std::fs::hard_link(&path, root.join("alias")).unwrap();
        let old = read(&path);
        assert_eq!(read_combined(&path, &Unsupported, None).values, old.values);
        let denied = read_combined(&path, &Denied, None);
        assert_eq!(denied.issue.unwrap().raw_os_error(), Some(5));
        assert_eq!(denied.values.allocation, Field::Missing(Missing::Failed));
        let native = Named::for_root(&root).unwrap();
        let actual = read_combined(&path, &native, None);
        assert!(actual.issue.is_none());
        assert_eq!(actual.values, old.values);
        assert_eq!(actual.values.links.value(), Some(2));
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn measured_reads_preserve_fields_and_report_open_query_close_failures() {
        let root = std::env::temp_dir().join(format!("diskweave-measured-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("data");
        std::fs::write(&path, b"123").unwrap();
        let expected = read(&path);
        let mut metrics = ReadMetrics::default();
        let actual = read_with_metrics(&path, Some(&mut metrics));
        assert_eq!(actual.values, expected.values);
        assert!(actual.issue.is_none());
        assert_eq!(metrics.calls, [1, 1, 1, 1, 0, 1]);
        let mut failed = ReadMetrics::default();
        assert!(read_with_metrics(&root.join("missing"), Some(&mut failed))
            .issue
            .is_some());
        assert_eq!(failed.calls, [1, 0, 0, 0, 0, 0]);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    #[ignore = "임시 NTFS 파일만 Sparse/압축 처리하는 별도 검증"]
    fn ntfs_sparse_compression_and_ads_are_measured_without_reading_file_contents() {
        use std::io::{Seek, SeekFrom, Write};
        use std::os::windows::ffi::OsStrExt;
        #[link(name = "kernel32")]
        extern "system" {
            fn DeviceIoControl(
                handle: *mut std::ffi::c_void,
                code: u32,
                input: *const std::ffi::c_void,
                input_size: u32,
                output: *mut std::ffi::c_void,
                output_size: u32,
                returned: *mut u32,
                overlapped: *mut std::ffi::c_void,
            ) -> i32;
            fn GetCompressedFileSizeW(path: *const u16, high: *mut u32) -> u32;
        }
        fn control(file: &File, code: u32, input: Option<&u16>) {
            let mut returned = 0;
            let (data, size) = input.map_or((std::ptr::null(), 0), |p| {
                (p as *const u16 as *const std::ffi::c_void, 2)
            });
            // 시험용 파일 핸들에 한정한 FSCTL 변경. 모든 입력/출력 버퍼는 호출 동안 유지.
            assert_ne!(
                unsafe {
                    DeviceIoControl(
                        file.as_raw_handle(),
                        code,
                        data,
                        size,
                        std::ptr::null_mut(),
                        0,
                        &mut returned,
                        std::ptr::null_mut(),
                    )
                },
                0,
                "{}",
                io::Error::last_os_error()
            );
        }
        let root =
            std::env::temp_dir().join(format!("diskweave-special-metadata-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let sparse = root.join("sparse.bin");
        let mut f = File::options()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&sparse)
            .unwrap();
        control(&f, 0x900c4, None);
        f.set_len(64 * 1024 * 1024).unwrap();
        f.write_all(&[7; 4096]).unwrap();
        f.sync_all().unwrap();
        drop(f);
        let compressed = root.join("compressed.bin");
        let mut f = File::options()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&compressed)
            .unwrap();
        control(&f, 0x9c040, Some(&2));
        f.write_all(&vec![0; 1024 * 1024]).unwrap();
        f.seek(SeekFrom::Start(0)).unwrap();
        f.sync_all().unwrap();
        drop(f);
        for path in [&sparse, &compressed] {
            let report = read(path);
            let named = Named::for_root(&root).unwrap();
            assert_eq!(read_combined(path, &named, None).values, report.values);
            assert!(report.issue.is_none(), "{:?}", report.issue);
            let allocated = report.values.allocation.value().unwrap();
            assert!(allocated < std::fs::metadata(path).unwrap().len());
            let name: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
            let mut high = 0;
            // 별도 경로 기반 OS 조회와 핸들 기반 수집 결과 대조. 시험 파일 크기는 4 GiB 미만.
            let low = unsafe { GetCompressedFileSizeW(name.as_ptr(), &mut high) };
            assert_ne!(low, u32::MAX);
            assert_eq!(allocated, ((high as u64) << 32) | low as u64);
        }
        for strategy in [
            crate::scanner::directory::Strategy::LargeFetch,
            crate::scanner::directory::Strategy::Bulk,
            crate::scanner::directory::Strategy::Combined,
            crate::scanner::directory::Strategy::Experimental(
                crate::scanner::directory::Experiment {
                    shared_handle: true,
                    reuse_buffer: true,
                    ..Default::default()
                },
            ),
            crate::scanner::directory::Strategy::Experimental(
                crate::scanner::directory::Experiment {
                    shared_handle: true,
                    reuse_buffer: true,
                    buffer_bytes: 4194304,
                },
            ),
        ] {
            let scan = crate::scanner::scan_with_strategy(
                &root,
                true,
                crate::scanner::Parallelism::Four,
                strategy,
                || false,
                |_| {},
                None,
            )
            .unwrap();
            for entry in &scan.files {
                let expected = read(&root.join(&entry.name));
                assert_eq!(entry.extra, expected.values);
            }
        }
        std::fs::write(root.join("sparse.bin:extra"), vec![1; 16384]).unwrap();
        let scan = crate::scanner::scan_with_options(&root, true, || false, |_| {}).unwrap();
        assert_eq!(scan.files.len(), 2);
        assert_eq!(scan.folders[0].logical_bytes, 65 * 1024 * 1024);
        assert_eq!(
            scan.files
                .iter()
                .find(|f| f.name == "sparse.bin")
                .unwrap()
                .extra
                .allocation
                .value(),
            read(&sparse).values.allocation.value()
        );
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn metadata_identifies_hard_links_and_keeps_allocation_separate_from_length() {
        let root = std::env::temp_dir().join(format!("diskweave-metadata-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("한글 파일.bin");
        std::fs::write(&path, vec![7; 8193]).unwrap();
        std::fs::hard_link(&path, root.join("alias.bin")).unwrap();
        std::fs::write(root.join("other.bin"), vec![7; 8193]).unwrap();
        let report = read(&path);
        assert!(report.issue.is_none(), "{:?}", report.issue);
        let id = report.values.identity.value().unwrap();
        assert_eq!(
            read(&root.join("alias.bin")).values.identity.value(),
            Some(id)
        );
        assert_ne!(
            read(&root.join("other.bin")).values.identity.value(),
            Some(id)
        );
        assert_eq!(report.values.links.value(), Some(2));
        assert!(report.values.allocation.value().unwrap() >= 8193);
        assert!(matches!(
            Extra::default().allocation,
            Field::Missing(Missing::NotCollected)
        ));
        let failed = read(&root.join("missing"));
        assert!(matches!(
            failed.values.allocation,
            Field::Missing(Missing::Failed)
        ));
        assert!(failed.issue.is_some());
        std::fs::remove_dir_all(root).unwrap();
    }
}
