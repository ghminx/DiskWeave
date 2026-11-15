use crate::{
    engine::{Method, Progress, Report, Stage},
    ntfs_native::{self, Target},
    scanner::{ScanProgress, ScanResult},
    snapshot::SavedScan,
};
use serde::{Deserialize, Serialize};
use std::{
    ffi::{OsStr, OsString},
    io::{self, Read, Write},
    os::windows::ffi::OsStrExt,
    path::Path,
    ptr::{null, null_mut},
    sync::Arc,
    time::{Duration, Instant, SystemTime},
};
use windows_sys::Win32::{
    Foundation::*,
    Security::{Authorization::*, Cryptography::*, *},
    Storage::FileSystem::*,
    System::{Com::*, Pipes::*, Threading::*, IO::*},
    UI::{Shell::*, WindowsAndMessaging::SW_HIDE},
};

fn wide(s: impl AsRef<OsStr>) -> Vec<u16> {
    s.as_ref().encode_wide().chain(Some(0)).collect()
}
fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}
struct Handle(HANDLE);
impl Drop for Handle {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.0);
        }
    }
}
struct Local(*mut core::ffi::c_void);
impl Drop for Local {
    fn drop(&mut self) {
        unsafe {
            LocalFree(self.0);
        }
    }
}
struct Worker(Handle);
impl Drop for Worker {
    fn drop(&mut self) {
        // 이 호출에서 생성한 읽기 전용 도우미 핸들에만 종료 요청
        unsafe {
            if WaitForSingleObject(self.0 .0, 0) == WAIT_TIMEOUT {
                TerminateProcess(self.0 .0, 130);
            }
        }
    }
}

/// 부모·도우미 양쪽에서 대상과 계산 옵션이 일치한 완료 응답만 수락.
pub fn validate_response(record: SavedScan, root: &Path, detailed: bool) -> io::Result<ScanResult> {
    let correct = record.engine.as_ref().is_some_and(|r| {
        r.requested == Method::Ntfs && r.actual == Method::Ntfs && r.fallback.is_none()
    });
    if !correct
        || record.scan.detailed != detailed
        || record.scan.folders.first().is_none_or(|f| f.path != root)
    {
        return Err(invalid("스캔 도우미의 응답 대상·방식이 일치하지 않습니다"));
    }
    Arc::try_unwrap(record.scan).map_err(|_| invalid("공유 중인 도우미 응답"))
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    version: u32,
    target: Target,
    detailed: bool,
}
#[derive(Serialize, Deserialize)]
enum Message {
    Progress {
        stage: Stage,
        files: u64,
        folders: u64,
    },
    Ready,
    Failed(String),
}

struct Pipe<'a> {
    handle: Handle,
    cancelled: &'a dyn Fn() -> bool,
}
impl Pipe<'_> {
    fn check(&self, start: Instant) -> io::Result<()> {
        if (self.cancelled)() {
            return Err(io::Error::other("스캔 취소"));
        }
        if start.elapsed() > Duration::from_secs(300) {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "스캔 도우미 응답 시간 초과",
            ));
        }
        Ok(())
    }
    fn event() -> io::Result<Handle> {
        let e = unsafe { CreateEventW(null(), 1, 0, null()) };
        if e.is_null() {
            Err(io::Error::last_os_error())
        } else {
            Ok(Handle(e))
        }
    }
    fn wait(&self, overlap: &mut OVERLAPPED, start: Instant) -> io::Result<u32> {
        loop {
            let state = unsafe { WaitForSingleObject(overlap.hEvent, 25) };
            let error = if state == WAIT_OBJECT_0 {
                None
            } else if state == WAIT_FAILED {
                Some(io::Error::last_os_error())
            } else {
                self.check(start).err()
            };
            if let Some(e) = error {
                // 커널 참조 종료까지 OVERLAPPED와 호출 버퍼의 수명 유지
                unsafe {
                    CancelIoEx(self.handle.0, overlap);
                    let mut bytes = 0;
                    GetOverlappedResult(self.handle.0, overlap, &mut bytes, 1);
                }
                return Err(e);
            }
            if state == WAIT_OBJECT_0 {
                let mut bytes = 0;
                return if unsafe { GetOverlappedResult(self.handle.0, overlap, &mut bytes, 0) } == 0
                {
                    Err(io::Error::last_os_error())
                } else {
                    Ok(bytes)
                };
            }
        }
    }
    fn connect(&self) -> io::Result<()> {
        let event = Self::event()?;
        let mut o: OVERLAPPED = unsafe { std::mem::zeroed() };
        o.hEvent = event.0;
        if unsafe { ConnectNamedPipe(self.handle.0, &mut o) } != 0 {
            return Ok(());
        }
        let e = io::Error::last_os_error();
        match e.raw_os_error() {
            Some(535) => Ok(()),
            Some(997) => self.wait(&mut o, Instant::now()).map(|_| ()),
            _ => Err(e),
        }
    }
    fn transfer(&self, buffer: *mut u8, len: usize, read: bool) -> io::Result<usize> {
        self.check(Instant::now())?;
        let event = Self::event()?;
        let mut o: OVERLAPPED = unsafe { std::mem::zeroed() };
        o.hEvent = event.0;
        let mut bytes = 0;
        // 호출 버퍼만 전달하고 보류 I/O의 완료 또는 취소 완료까지 대기
        let ok = unsafe {
            if read {
                ReadFile(
                    self.handle.0,
                    buffer,
                    len.min(u32::MAX as usize) as u32,
                    &mut bytes,
                    &mut o,
                )
            } else {
                WriteFile(
                    self.handle.0,
                    buffer.cast_const(),
                    len.min(u32::MAX as usize) as u32,
                    &mut bytes,
                    &mut o,
                )
            }
        };
        let result = if ok != 0 {
            Ok(bytes)
        } else {
            let e = io::Error::last_os_error();
            if e.raw_os_error() == Some(997) {
                self.wait(&mut o, Instant::now())
            } else {
                Err(e)
            }
        };
        match result {
            Err(e) if read && e.raw_os_error() == Some(ERROR_BROKEN_PIPE as i32) => Ok(0),
            other => other.map(|n| n as usize),
        }
    }
    fn send(&mut self, value: &impl Serialize) -> io::Result<()> {
        let data = serde_json::to_vec(value).map_err(crate::output::json_error)?;
        if data.len() > 16 * 1024 {
            return Err(invalid("도우미 제어 메시지가 너무 큽니다"));
        }
        self.write_all(&(data.len() as u32).to_le_bytes())?;
        self.write_all(&data)
    }
    fn receive<T: serde::de::DeserializeOwned>(&mut self) -> io::Result<T> {
        let mut size = [0; 4];
        self.read_exact(&mut size)?;
        let size = u32::from_le_bytes(size) as usize;
        if size > 16 * 1024 {
            return Err(invalid("도우미 메시지 길이 초과"));
        }
        let mut data = vec![0; size];
        self.read_exact(&mut data)?;
        serde_json::from_slice(&data).map_err(crate::output::json_error)
    }
}
impl Read for Pipe<'_> {
    fn read(&mut self, b: &mut [u8]) -> io::Result<usize> {
        if b.is_empty() {
            Ok(0)
        } else {
            self.transfer(b.as_mut_ptr(), b.len(), true)
        }
    }
}
impl Write for Pipe<'_> {
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        if b.is_empty() {
            Ok(0)
        } else {
            self.transfer(b.as_ptr().cast_mut(), b.len(), false)
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
fn descriptor() -> io::Result<Local> {
    let mut raw = null_mut();
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut raw) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let token = Handle(raw);
    let mut size = 0;
    unsafe {
        GetTokenInformation(token.0, TokenUser, null_mut(), 0, &mut size);
    }
    let mut data = vec![0usize; (size as usize).div_ceil(std::mem::size_of::<usize>())];
    if unsafe {
        GetTokenInformation(
            token.0,
            TokenUser,
            data.as_mut_ptr().cast(),
            size,
            &mut size,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    let user = unsafe { &*data.as_ptr().cast::<TOKEN_USER>() };
    let mut sid = null_mut();
    if unsafe { ConvertSidToStringSidW(user.User.Sid, &mut sid) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let _sid = Local(sid.cast());
    let mut len = 0;
    while unsafe { *sid.add(len) } != 0 {
        len += 1;
    }
    let sid = String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(sid, len) });
    let sddl = wide(format!("D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;GA;;;{sid})"));
    let mut d = null_mut();
    if unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(sddl.as_ptr(), 1, &mut d, null_mut())
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(Local(d))
}
fn nonce() -> io::Result<String> {
    let mut bytes = [0u8; 32];
    if unsafe {
        BCryptGenRandom(
            null_mut(),
            bytes.as_mut_ptr(),
            32,
            BCRYPT_USE_SYSTEM_PREFERRED_RNG,
        )
    } < 0
    {
        return Err(io::Error::other("도우미 연결 키 생성 실패"));
    }
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}
fn pipe_name(nonce: &str) -> io::Result<Vec<u16>> {
    if nonce.len() != 64 || !nonce.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(invalid("잘못된 도우미 연결 키"));
    }
    Ok(wide(format!("\\\\.\\pipe\\DiskWeave-{nonce}")))
}
fn spawn(nonce: &str) -> io::Result<Worker> {
    let initialized = unsafe {
        CoInitializeEx(
            null(),
            COINIT_APARTMENTTHREADED as u32 | COINIT_DISABLE_OLE1DDE as u32,
        )
    };
    if initialized < 0 {
        return Err(io::Error::other(format!(
            "Windows 권한 요청 초기화 실패: {initialized:#x}"
        )));
    }
    struct Com;
    impl Drop for Com {
        fn drop(&mut self) {
            unsafe {
                CoUninitialize();
            }
        }
    }
    let _com = Com;
    let exe = wide(std::env::current_exe()?);
    let args = wide(format!("--ntfs-worker {} {nonce}", std::process::id()));
    let verb = wide("runas");
    let mut info: SHELLEXECUTEINFOW = unsafe { std::mem::zeroed() };
    info.cbSize = std::mem::size_of_val(&info) as u32;
    info.fMask = SEE_MASK_NOCLOSEPROCESS | SEE_MASK_NOASYNC | SEE_MASK_FLAG_NO_UI;
    info.lpFile = exe.as_ptr();
    info.lpParameters = args.as_ptr();
    info.lpVerb = verb.as_ptr();
    info.nShow = SW_HIDE;
    if unsafe { ShellExecuteExW(&mut info) } == 0 {
        return Err(io::Error::last_os_error());
    }
    if info.hProcess.is_null() {
        return Err(io::Error::other("도우미 프로세스 핸들이 없습니다"));
    }
    Ok(Worker(Handle(info.hProcess)))
}
pub fn scan(
    root: &Path,
    detailed: bool,
    cancelled: &dyn Fn() -> bool,
    progress: &mut dyn FnMut(Progress),
) -> io::Result<ScanResult> {
    let target = ntfs_native::target(root)?;
    let key = nonce()?;
    let name = pipe_name(&key)?;
    let sd = descriptor()?;
    let sa = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: sd.0,
        bInheritHandle: 0,
    };
    let h = unsafe {
        CreateNamedPipeW(
            name.as_ptr(),
            PIPE_ACCESS_DUPLEX | FILE_FLAG_OVERLAPPED | FILE_FLAG_FIRST_PIPE_INSTANCE,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
            1,
            256 * 1024,
            256 * 1024,
            0,
            &sa,
        )
    };
    if h == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    let mut pipe = Pipe {
        handle: Handle(h),
        cancelled,
    };
    progress(Progress {
        stage: Stage::Authorizing,
        counts: Default::default(),
    });
    let worker = spawn(&key).map_err(|e| {
        io::Error::new(
            e.kind(),
            format!("Windows 권한 승인을 받지 못했습니다. {e}"),
        )
    })?;
    pipe.connect()?;
    let mut peer = 0;
    if unsafe { GetNamedPipeClientProcessId(pipe.handle.0, &mut peer) } == 0
        || peer != unsafe { GetProcessId(worker.0 .0) }
    {
        return Err(invalid("스캔 도우미 프로세스가 일치하지 않습니다"));
    }
    pipe.send(&Request {
        version: 1,
        target: target.clone(),
        detailed,
    })?;
    loop {
        match pipe.receive()? {
            Message::Progress {
                stage,
                files,
                folders,
            } => progress(Progress {
                stage,
                counts: ScanProgress {
                    files_scanned: files,
                    folders_scanned: folders,
                },
            }),
            Message::Failed(e) => return Err(io::Error::other(e)),
            Message::Ready => break,
        }
    }
    progress(Progress {
        stage: Stage::Transferring,
        counts: Default::default(),
    });
    let record = crate::snapshot::read_from(&mut pipe, cancelled)?;
    if ntfs_native::target(root)?.volume != target.volume {
        return Err(invalid("결과 수신 중 드라이브가 변경됐습니다"));
    }
    validate_response(record, Path::new(&target.root), detailed)
}
pub fn worker(arguments: &[OsString]) -> io::Result<()> {
    if arguments.len() != 2 {
        return Err(invalid("도우미 인자가 올바르지 않습니다"));
    }
    let parent = arguments[0]
        .to_str()
        .and_then(|s| s.parse::<u32>().ok())
        .filter(|n| *n != 0)
        .ok_or_else(|| invalid("잘못된 부모 프로세스"))?;
    let key = arguments[1]
        .to_str()
        .ok_or_else(|| invalid("잘못된 연결 키"))?;
    let name = pipe_name(key)?;
    let h = unsafe {
        CreateFileW(
            name.as_ptr(),
            GENERIC_READ | GENERIC_WRITE,
            0,
            null(),
            OPEN_EXISTING,
            FILE_FLAG_OVERLAPPED | SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION,
            null_mut(),
        )
    };
    if h == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    let never = || false;
    let mut pipe = Pipe {
        handle: Handle(h),
        cancelled: &never,
    };
    let mut server = 0;
    if unsafe { GetNamedPipeServerProcessId(pipe.handle.0, &mut server) } == 0 || server != parent {
        return Err(invalid("부모 프로세스가 일치하지 않습니다"));
    }
    let request: Request = pipe.receive()?;
    if request.version != 1 {
        return Err(invalid("지원하지 않는 도우미 버전"));
    }
    let root = Path::new(&request.target.root);
    let begin = SystemTime::now();
    let clock = Instant::now();
    let disconnected = std::cell::Cell::new(false);
    let mut last = Instant::now() - Duration::from_secs(1);
    let result = ntfs_native::Native::open(&request.target).and_then(|mut source| {
        crate::ntfs::collect(
            root,
            request.detailed,
            &mut source,
            || disconnected.get(),
            |counts| {
                if last.elapsed() >= Duration::from_millis(100) {
                    let stage = if counts.folders_scanned == 0 {
                        Stage::Enumerating
                    } else {
                        Stage::Metadata
                    };
                    if pipe
                        .send(&Message::Progress {
                            stage,
                            files: counts.files_scanned,
                            folders: counts.folders_scanned,
                        })
                        .is_err()
                    {
                        disconnected.set(true);
                    }
                    last = Instant::now();
                }
            },
        )
    });
    match result {
        Ok(result) => {
            let mut record =
                SavedScan::new(Arc::new(result), begin, SystemTime::now(), clock.elapsed());
            record.engine = Some(Report {
                requested: Method::Ntfs,
                actual: Method::Ntfs,
                fallback: None,
            });
            pipe.send(&Message::Ready)?;
            let mut writer = io::BufWriter::with_capacity(256 * 1024, &mut pipe);
            crate::snapshot::write_to(&record, &mut writer, || disconnected.get())?;
            writer.flush()?;
        }
        Err(e) => pipe.send(&Message::Failed(e.to_string()))?,
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn helper_responses_must_match_requested_root_mode_and_details() {
        let root = std::env::temp_dir().join(format!("diskweave-response-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let make = || {
            let mut r = crate::snapshot::SavedScan::new(
                std::sync::Arc::new(crate::scanner::scan(&root, || false).unwrap()),
                std::time::SystemTime::now(),
                std::time::SystemTime::now(),
                std::time::Duration::ZERO,
            );
            r.engine = Some(crate::engine::Report {
                requested: crate::engine::Method::Ntfs,
                actual: crate::engine::Method::Ntfs,
                fallback: None,
            });
            r
        };
        assert!(validate_response(make(), &root, false).is_ok());
        assert!(validate_response(make(), &root, true).is_err());
        assert!(validate_response(make(), &root.join("other"), false).is_err());
        let mut r = make();
        r.engine = None;
        assert!(validate_response(r, &root, false).is_err());
        for p in [
            "C:\\folder",
            "\\\\server\\share\\",
            "C:\\..\\",
            "C:/",
            "\\\\.\\C:",
        ] {
            assert!(crate::ntfs_native::target(std::path::Path::new(p)).is_err());
        }
        std::fs::remove_dir_all(root).unwrap();
    }
}
