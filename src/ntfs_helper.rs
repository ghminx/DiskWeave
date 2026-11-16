use crate::{
    engine::{Collector, FastMode, Method, Outcome, Progress, Report, Stage},
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
// 독점 소유한 Windows 커널 핸들은 스레드 간 이전 가능. 복제 소유 없이 Mutex로 순차 사용.
unsafe impl Send for Handle {}
struct Prepared {
    handle: Handle,
    worker: Worker,
}
static PREPARED: std::sync::Mutex<Option<Prepared>> = std::sync::Mutex::new(None);
static SKIP_PROMPT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
pub fn skip_permission_prompt() {
    SKIP_PROMPT.store(true, std::sync::atomic::Ordering::Relaxed);
}
pub fn shutdown() {
    *PREPARED.lock().unwrap() = None;
}
pub fn prepare() -> io::Result<()> {
    let (pipe, worker) = launch(&|| false)?;
    *PREPARED.lock().unwrap() = Some(Prepared {
        handle: pipe.handle,
        worker,
    });
    SKIP_PROMPT.store(false, std::sync::atomic::Ordering::Relaxed);
    Ok(())
}
fn prepared<'a>(cancelled: &'a dyn Fn() -> bool) -> io::Result<(Pipe<'a>, Worker)> {
    if let Some(prepared) = PREPARED.lock().unwrap().take() {
        if unsafe { WaitForSingleObject(prepared.worker.0 .0, 0) } == WAIT_TIMEOUT {
            return Ok((
                Pipe {
                    handle: prepared.handle,
                    cancelled,
                    idle: false,
                },
                prepared.worker,
            ));
        }
    }
    if SKIP_PROMPT.load(std::sync::atomic::Ordering::Relaxed) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "이번 실행에서는 고속 스캔 권한 요청을 건너뛰었습니다",
        ));
    }
    launch(cancelled)
}
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
pub(crate) struct Worker(Handle);
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
    #[serde(default)]
    session: bool,
    target: Target,
    detailed: bool,
    #[serde(default)]
    watch: Option<Vec<u16>>,
    #[serde(default)]
    resume: bool,
    #[serde(default)]
    fast_mode: FastMode,
    #[serde(default)]
    scope: Option<Vec<u16>>,
}
#[derive(Serialize, Deserialize)]
enum Message {
    Preview(crate::scanner::preview::Batch),
    Progress {
        stage: Stage,
        files: u64,
        folders: u64,
    },
    Ready,
    Checking,
    Cursor,
    Failed(String),
}

pub(crate) struct Pipe<'a> {
    idle: bool,
    handle: Handle,
    cancelled: &'a dyn Fn() -> bool,
}
impl Pipe<'_> {
    fn check(&self, start: Instant) -> io::Result<()> {
        if (self.cancelled)() {
            // Read/Write의 기본 재시도에서 취소가 영구 반복되지 않도록 연결 종료로 전달.
            return Err(io::Error::new(
                io::ErrorKind::ConnectionAborted,
                "스캔 취소",
            ));
        }
        if !self.idle && start.elapsed() > Duration::from_secs(300) {
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
    pub(crate) fn send(&mut self, value: &impl Serialize) -> io::Result<()> {
        let data = serde_json::to_vec(value).map_err(crate::output::json_error)?;
        if data.len() > 256 * 1024 {
            return Err(invalid("도우미 제어 메시지가 너무 큽니다"));
        }
        self.write_all(&(data.len() as u32).to_le_bytes())?;
        self.write_all(&data)
    }
    pub(crate) fn receive<T: serde::de::DeserializeOwned>(&mut self) -> io::Result<T> {
        let mut size = [0; 4];
        self.read_exact(&mut size)?;
        let size = u32::from_le_bytes(size) as usize;
        if size > 256 * 1024 {
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
struct Chunks<R> {
    pipe: R,
    remaining: usize,
    total: u64,
    ended: bool,
}
impl<R: Read> Read for Chunks<R> {
    fn read(&mut self, b: &mut [u8]) -> io::Result<usize> {
        if b.is_empty() || self.ended {
            return Ok(0);
        }
        if self.remaining == 0 {
            let mut n = [0u8; 4];
            self.pipe.read_exact(&mut n)?;
            self.remaining = u32::from_le_bytes(n) as usize;
            if self.remaining == 0 {
                self.ended = true;
                return Ok(0);
            }
            if self.remaining > 65536 {
                return Err(invalid("도우미 데이터 조각 길이 초과"));
            }
            self.total += self.remaining as u64;
            if self.total > 1_073_741_824 {
                return Err(invalid("도우미 데이터 크기 초과"));
            }
        }
        let len = b.len().min(self.remaining);
        self.pipe.read_exact(&mut b[..len])?;
        self.remaining -= len;
        Ok(len)
    }
}
struct ChunkWriter<W> {
    pipe: W,
    total: u64,
}
impl<W: Write> Write for ChunkWriter<W> {
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        if b.is_empty() {
            return Ok(0);
        }
        let n = b.len().min(65536);
        self.total += n as u64;
        if self.total > 1_073_741_824 {
            return Err(invalid("도우미 데이터 크기 초과"));
        }
        self.pipe.write_all(&(n as u32).to_le_bytes())?;
        self.pipe.write_all(&b[..n])?;
        Ok(n)
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
/// 결과 끝 표시를 포함한 유한 응답 전송. 연결 유지 상태에서도 수신 종료 보장.
pub fn write_completed(mut writer: impl Write, record: &SavedScan) -> io::Result<()> {
    let chunks = ChunkWriter {
        pipe: &mut writer,
        total: 0,
    };
    let mut buffer = io::BufWriter::with_capacity(65536, chunks);
    crate::snapshot::write_to(record, &mut buffer, || false)?;
    buffer.flush()?;
    drop(buffer);
    writer.write_all(&0u32.to_le_bytes())
}
fn read_record_from(reader: impl Read) -> io::Result<SavedScan> {
    crate::snapshot::read_from(
        Chunks {
            pipe: reader,
            remaining: 0,
            total: 0,
            ended: false,
        },
        || false,
    )
}
#[allow(dead_code, reason = "기존 전송·도구 공개 API의 회귀 대조")]
pub fn read_completed(reader: impl Read, root: &Path, detailed: bool) -> io::Result<ScanResult> {
    validate_response(read_record_from(reader)?, root, detailed)
}
impl Pipe<'_> {
    fn record(&mut self, record: &SavedScan) -> io::Result<()> {
        write_completed(self, record)
    }
    fn read_record(&mut self) -> io::Result<SavedScan> {
        read_record_from(self)
    }
}
#[derive(Serialize, Deserialize)]
struct Advance {
    next: i64,
    pending: Vec<u64>,
}
/// 명시적으로 선택한 NTFS 위치의 변경 추적을 제한된 읽기 전용 도우미에서 실행.
pub fn watch(
    root: &Path,
    detailed: bool,
    resume: Option<Arc<SavedScan>>,
    cancelled: &dyn Fn() -> bool,
    mut publish: impl FnMut(SavedScan),
    mut progress: impl FnMut(Progress),
) -> io::Result<()> {
    let target = crate::usn_native::target(root)?;
    let (mut pipe, _worker) = launch(cancelled)?;
    pipe.send(&Request {
        version: 1,
        session: false,
        target: target.clone(),
        detailed,
        watch: Some(root.as_os_str().encode_wide().collect()),
        resume: resume.is_some(),
        fast_mode: FastMode::Auto,
        scope: None,
    })?;
    if let Some(record) = &resume {
        pipe.record(record)?;
    }
    let mut previous: Option<SavedScan> = None;
    loop {
        crate::output::check(&cancelled)?;
        let record = match pipe.receive()? {
            Message::Preview(_) => return Err(invalid("실시간 추적에 잘못된 미리보기 응답")),
            Message::Ready => {
                let record = pipe.read_record()?;
                if record.scan.folders[0].path != root
                    || record.scan.detailed != detailed
                    || record
                        .tracking
                        .as_ref()
                        .is_none_or(|c| c.volume != target.volume)
                {
                    return Err(invalid("변경 추적 응답의 대상·옵션 불일치"));
                }
                record
            }
            Message::Cursor => {
                let value: Advance = serde_json::from_reader(Chunks {
                    pipe: &mut pipe,
                    remaining: 0,
                    total: 0,
                    ended: false,
                })
                .map_err(crate::output::json_error)?;
                let old = previous
                    .as_ref()
                    .ok_or_else(|| invalid("기준 결과 없는 변경 위치 응답"))?;
                let mut next = SavedScan::new(
                    Arc::clone(&old.scan),
                    old.started,
                    SystemTime::now(),
                    old.elapsed,
                );
                next.tracking = old.tracking.clone();
                let state = next.tracking.as_mut().unwrap();
                if value.next < state.next || value.pending.len() > state.files.len() {
                    return Err(invalid("올바르지 않은 변경 추적 위치"));
                }
                state.next = value.next;
                state.pending = value.pending;
                state.validate(&next.scan)?;
                next
            }
            Message::Checking => {
                progress(Progress {
                    stage: Stage::Basic,
                    counts: Default::default(),
                });
                continue;
            }
            Message::Progress {
                stage,
                files,
                folders,
            } => {
                progress(Progress {
                    stage,
                    counts: ScanProgress {
                        files_scanned: files,
                        folders_scanned: folders,
                    },
                });
                continue;
            }
            Message::Failed(error) => return Err(io::Error::other(error)),
        };
        if crate::usn_native::target(root)?.volume != target.volume {
            return Err(invalid("응답 중 추적 드라이브가 바뀌었습니다"));
        }
        let mut delivered = SavedScan::new(
            Arc::clone(&record.scan),
            record.started,
            record.finished,
            record.elapsed,
        );
        delivered.tracking = record.tracking.clone();
        delivered.engine = record.engine.clone();
        previous = Some(record);
        publish(delivered);
    }
}
fn watch_worker(pipe: &mut Pipe<'_>, request: &Request) -> io::Result<()> {
    use std::os::windows::ffi::OsStringExt;
    let value = request.watch.as_ref().unwrap();
    if value.len() > 32767 || value.contains(&0) {
        return Err(invalid("추적 경로 길이·형식 오류"));
    }
    let root = std::path::PathBuf::from(OsString::from_wide(value));
    if !root.is_absolute() {
        return Err(invalid("추적 경로는 절대 경로여야 합니다"));
    }
    let result: io::Result<()> = (|| {
        if crate::usn_native::target(&root)?.volume != request.target.volume {
            return Err(invalid("추적 볼륨 요청 불일치"));
        }
        let mut source = crate::usn_native::Native::open(&root)?;
        let mut index = if request.resume {
            let record = pipe.read_record()?;
            if record.scan.folders[0].path != root || record.scan.detailed != request.detailed {
                return Err(invalid("복원 대상·수집 옵션 불일치"));
            }
            let mut index = crate::usn::Index::restore(&record)?;
            index.refresh(&mut Traced::new(&mut source, pipe), &|| false)?;
            index
        } else {
            crate::usn::Index::begin(
                &root,
                request.detailed,
                &mut Traced::new(&mut source, pipe),
                &|| false,
            )?
        };
        let record = index.snapshot(&|| false)?;
        pipe.send(&Message::Ready)?;
        pipe.record(&record)?;
        loop {
            pipe.send(&Message::Checking)?;
            std::thread::sleep(Duration::from_secs(1));
            let before = (index.checkpoint.next, index.checkpoint.pending.clone());
            let changed = index.refresh(&mut Traced::new(&mut source, pipe), &|| false)?;
            if changed {
                let record = index.snapshot(&|| false)?;
                pipe.send(&Message::Ready)?;
                pipe.record(&record)?;
            } else if index.checkpoint.next != before.0 || index.checkpoint.pending != before.1 {
                pipe.send(&Message::Cursor)?;
                let value = Advance {
                    next: index.checkpoint.next,
                    pending: index.checkpoint.pending.clone(),
                };
                let mut writer = io::BufWriter::with_capacity(
                    65536,
                    ChunkWriter {
                        pipe: &mut *pipe,
                        total: 0,
                    },
                );
                serde_json::to_writer(&mut writer, &value).map_err(crate::output::json_error)?;
                writer.flush()?;
                drop(writer);
                pipe.write_all(&0u32.to_le_bytes())?;
            }
        }
    })();
    if let Err(error) = result {
        pipe.send(&Message::Failed(error.to_string()))?;
    }
    Ok(())
}
struct Traced<'a, 'p, 'c> {
    source: &'a mut crate::usn_native::Native,
    pipe: &'p mut Pipe<'c>,
    last: Instant,
    counts: ScanProgress,
}
impl<'a, 'p, 'c> Traced<'a, 'p, 'c> {
    fn new(source: &'a mut crate::usn_native::Native, pipe: &'p mut Pipe<'c>) -> Self {
        Self {
            source,
            pipe,
            last: Instant::now() - Duration::from_secs(1),
            counts: ScanProgress::default(),
        }
    }
    fn notify(&mut self) -> io::Result<()> {
        if self.last.elapsed() >= Duration::from_millis(100) {
            self.pipe.send(&Message::Progress {
                stage: Stage::Metadata,
                files: self.counts.files_scanned,
                folders: self.counts.folders_scanned,
            })?;
            self.last = Instant::now();
        }
        Ok(())
    }
}
impl crate::usn::Source for Traced<'_, '_, '_> {
    fn journal(&mut self) -> io::Result<crate::usn::Journal> {
        self.notify()?;
        self.source.journal()
    }
    fn read(&mut self, start: i64, end: i64) -> io::Result<Vec<crate::usn::Event>> {
        self.source.read(start, end)
    }
    fn inspect(&mut self, path: &Path, detailed: bool) -> io::Result<crate::ntfs::Info> {
        let info = self.source.inspect(path, detailed)?;
        if info.attributes & 0x10 != 0 {
            self.counts.folders_scanned += 1;
        } else {
            self.counts.files_scanned += 1;
        }
        self.notify()?;
        Ok(info)
    }
    fn list(&mut self, path: &Path) -> io::Result<Vec<std::path::PathBuf>> {
        self.source.list(path)
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
pub(crate) fn launch(cancelled: &dyn Fn() -> bool) -> io::Result<(Pipe<'_>, Worker)> {
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
    let pipe = Pipe {
        idle: false,
        handle: Handle(h),
        cancelled,
    };
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
    Ok((pipe, worker))
}
#[allow(dead_code, reason = "기존 CLI·개발 도구의 비미리보기 호출 경계 유지")]
pub fn scan(
    root: &Path,
    detailed: bool,
    cancelled: &dyn Fn() -> bool,
    progress: &mut dyn FnMut(Progress),
) -> io::Result<ScanResult> {
    scan_preview(root, detailed, cancelled, progress, &mut |_| Ok(()))
}
pub fn scan_preview(
    root: &Path,
    detailed: bool,
    cancelled: &dyn Fn() -> bool,
    progress: &mut dyn FnMut(Progress),
    preview: &mut crate::scanner::preview::Observer<'_>,
) -> io::Result<ScanResult> {
    Ok(scan_selected(root, detailed, FastMode::Auto, cancelled, progress, preview)?.result)
}
pub fn scan_selected(
    root: &Path,
    detailed: bool,
    mode: FastMode,
    cancelled: &dyn Fn() -> bool,
    progress: &mut dyn FnMut(Progress),
    preview: &mut crate::scanner::preview::Observer<'_>,
) -> io::Result<Outcome> {
    let target = selected_target(root, mode)?;
    progress(Progress {
        stage: Stage::Authorizing,
        counts: Default::default(),
    });
    let (mut pipe, worker) = prepared(cancelled)?;
    pipe.send(&Request {
        version: 1,
        session: true,
        target: target.clone(),
        detailed,
        watch: None,
        resume: false,
        fast_mode: mode,
        scope: (mode == FastMode::Mft).then(|| root.as_os_str().encode_wide().collect()),
    })?;
    let mut validation = crate::scanner::preview::ValidatedStream::new(root.to_owned());
    loop {
        match pipe.receive()? {
            Message::Preview(batch) => {
                validation.accept(batch.clone())?;
                preview(batch)?;
            }
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
            _ => return Err(invalid("일반 스캔에 잘못된 도우미 응답")),
        }
    }
    progress(Progress {
        stage: Stage::Transferring,
        counts: Default::default(),
    });
    let result = validate_selected_response(read_record_from(&mut pipe)?, root, detailed, mode)?;
    if selected_target(root, mode)?.volume != target.volume {
        return Err(invalid("결과 수신 중 드라이브가 변경됐습니다"));
    }
    crate::output::check(&|| cancelled())?;
    *PREPARED.lock().unwrap() = Some(Prepared {
        handle: pipe.handle,
        worker,
    });
    Ok(result)
}
pub fn selected_target(root: &Path, mode: FastMode) -> io::Result<Target> {
    if mode == FastMode::Mft {
        if !root.is_absolute()
            || root
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
            || root.as_os_str().encode_wide().any(|u| u == 0)
        {
            return Err(invalid("MFT 분석 폴더의 절대 경로가 올바르지 않습니다"));
        }
        crate::usn_native::target(root)
    } else {
        ntfs_native::target(root)
    }
}
pub fn validate_selected_response(
    record: SavedScan,
    root: &Path,
    detailed: bool,
    mode: FastMode,
) -> io::Result<Outcome> {
    let report = record
        .engine
        .clone()
        .ok_or_else(|| invalid("실제 수집 방식이 없는 응답"))?;
    if report.fast_mode != Some(mode)
        || match mode {
            FastMode::Mft => report.collector != Some(Collector::MftMixed),
            _ => report.collector != Some(Collector::NtfsRecords),
        }
    {
        return Err(invalid("요청한 고속 수집 방식과 응답이 일치하지 않습니다"));
    }
    Ok(Outcome {
        result: validate_response(record, root, detailed)?,
        report,
    })
}
fn send_preview(pipe: &mut Pipe<'_>, batch: crate::scanner::preview::Batch) -> io::Result<()> {
    use crate::scanner::preview::{Batch, Update};
    let mut packet = Batch {
        reset: batch.reset,
        updates: Vec::new(),
    };
    let mut estimated = 64;
    for update in batch.updates {
        let bytes = match &update {
            Update::Folder { name, .. } => 128 + name.len() * 6,
            Update::Values { .. } => 200,
            Update::Finished => 32,
        };
        if !packet.updates.is_empty()
            && (estimated + bytes > 192 * 1024 || packet.updates.len() == 128)
        {
            pipe.send(&Message::Preview(std::mem::take(&mut packet)))?;
            estimated = 64;
        }
        estimated += bytes;
        packet.updates.push(update);
    }
    if packet.reset || !packet.updates.is_empty() {
        pipe.send(&Message::Preview(packet))?;
    }
    Ok(())
}
fn request_scope(request: &Request) -> io::Result<std::path::PathBuf> {
    use std::os::windows::ffi::OsStringExt;
    let root = if request.fast_mode == FastMode::Mft {
        let units = request
            .scope
            .as_ref()
            .filter(|u| !u.is_empty() && u.len() <= 32768)
            .ok_or_else(|| invalid("MFT 범위 누락"))?;
        std::path::PathBuf::from(OsString::from_wide(units))
    } else {
        if request.scope.is_some() {
            return Err(invalid("기존 NTFS 방식에 폴더 범위 지정 불가"));
        }
        std::path::PathBuf::from(&request.target.root)
    };
    let actual = selected_target(&root, request.fast_mode)?;
    if actual.root != request.target.root || actual.volume != request.target.volume {
        return Err(invalid("분석 범위와 볼륨이 일치하지 않습니다"));
    }
    Ok(root)
}
fn collect_mft<'a>(
    root: &Path,
    target: &Target,
    detailed: bool,
    cancelled: impl Fn() -> bool,
    progress: &mut dyn FnMut(ScanProgress),
    preview: &'a mut crate::scanner::preview::Observer<'a>,
) -> io::Result<ScanResult> {
    let mut source = crate::ntfs_mft::Native::open(target)?;
    // 측정 후보는 별도 선택하며 제품 기본 정책은 검증 후에만 변경.
    let catalog = crate::ntfs_mft::collect_tuned(
        &mut source,
        Default::default(),
        &cancelled,
        &mut *progress,
    )?;
    crate::output::check(&cancelled)?;
    Ok(crate::ntfs_mft::scan_bulk_observed(
        root,
        &catalog,
        detailed,
        cancelled,
        progress,
        Some(preview),
    )?
    .0)
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
        idle: false,
        handle: Handle(h),
        cancelled: &never,
    };
    let mut server = 0;
    if unsafe { GetNamedPipeServerProcessId(pipe.handle.0, &mut server) } == 0 || server != parent {
        return Err(invalid("부모 프로세스가 일치하지 않습니다"));
    }
    loop {
        pipe.idle = true;
        let request: Request = pipe.receive()?;
        pipe.idle = false;
        if request.version != 1 {
            return Err(invalid("지원하지 않는 도우미 버전"));
        }
        if request.watch.is_some() {
            if request.fast_mode != FastMode::Auto || request.scope.is_some() {
                return Err(invalid("변경 추적 요청에 잘못된 수집 옵션"));
            }
            return watch_worker(&mut pipe, &request);
        }
        let scope = request_scope(&request)?;
        let root = scope.as_path();
        let begin = SystemTime::now();
        let clock = Instant::now();
        let disconnected = std::cell::Cell::new(false);
        let mut last = Instant::now() - Duration::from_secs(1);
        let mut last_stage = Stage::Authorizing;
        let shared = std::cell::RefCell::new(&mut pipe);
        let mut update = |counts: ScanProgress| {
            let stage = if counts.folders_scanned == 0 {
                Stage::Enumerating
            } else {
                Stage::Metadata
            };
            if last_stage != stage || last.elapsed() >= Duration::from_millis(100) {
                last_stage = stage;
                if shared
                    .borrow_mut()
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
        };
        let mut observer = |batch| {
            let result = send_preview(&mut shared.borrow_mut(), batch);
            if result.is_err() {
                disconnected.set(true);
            }
            result
        };
        let result = if request.fast_mode == FastMode::Mft {
            collect_mft(
                root,
                &request.target,
                request.detailed,
                || disconnected.get(),
                &mut update,
                &mut observer,
            )
        } else {
            ntfs_native::Native::open(&request.target).and_then(|mut source| {
                crate::ntfs::collect_with_preview(
                    root,
                    request.detailed,
                    &mut source,
                    || disconnected.get(),
                    &mut update,
                    &mut observer,
                )
            })
        };
        match result {
            Ok(result) => {
                let mut record =
                    SavedScan::new(Arc::new(result), begin, SystemTime::now(), clock.elapsed());
                record.engine = Some(Report {
                    requested: Method::Ntfs,
                    actual: Method::Ntfs,
                    fallback: None,
                    fast_mode: Some(request.fast_mode),
                    collector: Some(if request.fast_mode == FastMode::Mft {
                        Collector::MftMixed
                    } else {
                        Collector::NtfsRecords
                    }),
                });
                pipe.send(&Message::Ready)?;
                if request.session {
                    pipe.record(&record)?;
                } else {
                    let mut writer = io::BufWriter::with_capacity(256 * 1024, &mut pipe);
                    crate::snapshot::write_to(&record, &mut writer, || disconnected.get())?;
                    writer.flush()?;
                }
            }
            Err(e) => pipe.send(&Message::Failed(e.to_string()))?,
        }
        if !request.session {
            break;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn selected_response_requires_exact_mode_collector_scope_and_metadata_option() {
        let root = std::env::temp_dir().join(format!(
            "diskweave-selected-response-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let make = || {
            let now = SystemTime::now();
            let mut record = SavedScan::new(
                Arc::new(crate::scanner::scan(&root, || false).unwrap()),
                now,
                now,
                Duration::ZERO,
            );
            record.engine = Some(Report {
                requested: Method::Ntfs,
                actual: Method::Ntfs,
                fast_mode: Some(FastMode::Mft),
                collector: Some(Collector::MftMixed),
                ..Default::default()
            });
            record
        };
        assert!(validate_selected_response(make(), &root, false, FastMode::Mft).is_ok());
        assert!(validate_selected_response(make(), &root, false, FastMode::Legacy).is_err());
        assert!(validate_selected_response(make(), &root, true, FastMode::Mft).is_err());
        assert!(
            validate_selected_response(make(), &root.join("other"), false, FastMode::Mft).is_err()
        );
        let mut wrong = make();
        wrong.engine.as_mut().unwrap().collector = Some(Collector::NtfsRecords);
        assert!(validate_selected_response(wrong, &root, false, FastMode::Mft).is_err());
        assert!(selected_target(&root, FastMode::Auto).is_err());
        assert!(selected_target(&root, FastMode::Mft).is_ok());
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn persistent_response_stops_at_its_frame_without_waiting_for_process_exit() {
        let root =
            std::env::temp_dir().join(format!("diskweave-helper-frame-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("known"), b"123").unwrap();
        let now = SystemTime::now();
        let mut record = SavedScan::new(
            Arc::new(crate::scanner::scan(&root, || false).unwrap()),
            now,
            now,
            Duration::ZERO,
        );
        record.engine = Some(Report {
            requested: Method::Ntfs,
            actual: Method::Ntfs,
            fallback: None,
            ..Default::default()
        });
        let mut wire = Vec::new();
        write_completed(&mut wire, &record).unwrap();
        write_completed(&mut wire, &record).unwrap();
        wire.extend_from_slice(b"NEXT");
        let mut reader = wire.as_slice();
        for _ in 0..2 {
            let result = read_completed(&mut reader, &root, false).unwrap();
            assert_eq!(
                (result.folders[0].logical_bytes, result.files.len()),
                (3, 1)
            );
        }
        assert_eq!(reader, b"NEXT");
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn cancelled_transport_returns_without_retrying_the_read() {
        let (done, wait) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut reader = null_mut();
            let mut writer = null_mut();
            assert_ne!(
                unsafe { CreatePipe(&mut reader, &mut writer, null(), 0) },
                0
            );
            let _writer = Handle(writer);
            let mut pipe = Pipe {
                idle: false,
                handle: Handle(reader),
                cancelled: &|| true,
            };
            let result = pipe.read_exact(&mut [0u8; 4]);
            done.send(result.map_err(|e| e.kind())).unwrap();
        });
        assert_eq!(
            wait.recv_timeout(Duration::from_secs(1)),
            Ok(Err(io::ErrorKind::ConnectionAborted))
        );
    }
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
                ..Default::default()
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
