//! 상세 조회 후보의 실제 OS 호출·대체 경로와 경로 결합 검증.

use crate::{
    metadata::{self, Identity, Stat, StatSource},
    scanner::{MetadataProbe, MetadataRequest},
};
use std::{
    cell::RefCell,
    ffi::OsStr,
    fs::File,
    io,
    os::windows::{
        ffi::OsStrExt,
        fs::{MetadataExt, OpenOptionsExt},
        io::AsRawHandle,
    },
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
};
use windows_sys::Win32::Storage::FileSystem::{GetFileInformationByHandleEx, FILE_ID_INFO};
#[derive(Default)]
pub struct Counts {
    pub native_calls: AtomicU64,
    pub native_success: AtomicU64,
    pub fallback: AtomicU64,
    pub parents: AtomicU64,
    pub binding_queries: AtomicU64,
    pub compressed: AtomicU64,
}
impl Counts {
    pub fn json(&self) -> serde_json::Value {
        serde_json::json!({"native_calls":self.native_calls.load(Ordering::Relaxed),"native_success":self.native_success.load(Ordering::Relaxed),"fallback_files":self.fallback.load(Ordering::Relaxed),"parent_opens":self.parents.load(Ordering::Relaxed),"binding_queries":self.binding_queries.load(Ordering::Relaxed),"compression_fallbacks":self.compressed.load(Ordering::Relaxed)})
    }
}
#[repr(C)]
struct Unicode {
    length: u16,
    maximum: u16,
    buffer: *mut u16,
}
#[repr(C)]
struct Objects {
    length: u32,
    root: *mut std::ffi::c_void,
    name: *mut Unicode,
    flags: u32,
    security: *mut std::ffi::c_void,
    quality: *mut std::ffi::c_void,
}
#[repr(C)]
struct Status {
    status: usize,
    information: usize,
}
type Query =
    unsafe extern "system" fn(*mut Objects, *mut Status, *mut std::ffi::c_void, u32, i32) -> i32;
#[link(name = "kernel32")]
extern "system" {
    fn GetModuleHandleW(name: *const u16) -> *mut std::ffi::c_void;
    fn GetProcAddress(module: *mut std::ffi::c_void, name: *const u8) -> *mut std::ffi::c_void;
}
#[link(name = "ntdll")]
extern "system" {
    fn RtlNtStatusToDosError(status: i32) -> u32;
}
pub fn abi_sizes() -> [usize; 4] {
    [
        std::mem::size_of::<Unicode>(),
        std::mem::size_of::<Objects>(),
        std::mem::size_of::<Status>(),
        std::mem::size_of::<Stat>(),
    ]
}
fn query_function() -> Option<Query> {
    static FUNCTION: std::sync::OnceLock<Option<Query>> = std::sync::OnceLock::new();
    *FUNCTION.get_or_init(|| {
        let module: Vec<_> = "ntdll.dll".encode_utf16().chain(Some(0)).collect();
        // 이미 로드된 시스템 ntdll의 수명에 한정한 동적 함수 주소.
        unsafe {
            let module = GetModuleHandleW(module.as_ptr());
            if module.is_null() {
                return None;
            }
            let address = GetProcAddress(module, c"NtQueryInformationByName".as_ptr().cast());
            if address.is_null() {
                None
            } else {
                Some(std::mem::transmute::<*mut std::ffi::c_void, Query>(address))
            }
        }
    })
}
fn directory(path: &Path) -> io::Result<File> {
    let file = File::options()
        .access_mode(0xa0)
        .share_mode(7)
        .custom_flags(0x2200000)
        .open(path)?;
    let m = file.metadata()?;
    if !m.is_dir() || m.file_attributes() & 0x400 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "조회할 부모가 폴더가 아니거나 Reparse Point입니다",
        ));
    }
    Ok(file)
}
fn identity(file: &File) -> io::Result<Identity> {
    let mut info: FILE_ID_INFO = unsafe { std::mem::zeroed() };
    // SDK의 고정 정수 구조체와 실제 핸들에서 full128/볼륨을 독립 조회.
    if unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle(),
            18,
            (&mut info as *mut FILE_ID_INFO).cast(),
            std::mem::size_of_val(&info) as u32,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(Identity {
        volume: info.VolumeSerialNumber,
        file: info.FileId.Identifier,
    })
}
fn nt_stat(function: Query, parent: &File, name: &OsStr) -> io::Result<Option<Stat>> {
    let mut text: Vec<_> = name.encode_wide().collect();
    let bytes = text
        .len()
        .checked_mul(2)
        .and_then(|v| u16::try_from(v).ok())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "NT 상대 이름의 길이 초과"))?;
    if text.contains(&0) || text.contains(&92) || text.contains(&47) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "NT 상대 조회는 단일 파일명만 허용합니다",
        ));
    }
    let mut name = Unicode {
        length: bytes,
        maximum: bytes,
        buffer: text.as_mut_ptr(),
    };
    let mut attributes = Objects {
        length: std::mem::size_of::<Objects>() as u32,
        root: parent.as_raw_handle(),
        name: &mut name,
        flags: 0,
        security: std::ptr::null_mut(),
        quality: std::ptr::null_mut(),
    };
    let mut status = Status {
        status: 0,
        information: 0,
    };
    let mut result = Stat::default();
    // x64 ABI의 실제 생존 버퍼와 공개 정보 클래스 77만 전달. NTSTATUS를 Win32 오류로 변환.
    let code = unsafe {
        function(
            &mut attributes,
            &mut status,
            (&mut result as *mut Stat).cast(),
            std::mem::size_of::<Stat>() as u32,
            77,
        )
    };
    if code < 0 {
        if [0xc0000002u32, 0xc0000003, 0xc000000d, 0xc00000bb].contains(&(code as u32)) {
            return Ok(None);
        }
        return Err(io::Error::from_raw_os_error(
            unsafe { RtlNtStatusToDosError(code) } as i32,
        ));
    }
    if status.information != 0 && status.information != std::mem::size_of::<Stat>() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "NT 상세 응답 길이 오류",
        ));
    }
    Ok(Some(result))
}
struct Parent {
    path: PathBuf,
    root: Identity,
    file: File,
    id: Identity,
}
thread_local! {static PARENT:RefCell<Option<Parent>>=const {RefCell::new(None)};}
pub struct Relative {
    root: PathBuf,
    id: Identity,
    function: Option<Query>,
    original: metadata::Named,
    pub counts: Counts,
}
impl Relative {
    pub fn open(root: &Path) -> io::Result<Self> {
        let root = std::path::absolute(root)?;
        let file = directory(&root)?;
        let id = identity(&file)?;
        let mut function = query_function();
        if let (Some(query), Some(parent), Some(name)) = (function, root.parent(), root.file_name())
        {
            let parent = directory(parent)?;
            match nt_stat(query, &parent, name)? {
                Some(stat) if stat.volume == id.volume && stat.id == id.file => {}
                Some(_) => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "NT 조회 ABI/루트 ID 불일치",
                    ))
                }
                None => function = None,
            }
        }
        Ok(Self {
            original: metadata::Named::for_root(&root)?,
            root,
            id,
            function,
            counts: Default::default(),
        })
    }
    pub fn available(&self) -> bool {
        self.function.is_some()
    }
    pub fn check_root(&self) -> io::Result<()> {
        if identity(&directory(&self.root)?)? != self.id {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "조회 중 분석 루트 변경",
            ));
        }
        Ok(())
    }
}
struct RelativeStat<'a> {
    source: &'a Relative,
    parent: &'a File,
}
impl StatSource for RelativeStat<'_> {
    fn stat(&self, path: &Path) -> io::Result<Option<Stat>> {
        let name = path
            .file_name()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "파일명 누락"))?;
        self.source
            .counts
            .native_calls
            .fetch_add(1, Ordering::Relaxed);
        match nt_stat(self.source.function.unwrap(), self.parent, name)? {
            Some(stat) => {
                self.source
                    .counts
                    .native_success
                    .fetch_add(1, Ordering::Relaxed);
                if stat.attributes & 0xa00 != 0 {
                    self.source
                        .counts
                        .compressed
                        .fetch_add(1, Ordering::Relaxed);
                }
                Ok(Some(stat))
            }
            None => {
                self.source.counts.fallback.fetch_add(1, Ordering::Relaxed);
                self.source.original.stat(path)
            }
        }
    }
}
fn duplicate(error: &io::Error) -> io::Error {
    error
        .raw_os_error()
        .map(io::Error::from_raw_os_error)
        .unwrap_or_else(|| io::Error::new(error.kind(), error.to_string()))
}
fn in_scope(root: &Path, path: &Path) -> bool {
    path.strip_prefix(root).is_ok_and(|p| {
        let mut parts = p.components().peekable();
        parts.peek().is_some() && parts.all(|p| matches!(p, std::path::Component::Normal(_)))
    })
}
impl MetadataProbe for Relative {
    fn read_batch(&self, requests: &[MetadataRequest], stop: &AtomicBool) -> Vec<metadata::Report> {
        let mut output = Vec::with_capacity(requests.len());
        for group in requests.chunk_by(|a, b| a.path.parent() == b.path.parent()) {
            if stop.load(Ordering::Relaxed) {
                break;
            }
            if group.iter().any(|r| !in_scope(&self.root, &r.path)) {
                output.extend(group.iter().map(|_| {
                    metadata::Report::unavailable(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "분석 루트 밖의 조회",
                    ))
                }));
                continue;
            }
            if !self.available() {
                for item in group {
                    if stop.load(Ordering::Relaxed) {
                        break;
                    }
                    self.counts.fallback.fetch_add(1, Ordering::Relaxed);
                    output.push(metadata::read_combined(&item.path, &self.original, None));
                }
                continue;
            }
            let result = PARENT.with(|storage| -> io::Result<Vec<metadata::Report>> {
                let mut cache = storage.borrow_mut();
                let parent = group[0]
                    .path
                    .parent()
                    .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "부모 경로 누락"))?;
                if cache
                    .as_ref()
                    .is_none_or(|p| p.path != parent || p.root != self.id)
                {
                    let file = directory(parent)?;
                    let id = identity(&file)?;
                    *cache = Some(Parent {
                        path: parent.to_owned(),
                        root: self.id,
                        file,
                        id,
                    });
                    self.counts.parents.fetch_add(1, Ordering::Relaxed);
                }
                let cached = cache.as_ref().unwrap();
                let source = RelativeStat {
                    source: self,
                    parent: &cached.file,
                };
                let reports: Vec<_> = group
                    .iter()
                    .take_while(|_| !stop.load(Ordering::Relaxed))
                    .map(|r| metadata::read_combined(&r.path, &source, None))
                    .collect();
                if !stop.load(Ordering::Relaxed) {
                    self.counts.binding_queries.fetch_add(1, Ordering::Relaxed);
                    if identity(&directory(parent)?)? != cached.id {
                        cache.take();
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "상대 조회 중 부모 경로가 다른 폴더로 변경됐습니다",
                        ));
                    }
                }
                Ok(reports)
            });
            match result {
                Ok(mut values) => output.append(&mut values),
                Err(error) => output.extend(
                    group
                        .iter()
                        .map(|_| metadata::Report::unavailable(duplicate(&error))),
                ),
            }
        }
        output
    }
}

pub struct ById {
    root: PathBuf,
    hint: File,
    id: Identity,
    original: metadata::Named,
    pub counts: Counts,
}
impl ById {
    pub fn open(root: &Path) -> io::Result<Self> {
        let root = std::path::absolute(root)?;
        crate::ntfs_native::target(
            root.ancestors()
                .last()
                .ok_or_else(|| io::Error::other("드라이브 루트 누락"))?,
        )?;
        let hint = directory(&root)?;
        let id = identity(&hint)?;
        Ok(Self {
            original: metadata::Named::for_root(&root)?,
            root,
            hint,
            id,
            counts: Default::default(),
        })
    }
    pub fn check_root(&self) -> io::Result<()> {
        if identity(&directory(&self.root)?)? != self.id {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "조회 중 분석 루트 변경",
            ));
        }
        Ok(())
    }
    fn read(&self, request: &MetadataRequest) -> metadata::Report {
        use std::os::windows::io::FromRawHandle;
        use windows_sys::Win32::{
            Foundation::INVALID_HANDLE_VALUE,
            Storage::FileSystem::{OpenFileById, FILE_ID_DESCRIPTOR},
        };
        let Some(expected) = request.identity else {
            self.counts.fallback.fetch_add(1, Ordering::Relaxed);
            return metadata::read_combined(&request.path, &self.original, None);
        };
        if expected.volume != self.id.volume || expected.file[8..].iter().any(|b| *b != 0) {
            return metadata::Report::unavailable(io::Error::new(
                io::ErrorKind::Unsupported,
                "시험 ID 조회는 NTFS full128의 원래 64-bit 참조 형식에 한정됩니다",
            ));
        }
        let mut descriptor = FILE_ID_DESCRIPTOR {
            dwSize: std::mem::size_of::<FILE_ID_DESCRIPTOR>() as u32,
            ..Default::default()
        };
        descriptor.Anonymous.FileId = i64::from_le_bytes(expected.file[..8].try_into().unwrap());
        self.counts.native_calls.fetch_add(1, Ordering::Relaxed);
        // 열거 ID를 변형하지 않고 같은 볼륨 hint와 읽기 속성 권한만 전달.
        let handle = unsafe {
            OpenFileById(
                self.hint.as_raw_handle(),
                &descriptor,
                0x80,
                7,
                std::ptr::null(),
                0x2200000,
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return metadata::Report::unavailable(io::Error::last_os_error());
        }
        let file = unsafe { File::from_raw_handle(handle) };
        let report = metadata::read_file(&file);
        if report.issue.is_some() {
            return report;
        }
        if report.values.identity.value() != Some(expected) {
            return metadata::Report::unavailable(io::Error::new(
                io::ErrorKind::InvalidData,
                "ID로 연 파일의 전체 ID가 열거 값과 다릅니다",
            ));
        }
        self.counts.binding_queries.fetch_add(1, Ordering::Relaxed);
        let current = match self.original.stat(&request.path) {
            Ok(Some(stat)) => Some(Identity {
                volume: stat.volume,
                file: stat.id,
            }),
            Ok(None) => {
                self.counts.fallback.fetch_add(1, Ordering::Relaxed);
                let report = metadata::read(&request.path);
                if let Some(error) = report.issue {
                    return metadata::Report::unavailable(error);
                }
                report.values.identity.value()
            }
            Err(error) => return metadata::Report::unavailable(error),
        };
        if current != Some(expected) {
            return metadata::Report::unavailable(io::Error::new(
                io::ErrorKind::InvalidData,
                "열거 후 파일 경로가 다른 객체로 교체됐습니다",
            ));
        }
        self.counts.native_success.fetch_add(1, Ordering::Relaxed);
        report
    }
}
impl MetadataProbe for ById {
    fn read_batch(&self, requests: &[MetadataRequest], stop: &AtomicBool) -> Vec<metadata::Report> {
        requests
            .iter()
            .take_while(|_| !stop.load(Ordering::Relaxed))
            .map(|request| {
                if !in_scope(&self.root, &request.path) {
                    return metadata::Report::unavailable(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "분석 루트 밖의 ID 조회",
                    ));
                }
                self.read(request)
            })
            .collect()
    }
}

pub fn profile(args: &[std::ffi::OsString], check: bool) -> io::Result<()> {
    if !(3..=4).contains(&args.len()) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "<폴더> <basic|detailed> <relative|by-id|control> [제한 초] 형식 필요",
        ));
    }
    let detailed = match args[1].to_str() {
        Some("basic") => false,
        Some("detailed") => true,
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "수집 방식 오류",
            ))
        }
    };
    let method = args[2]
        .to_str()
        .filter(|s| ["relative", "by-id", "control"].contains(s))
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "조회 후보 오류"))?;
    let limit = match args.get(3) {
        None => 180.0,
        Some(v) => v
            .to_str()
            .and_then(|s| s.parse::<f64>().ok())
            .filter(|v| v.is_finite() && *v > 0.0 && *v <= 600.0)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "제한 시간 오류"))?,
    };
    let root = std::path::absolute(&args[0])?;
    // 비교 전 기준 스캔 두 번으로 Hard Link 이름 캐시의 자체 갱신을 분리.
    let expected = if check {
        drop(crate::scanner::scan_with_strategy(
            &root,
            detailed,
            crate::scanner::Parallelism::Eight,
            crate::scanner::directory::Strategy::Combined,
            || false,
            |_| {},
            None,
        )?);
        Some(crate::scanner::scan_with_strategy(
            &root,
            detailed,
            crate::scanner::Parallelism::Eight,
            crate::scanner::directory::Strategy::Combined,
            || false,
            |_| {},
            None,
        )?)
    } else {
        None
    };
    let start = std::time::Instant::now();
    let cancelled = || start.elapsed().as_secs_f64() > limit;
    let result = (|| {
        if method == "control" {
            Ok((
                crate::scanner::scan_with_strategy(
                    &root,
                    detailed,
                    crate::scanner::Parallelism::Eight,
                    crate::scanner::directory::Strategy::Combined,
                    cancelled,
                    |_| {},
                    None,
                )?,
                serde_json::Value::Null,
                true,
            ))
        } else if method == "relative" {
            let probe = Relative::open(&root)?;
            let scan = crate::scanner::scan_with_metadata_source(
                &root,
                detailed,
                crate::scanner::Parallelism::Eight,
                crate::scanner::directory::Strategy::LargeFetch,
                cancelled,
                |_| {},
                &probe,
            )?;
            probe.check_root()?;
            Ok((scan, probe.counts.json(), probe.available()))
        } else {
            let probe = ById::open(&root)?;
            let scan = crate::scanner::scan_with_metadata_source(
                &root,
                detailed,
                crate::scanner::Parallelism::Eight,
                crate::scanner::directory::Strategy::Bulk,
                cancelled,
                |_| {},
                &probe,
            )?;
            probe.check_root()?;
            Ok::<_, io::Error>((scan, probe.counts.json(), true))
        }
    })();
    let elapsed = start.elapsed().as_secs_f64() * 1000.0;
    match result {
        Ok((scan, calls, available)) => {
            let differences = expected
                .as_ref()
                .map(|r| crate::layout::differences(r, &scan));
            let count = differences.as_ref().map(Vec::len);
            println!(
                "{}",
                serde_json::json!({"completed":true,"strategy":method,"detailed":detailed,"elapsed_ms":elapsed,"files":scan.files.len(),"folders":scan.folders.len(),"bytes":scan.folders[0].logical_bytes,"errors":scan.error_count,"metadata_errors":scan.metadata_error_count,"reparse_excluded":scan.skipped_reparse_points,"available":available,"calls":calls,"difference_count":count,"differences":differences})
            );
            if count.is_some_and(|n| n != 0) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "후보 결과와 기준 결과가 다릅니다",
                ));
            }
            Ok(())
        }
        Err(error) => {
            println!(
                "{}",
                serde_json::json!({"completed":false,"strategy":method,"detailed":detailed,"elapsed_ms":elapsed,"error":error.to_string(),"error_kind":format!("{:?}",error.kind())})
            );
            Err(error)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn identical_method_control_compares_real_scans_without_using_a_candidate() {
        let root =
            std::env::temp_dir().join(format!("diskweave-method-control-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("a.bin"), b"abc").unwrap();
        profile(
            &[root.as_os_str().into(), "detailed".into(), "control".into()],
            true,
        )
        .unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn cached_parent_and_analysis_root_replacement_are_not_accepted_as_original_objects() {
        let root =
            std::env::temp_dir().join(format!("diskweave-query-parent-{}", std::process::id()));
        let parent = root.join("sub");
        std::fs::create_dir_all(&parent).unwrap();
        let path = parent.join("a");
        std::fs::write(&path, b"old").unwrap();
        let probe = Relative::open(&root).unwrap();
        if probe.available() {
            let request = MetadataRequest {
                attributes:0,
                logical_bytes: 0,
                allocation: None,
                index: 0,
                path: path.clone(),
                identity: None,
            };
            assert!(
                probe.read_batch(std::slice::from_ref(&request), &AtomicBool::new(false))[0]
                    .issue
                    .is_none()
            );
            std::fs::rename(&parent, root.join("moved")).unwrap();
            std::fs::create_dir(&parent).unwrap();
            std::fs::write(&path, b"new").unwrap();
            assert_eq!(
                probe.read_batch(&[request], &AtomicBool::new(false))[0]
                    .issue
                    .as_ref()
                    .unwrap()
                    .kind(),
                io::ErrorKind::InvalidData
            );
        }
        let moved = root.with_extension("moved");
        std::fs::rename(&root, &moved).unwrap();
        std::fs::create_dir(&root).unwrap();
        assert_eq!(
            probe.check_root().err().unwrap().kind(),
            io::ErrorKind::InvalidData
        );
        std::fs::remove_dir_all(root).unwrap();
        std::fs::remove_dir_all(moved).unwrap();
    }
    #[test]
    fn comparison_cli_rejects_unknown_candidates_modes_and_invalid_time_limits() {
        for (mode, method, limit) in [
            ("detail", "relative", "1"),
            ("basic", "other", "1"),
            ("basic", "relative", "NaN"),
            ("detailed", "by-id", "0"),
        ] {
            let args = [
                "not-opened".into(),
                mode.into(),
                method.into(),
                limit.into(),
            ];
            assert_eq!(
                profile(&args, false).err().unwrap().kind(),
                io::ErrorKind::InvalidInput
            );
        }
    }
    #[test]
    fn native_boundary_distinguishes_unsupported_failure_and_corrupt_length() {
        unsafe extern "system" fn unsupported(
            _: *mut Objects,
            _: *mut Status,
            _: *mut std::ffi::c_void,
            _: u32,
            _: i32,
        ) -> i32 {
            0xc0000003u32 as i32
        }
        unsafe extern "system" fn denied(
            _: *mut Objects,
            _: *mut Status,
            _: *mut std::ffi::c_void,
            _: u32,
            _: i32,
        ) -> i32 {
            0xc0000022u32 as i32
        }
        unsafe extern "system" fn malformed(
            _: *mut Objects,
            s: *mut Status,
            _: *mut std::ffi::c_void,
            _: u32,
            _: i32,
        ) -> i32 {
            unsafe {
                (*s).information = 1;
            }
            0
        }
        let file = directory(&std::env::temp_dir()).unwrap();
        assert!(nt_stat(unsupported, &file, OsStr::new("a"))
            .unwrap()
            .is_none());
        assert_eq!(
            nt_stat(denied, &file, OsStr::new("a"))
                .err()
                .unwrap()
                .kind(),
            io::ErrorKind::PermissionDenied
        );
        assert_eq!(
            nt_stat(malformed, &file, OsStr::new("a"))
                .err()
                .unwrap()
                .kind(),
            io::ErrorKind::InvalidData
        );
    }
    #[test]
    fn long_lossless_names_fallback_and_cancelled_batches_keep_contract() {
        use std::os::windows::ffi::OsStringExt;
        let root =
            std::env::temp_dir().join(format!("diskweave-query-names-{}", std::process::id()));
        let mut parent = root.clone();
        for n in 0..5 {
            parent = parent.join(format!("{n}-{}", "x".repeat(50)));
        }
        std::fs::create_dir_all(&parent).unwrap();
        let name = std::ffi::OsString::from_wide(&[0x97d3, 0xd800, 46, 98, 105, 110]);
        let path = parent.join(name);
        std::fs::write(&path, b"abc").unwrap();
        let mut relative = Relative::open(&root).unwrap();
        let by_id = ById::open(&root).unwrap();
        let request = MetadataRequest {
                attributes:0,
            logical_bytes: 0,
            allocation: None,
            index: 0,
            path,
            identity: metadata::read(&parent.join(std::ffi::OsString::from_wide(&[
                0x97d3, 0xd800, 46, 98, 105, 110,
            ])))
            .values
            .identity
            .value(),
        };
        let expected = metadata::read(&request.path);
        for report in [
            relative
                .read_batch(std::slice::from_ref(&request), &AtomicBool::new(false))
                .remove(0),
            by_id
                .read_batch(std::slice::from_ref(&request), &AtomicBool::new(false))
                .remove(0),
        ] {
            assert!(report.issue.is_none());
            assert_eq!(report.values, expected.values);
        }
        relative.function = None;
        let reports = relative.read_batch(std::slice::from_ref(&request), &AtomicBool::new(false));
        assert_eq!(reports[0].values, expected.values);
        assert_eq!(relative.counts.fallback.load(Ordering::Relaxed), 1);
        let calls = by_id.counts.native_calls.load(Ordering::Relaxed);
        assert!(by_id
            .read_batch(&[request], &AtomicBool::new(true))
            .is_empty());
        assert_eq!(calls, by_id.counts.native_calls.load(Ordering::Relaxed));
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn queries_reject_parent_components_before_native_or_fallback_io() {
        let root =
            std::env::temp_dir().join(format!("diskweave-query-boundary-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let relative = Relative::open(&root).unwrap();
        let by_id = ById::open(&root).unwrap();
        let request = MetadataRequest {
                attributes:0,
            logical_bytes: 0,
            allocation: None,
            index: 0,
            path: root.join("../outside.bin"),
            identity: None,
        };
        for reports in [
            relative.read_batch(std::slice::from_ref(&request), &AtomicBool::new(false)),
            by_id.read_batch(&[request], &AtomicBool::new(false)),
        ] {
            assert_eq!(
                reports[0].issue.as_ref().unwrap().kind(),
                io::ErrorKind::InvalidInput
            );
        }
        assert_eq!(relative.counts.native_calls.load(Ordering::Relaxed), 0);
        assert_eq!(by_id.counts.fallback.load(Ordering::Relaxed), 0);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn relative_metadata_matches_reference_and_reports_actual_native_support() {
        let root = std::env::temp_dir().join(format!("diskweave-relative-{}", std::process::id()));
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::write(root.join("sub/a.bin"), b"1234567890123").unwrap();
        std::fs::hard_link(root.join("sub/a.bin"), root.join("alias.bin")).unwrap();
        let probe = Relative::open(&root).unwrap();
        let expected = crate::scanner::scan_with_options(&root, true, || false, |_| {}).unwrap();
        let observed = crate::scanner::scan_with_metadata_source(
            &root,
            true,
            crate::scanner::Parallelism::Eight,
            crate::scanner::directory::Strategy::LargeFetch,
            || false,
            |_| {},
            &probe,
        )
        .unwrap();
        assert!(crate::layout::differences(&expected, &observed).is_empty());
        if probe.available() {
            assert!(
                probe
                    .counts
                    .native_success
                    .load(std::sync::atomic::Ordering::Relaxed)
                    > 0
            );
        } else {
            assert!(
                probe
                    .counts
                    .fallback
                    .load(std::sync::atomic::Ordering::Relaxed)
                    > 0
            );
        }
        assert_eq!(abi_sizes(), [16, 48, 16, 104]);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn id_query_preserves_fields_and_rejects_path_replaced_after_enumeration() {
        use crate::scanner::MetadataProbe;
        let root = std::env::temp_dir().join(format!("diskweave-id-query-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("original.bin");
        std::fs::write(&path, b"1234567890123").unwrap();
        std::fs::hard_link(&path, root.join("keep.bin")).unwrap();
        let probe = ById::open(&root).unwrap();
        let expected = crate::scanner::scan_with_options(&root, true, || false, |_| {}).unwrap();
        let result = crate::scanner::scan_with_metadata_source(
            &root,
            true,
            crate::scanner::Parallelism::Eight,
            crate::scanner::directory::Strategy::Bulk,
            || false,
            |_| {},
            &probe,
        )
        .unwrap();
        assert!(crate::layout::differences(&expected, &result).is_empty());
        assert!(probe.counts.native_success.load(Ordering::Relaxed) > 0);
        let original = metadata::read(&path).values.identity.value().unwrap();
        std::fs::remove_file(&path).unwrap();
        std::fs::write(&path, b"new").unwrap();
        let reports = probe.read_batch(
            &[MetadataRequest {
                attributes:0,
                logical_bytes: 0,
                allocation: None,
                index: 0,
                path: path.clone(),
                identity: Some(original),
            }],
            &AtomicBool::new(false),
        );
        assert!(reports[0].issue.is_some());
        assert!(reports[0].values.identity.value().is_none());
        assert_eq!(std::fs::read(&path).unwrap(), b"new");
        std::fs::remove_dir_all(root).unwrap();
    }
}
