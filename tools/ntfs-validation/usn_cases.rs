use crate::{snapshot, usn, usn_native};
use std::os::windows::{
    ffi::OsStrExt,
    io::{AsRawHandle, FromRawHandle},
};
use std::{
    fs, io,
    path::Path,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};
use windows_sys::Win32::{
    Foundation::*,
    Storage::FileSystem::*,
    System::{Ioctl::*, IO::DeviceIoControl},
};
fn journal(target: &crate::ntfs_native::Target, replace: bool) -> io::Result<()> {
    let device: Vec<u16> = target
        .volume
        .trim_end_matches('\\')
        .encode_utf16()
        .chain(Some(0))
        .collect();
    // 호출 전 새 VHD의 GUID·소유 표식 검증을 마친 검증기에서만 사용.
    let raw = unsafe {
        CreateFileW(
            device.as_ptr(),
            GENERIC_READ | GENERIC_WRITE,
            7,
            std::ptr::null(),
            OPEN_EXISTING,
            0,
            std::ptr::null_mut(),
        )
    };
    if raw == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    let f = unsafe { fs::File::from_raw_handle(raw) };
    let mut bytes = 0;
    if replace {
        let mut state: USN_JOURNAL_DATA_V0 = unsafe { std::mem::zeroed() };
        if unsafe {
            DeviceIoControl(
                f.as_raw_handle(),
                FSCTL_QUERY_USN_JOURNAL,
                std::ptr::null(),
                0,
                (&mut state as *mut USN_JOURNAL_DATA_V0).cast(),
                std::mem::size_of_val(&state) as u32,
                &mut bytes,
                std::ptr::null_mut(),
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        let deletion = DELETE_USN_JOURNAL_DATA {
            UsnJournalID: state.UsnJournalID,
            DeleteFlags: 3,
        };
        if unsafe {
            DeviceIoControl(
                f.as_raw_handle(),
                FSCTL_DELETE_USN_JOURNAL,
                (&deletion as *const DELETE_USN_JOURNAL_DATA).cast(),
                std::mem::size_of_val(&deletion) as u32,
                std::ptr::null_mut(),
                0,
                &mut bytes,
                std::ptr::null_mut(),
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
    }
    let create = CREATE_USN_JOURNAL_DATA {
        MaximumSize: 2 * 1024 * 1024,
        AllocationDelta: 512 * 1024,
    };
    if unsafe {
        DeviceIoControl(
            f.as_raw_handle(),
            FSCTL_CREATE_USN_JOURNAL,
            (&create as *const CREATE_USN_JOURNAL_DATA).cast(),
            std::mem::size_of_val(&create) as u32,
            std::ptr::null_mut(),
            0,
            &mut bytes,
            std::ptr::null_mut(),
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}
fn compare(
    index: &mut usn::Index,
    source: &mut usn_native::Native,
    root: &Path,
    label: &str,
) -> io::Result<serde_json::Value> {
    let start = Instant::now();
    index.refresh(source, &|| false)?;
    let record = index.snapshot(&|| false)?;
    let reference = crate::scanner::scan_with_options(root, true, || false, |_| {})?;
    if super::rows(&record.scan) != super::rows(&reference)
        || record.scan.skipped_reparse_points != reference.skipped_reparse_points
    {
        return Err(io::Error::other(format!("변경 반영 대조 불일치: {label}")));
    }
    Ok(
        serde_json::json!({"case":label,"files":reference.files.len(),"bytes":reference.folders[0].logical_bytes,"ms":start.elapsed().as_secs_f64()*1000.0}),
    )
}
struct ReplaceDuringRead<'a> {
    source: &'a mut usn_native::Native,
    replace: Option<(std::path::PathBuf, std::path::PathBuf)>,
}
impl usn::Source for ReplaceDuringRead<'_> {
    fn journal(&mut self) -> io::Result<usn::Journal> {
        usn::Source::journal(self.source)
    }
    fn read(&mut self, start: i64, end: i64) -> io::Result<Vec<usn::Event>> {
        let events = usn::Source::read(self.source, start, end)?;
        if let Some((path, previous)) = self.replace.take() {
            fs::rename(&path, previous)?;
            fs::write(path, b"123")?;
        }
        Ok(events)
    }
    fn inspect(&mut self, path: &Path, detailed: bool) -> io::Result<crate::ntfs::Info> {
        usn::Source::inspect(self.source, path, detailed)
    }
    fn list(&mut self, path: &Path) -> io::Result<Vec<std::path::PathBuf>> {
        usn::Source::list(self.source, path)
    }
}
pub fn run(root: &Path, target: &crate::ntfs_native::Target) -> io::Result<()> {
    let mut total = 0;
    let path: Vec<u16> = root.as_os_str().encode_wide().chain(Some(0)).collect();
    if unsafe {
        GetDiskFreeSpaceExW(
            path.as_ptr(),
            std::ptr::null_mut(),
            &mut total,
            std::ptr::null_mut(),
        )
    } == 0
        || total > 300 * 1024 * 1024
    {
        return Err(io::Error::other("새 소형 시험 볼륨 조건 불일치"));
    }
    journal(target, false)?;
    eprintln!("USN: 전체 드라이브 기준 수집");
    let mut whole_source = usn_native::Native::open(root)?;
    let mut whole = usn::Index::begin(root, true, &mut whole_source, &|| false)?;
    let whole_record = whole.snapshot(&|| false)?;
    let whole_reference = crate::scanner::scan_with_options(root, true, || false, |_| {})?;
    let protected_path = root.join("System Volume Information");
    if whole_reference
        .issues
        .iter()
        .any(|e| e.path != protected_path)
    {
        return Err(io::Error::other("일반 기준 수집의 예상하지 못한 오류"));
    }
    // 보호 폴더 내부는 일반 열거 권한과 다르므로 별도 존재 검증. 나머지 경로는 전체 대조.
    let visible = |rows: std::collections::BTreeMap<std::path::PathBuf, super::Row>| {
        rows.into_iter()
            .filter(|(p, _)| {
                !p.as_os_str().is_empty() && !p.starts_with("System Volume Information")
            })
            .collect::<std::collections::BTreeMap<_, _>>()
    };
    if visible(super::rows(&whole_record.scan)) != visible(super::rows(&whole_reference)) {
        return Err(io::Error::other(
            "전체 드라이브의 일반 접근 가능 경로 대조 불일치",
        ));
    }
    let protected = root.join("System Volume Information");
    if !whole_record
        .scan
        .folders
        .iter()
        .any(|f| f.path == protected)
    {
        return Err(io::Error::other(
            "시험 볼륨의 보호 폴더가 수집되지 않았습니다",
        ));
    }
    let ordinary_access = fs::read_dir(&protected).is_ok();
    let mut cases = vec![
        serde_json::json!({"case":"whole-drive-baseline","matched_visible_rows":true,"protected_contents_compared":false,"protected_collected":true,"protected_standard_access":ordinary_access}),
    ];
    eprintln!("USN: 전체 드라이브·보호 폴더 일치");
    drop(whole_source);
    let scope = root.join("Cases");
    let mut source = usn_native::Native::open(&scope)?;
    let mut index = usn::Index::begin(&scope, true, &mut source, &|| false)?;
    cases.push(compare(&mut index, &mut source, &scope, "baseline")?);
    let replacement = scope.join("replace-save.bin");
    fs::write(&replacement, b"12345")?;
    cases.push(compare(
        &mut index,
        &mut source,
        &scope,
        "replacement-created",
    )?);
    fs::write(&replacement, b"123456")?;
    let mut racing = ReplaceDuringRead {
        source: &mut source,
        replace: Some((replacement, root.join("previous-save.bin"))),
    };
    index.refresh(&mut racing, &|| false)?;
    let replaced = index.snapshot(&|| false)?;
    let reference = crate::scanner::scan_with_options(&scope, true, || false, |_| {})?;
    if super::rows(&replaced.scan) != super::rows(&reference) {
        return Err(io::Error::other(
            "실제 저널 읽기 이후 파일 교체 대조 불일치",
        ));
    }
    cases.push(serde_json::json!({"case":"replacement-after-usn-read","matched":true}));
    fs::hard_link(scope.join("regular.bin"), root.join("external-alias"))?;
    fs::write(root.join("external-alias"), b"123456789")?;
    cases.push(compare(
        &mut index,
        &mut source,
        &scope,
        "external-hardlink-write",
    )?);
    fs::rename(scope.join("empty"), scope.join("renamed"))?;
    cases.push(compare(&mut index, &mut source, &scope, "file-rename")?);
    fs::create_dir_all(root.join("incoming/deep"))?;
    fs::write(root.join("incoming/deep/payload"), b"1234567")?;
    fs::rename(root.join("incoming"), scope.join("incoming"))?;
    cases.push(compare(
        &mut index,
        &mut source,
        &scope,
        "incoming-subtree",
    )?);
    fs::rename(scope.join("incoming"), scope.join("renamed-tree"))?;
    cases.push(compare(&mut index, &mut source, &scope, "folder-rename")?);
    fs::rename(scope.join("renamed-tree"), root.join("outgoing"))?;
    cases.push(compare(
        &mut index,
        &mut source,
        &scope,
        "outgoing-subtree",
    )?);
    fs::remove_file(scope.join("sub/alias.bin"))?;
    fs::remove_dir(scope.join("loop"))?;
    cases.push(compare(
        &mut index,
        &mut source,
        &scope,
        "delete-link-and-reparse",
    )?);
    let record = index.snapshot(&|| false)?;
    let file = root.join("resume.dwsnap");
    snapshot::save(&record, &file, false, || false)?;
    let loaded = snapshot::load(&file, || false)?;
    let mut resumed = usn::Index::restore(&loaded)?;
    fs::write(scope.join("after-restart"), b"12345")?;
    cases.push(compare(
        &mut resumed,
        &mut source,
        &scope,
        "restart-catchup",
    )?);
    let output = Arc::new(Mutex::new(None));
    eprintln!("USN: 도우미 재시작·반영·진행·취소");
    let progress = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let worker_progress = Arc::clone(&progress);
    let worker_output = Arc::clone(&output);
    let stop = Arc::new(AtomicBool::new(false));
    let worker_stop = Arc::clone(&stop);
    let watch_root = scope.clone();
    let worker = std::thread::spawn(move || {
        crate::ntfs_helper::watch(
            &watch_root,
            true,
            Some(Arc::new(loaded)),
            &|| worker_stop.load(Ordering::Relaxed),
            |record| *worker_output.lock().unwrap() = Some(record),
            |_| {
                worker_progress.fetch_add(1, Ordering::Relaxed);
            },
        )
    });
    let until = Instant::now() + Duration::from_secs(20);
    while output.lock().unwrap().is_none() {
        if Instant::now() > until {
            stop.store(true, Ordering::Relaxed);
            let result = worker.join().unwrap();
            return Err(io::Error::other(format!(
                "도우미 최초 응답 실패: {result:?}"
            )));
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    fs::write(scope.join("helper-update"), b"12345678901234567")?;
    loop {
        if output
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|r: &snapshot::SavedScan| {
                r.scan.files.iter().any(|f| f.name == "helper-update")
            })
        {
            break;
        }
        if Instant::now() > until {
            stop.store(true, Ordering::Relaxed);
            worker.join().unwrap().err();
            return Err(io::Error::other("도우미 변경 응답 시간 초과"));
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let latest = output.lock().unwrap().take().unwrap();
    let basic = crate::scanner::scan_with_options(&scope, true, || false, |_| {})?;
    if super::rows(&latest.scan) != super::rows(&basic) {
        return Err(io::Error::other("도우미 결과 불일치"));
    }
    stop.store(true, Ordering::Relaxed);
    let cancellation_started = Instant::now();
    while !worker.is_finished() {
        if cancellation_started.elapsed() > Duration::from_secs(3) {
            return Err(io::Error::other(
                "도우미 취소가 3초 안에 완료되지 않았습니다",
            ));
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let cancelled = worker.join().unwrap();
    if cancelled.is_ok() {
        return Err(io::Error::other("도우미 취소 누락"));
    }
    if progress.load(Ordering::Relaxed) == 0 {
        return Err(io::Error::other("도우미 진행 응답 누락"));
    }
    cases.push(serde_json::json!({"case":"helper-live-and-cancel","matched":true,"progress_events":progress.load(Ordering::Relaxed),"cancel_ms":cancellation_started.elapsed().as_secs_f64()*1000.0}));
    eprintln!("USN: 도우미 중지 완료, 상위 경로·루트·저널 교체 검사");
    let parent = root.join("parent-guard");
    fs::create_dir_all(parent.join("scope"))?;
    let protected = parent.join("scope");
    let mut guard_source = usn_native::Native::open(&protected)?;
    let mut guard_index = usn::Index::begin(&protected, false, &mut guard_source, &|| false)?;
    fs::rename(&parent, root.join("parent-renamed"))?;
    fs::rename(root.join("parent-renamed"), &parent)?;
    if guard_index.refresh(&mut guard_source, &|| false).is_ok() {
        return Err(io::Error::other("상위 폴더 이동 기록 검출 실패"));
    }
    cases.push(serde_json::json!({"case":"ancestor-moved-and-returned","rejected":true}));
    fs::rename(&scope, root.join("moved-root"))?;
    fs::create_dir(&scope)?;
    if resumed.refresh(&mut source, &|| false).is_ok() {
        return Err(io::Error::other("교체된 루트가 수락됨"));
    }
    drop(source);
    let moved = root.join("moved-root");
    let mut source = usn_native::Native::open(&moved)?;
    let mut index = usn::Index::begin(&moved, true, &mut source, &|| false)?;
    let checkpoint = index.checkpoint.clone();
    journal(target, true)?;
    if index.refresh(&mut source, &|| false).is_ok() || index.checkpoint != checkpoint {
        return Err(io::Error::other("저널 교체 검출 실패"));
    }
    cases.push(serde_json::json!({"case":"root-and-journal-replacement","rejected":true}));
    println!(
        "{}",
        serde_json::json!({"verified":"새 NTFS 시험 볼륨의 실제 USN·도우미·재시작·교체 대조","cases":cases})
    );
    Ok(())
}
