use crate::{
    metadata::{Extra, Field, Identity},
    scanner::{FileEntry, Folder, ScanResult},
    snapshot::SavedScan,
    usn::{Checkpoint, Event, Index, Journal, Source},
};
use std::{
    fs,
    io::{self, Read, Write},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, SyncSender},
        Arc,
    },
    time::{Duration, Instant, SystemTime},
};
const BUFFER: usize = 65536;
fn cpu() -> io::Result<f64> {
    use windows_sys::Win32::{
        Foundation::FILETIME,
        System::Threading::{GetCurrentProcess, GetProcessTimes},
    };
    let mut times = [FILETIME {
        dwLowDateTime: 0,
        dwHighDateTime: 0,
    }; 4];
    let [created, exited, kernel, user] = &mut times;
    if unsafe { GetProcessTimes(GetCurrentProcess(), created, exited, kernel, user) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(times[2..]
        .iter()
        .map(|t| ((t.dwHighDateTime as u64) << 32) | t.dwLowDateTime as u64)
        .sum::<u64>() as f64
        / 10_000_000.0)
}
fn working_set() -> io::Result<u64> {
    use windows_sys::Win32::System::{
        ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS},
        Threading::GetCurrentProcess,
    };
    let mut info: PROCESS_MEMORY_COUNTERS = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of_val(&info) as u32;
    if unsafe { GetProcessMemoryInfo(GetCurrentProcess(), &mut info, size) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(info.WorkingSetSize as u64)
}
#[derive(serde::Serialize)]
struct Cost {
    seconds: f64,
    cpu_seconds: f64,
    peak_working_set_sample: u64,
    receive_seconds: Option<f64>,
    rebuild_seconds: Option<f64>,
}
fn measured<T>(run: impl FnOnce() -> io::Result<T>) -> io::Result<(T, Cost)> {
    let stop = AtomicBool::new(false);
    let initial = cpu()?;
    let started = Instant::now();
    std::thread::scope(|scope| {
        let sampler = scope.spawn(|| {
            let mut peak = working_set()?;
            while !stop.load(Ordering::Relaxed) {
                peak = peak.max(working_set()?);
                std::thread::sleep(Duration::from_millis(25));
            }
            Ok::<_, io::Error>(peak)
        });
        let result = run();
        let seconds = started.elapsed().as_secs_f64();
        let used = cpu();
        stop.store(true, Ordering::Relaxed);
        let peak = sampler
            .join()
            .map_err(|_| io::Error::other("메모리 표본 시험 실패"))??;
        Ok((
            result?,
            Cost {
                seconds,
                cpu_seconds: used? - initial,
                peak_working_set_sample: peak,
                receive_seconds: None,
                rebuild_seconds: None,
            },
        ))
    })
}
struct Writer {
    tx: SyncSender<Vec<u8>>,
    buffer: Vec<u8>,
    bytes: u64,
}
impl Write for Writer {
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        let n = b.len().min(BUFFER - self.buffer.len());
        self.buffer.extend_from_slice(&b[..n]);
        self.bytes += n as u64;
        if self.buffer.len() == BUFFER {
            self.flush()?;
        }
        Ok(n)
    }
    fn flush(&mut self) -> io::Result<()> {
        if !self.buffer.is_empty() {
            let bytes = std::mem::replace(&mut self.buffer, Vec::with_capacity(BUFFER));
            self.tx
                .send(bytes)
                .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "시험 수신기 종료"))?;
        }
        Ok(())
    }
}
struct Reader {
    rx: Receiver<Vec<u8>>,
    buffer: io::Cursor<Vec<u8>>,
}
impl Read for Reader {
    fn read(&mut self, b: &mut [u8]) -> io::Result<usize> {
        if b.is_empty() {
            return Ok(0);
        }
        if self.buffer.position() == self.buffer.get_ref().len() as u64 {
            match self.rx.recv() {
                Ok(bytes) => self.buffer = io::Cursor::new(bytes),
                Err(_) => return Ok(0),
            }
        }
        self.buffer.read(b)
    }
}
fn channel() -> (Writer, Reader) {
    let (tx, rx) = mpsc::sync_channel(4);
    (
        Writer {
            tx,
            buffer: Vec::with_capacity(BUFFER),
            bytes: 0,
        },
        Reader {
            rx,
            buffer: io::Cursor::new(vec![]),
        },
    )
}
fn legacy(record: &SavedScan) -> io::Result<(SavedScan, u64, Cost)> {
    let (mut writer, mut reader) = channel();
    let ((result, bytes), cost) = measured(|| {
        std::thread::scope(|scope| {
            let task = scope.spawn(move || {
                crate::snapshot::write_to(record, &mut writer, || false)?;
                writer.flush()?;
                Ok::<_, io::Error>(writer.bytes)
            });
            let result = crate::snapshot::read_from(&mut reader, || false);
            drop(reader);
            let sent = task
                .join()
                .map_err(|_| io::Error::other("시험 작성기 실패"))?;
            let bytes = sent?;
            Ok((result?, bytes))
        })
    })?;
    Ok((result, bytes, cost))
}
fn transfer(
    sender: &mut crate::usn::stream::Sender,
    index: &mut Index,
    receiver: &mut crate::usn::stream::Receiver,
) -> io::Result<(u64, Cost)> {
    let (mut writer, mut reader) = channel();
    let ((bytes, receive, rebuild), mut cost) = measured(|| {
        std::thread::scope(|scope| {
            let task = scope.spawn(move || sender.write(&mut writer, index, &|| false));
            let started = Instant::now();
            let result = receiver.receive(&mut reader, &|| false);
            let receive = started.elapsed().as_secs_f64();
            drop(reader);
            let sent = task
                .join()
                .map_err(|_| io::Error::other("시험 작성기 실패"))?;
            let stats = sent?;
            result?;
            let started = Instant::now();
            let _complete = receiver.snapshot(&|| false)?;
            Ok((stats.bytes, receive, started.elapsed().as_secs_f64()))
        })
    })?;
    cost.receive_seconds = Some(receive);
    cost.rebuild_seconds = Some(rebuild);
    Ok((bytes, cost))
}
fn cancellation(index: &mut Index, root: &Path) -> io::Result<f64> {
    let stop = AtomicBool::new(false);
    let (mut writer, reader) = channel();
    struct Cancelling<'a> {
        reader: Reader,
        stop: &'a AtomicBool,
        bytes: usize,
        started: Option<Instant>,
    }
    impl Read for Cancelling<'_> {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            let n = self.reader.read(buffer)?;
            self.bytes += n;
            if self.bytes >= BUFFER * 4 && self.started.is_none() {
                self.started = Some(Instant::now());
                self.stop.store(true, Ordering::Relaxed);
            }
            Ok(n)
        }
    }
    let mut reader = Cancelling {
        reader,
        stop: &stop,
        bytes: 0,
        started: None,
    };
    let mut sender = crate::usn::stream::Sender::new(19, Default::default());
    let mut receiver = crate::usn::stream::Receiver::new(root.to_owned(), true, Default::default());
    std::thread::scope(|scope| {
        let task =
            scope.spawn(|| sender.write(&mut writer, index, &|| stop.load(Ordering::Relaxed)));
        let received = receiver.receive(&mut reader, &|| stop.load(Ordering::Relaxed));
        let started = reader
            .started
            .ok_or_else(|| io::Error::other("시험 취소 경계 미도달"))?;
        drop(reader);
        assert!(task
            .join()
            .map_err(|_| io::Error::other("취소 작성기 실패"))?
            .is_err());
        assert!(received.is_err());
        assert!(receiver.snapshot(&|| false).is_err());
        Ok(started.elapsed().as_secs_f64() * 1000.0)
    })
}
fn name(i: usize, width: usize) -> String {
    format!("entry-{i:08}-{}", "x".repeat(width.saturating_sub(15)))
}
fn extra(id: u64) -> Extra {
    let mut file = [0; 16];
    file[..8].copy_from_slice(&id.to_le_bytes());
    Extra {
        allocation: Field::Value(4096),
        identity: Field::Value(Identity { volume: 7, file }),
        ..Default::default()
    }
}
fn baseline(count: usize, width: usize) -> io::Result<SavedScan> {
    let root = PathBuf::from("C:\\DiskWeaveSynthetic");
    let time = SystemTime::UNIX_EPOCH + Duration::from_secs(1000);
    let mut scan = ScanResult {
        detailed: true,
        folders: vec![Folder {
            access: Default::default(),
            path: root,
            parent: None,
            logical_bytes: count as u64 * 8,
            file_count: count as u64,
            folder_count: 0,
            allocation: Default::default(),
            created: Some(time),
            modified: Some(time),
            attributes: 16,
        }],
        files: (0..count)
            .map(|i| FileEntry {
                name: name(i, width).into(),
                parent: 0,
                logical_bytes: 8,
                created: Some(time),
                modified: Some(time),
                attributes: 32,
                extra: extra(i as u64 + 2),
            })
            .collect(),
        children: vec![],
        file_children: vec![],
        error_count: 0,
        issues: vec![],
        metadata_error_count: 0,
        metadata_issues: vec![],
        skipped_reparse_points: 0,
    };
    for file in &scan.files {
        scan.folders[0].allocation.add(file.extra.allocation)?;
    }
    crate::scanner::finish(&mut scan)?;
    let mut record = SavedScan::new(Arc::new(scan), time, time, Duration::ZERO);
    record.tracking = Some(Checkpoint {
        version: 3,
        volume: "synthetic".into(),
        root: 1,
        journal: 7,
        next: 100,
        folders: vec![1],
        files: (2..count as u64 + 2).collect(),
        excluded: vec![],
        pending: vec![],
        unavailable: vec![],
        confirmed_after: Some(time),
    });
    Ok(record)
}
struct Synthetic {
    name: String,
    time: SystemTime,
}
impl Source for Synthetic {
    fn journal(&mut self) -> io::Result<Journal> {
        Ok(Journal {
            volume: "synthetic".into(),
            id: 7,
            first: 0,
            lowest: 0,
            next: 108,
        })
    }
    fn read(&mut self, start: i64, _: i64) -> io::Result<Vec<Event>> {
        Ok(if start == 100 {
            vec![Event {
                usn: 100,
                id: 2,
                parent: 1,
                reason: 0x80000001,
            }]
        } else {
            vec![]
        })
    }
    fn inspect(&mut self, path: &Path, _: bool) -> io::Result<crate::ntfs::Info> {
        let file = path.file_name().is_some_and(|n| n == self.name.as_str());
        Ok(crate::ntfs::Info {
            resolved: Some(path.to_owned()),
            reference: Some(crate::ntfs::Id::V2(if file { 2 } else { 1 })),
            bytes: if file { 17 } else { 0 },
            attributes: if file { 32 } else { 16 },
            created: Some(self.time),
            modified: Some(self.time),
            extra: crate::metadata::Report {
                values: if file { extra(2) } else { Default::default() },
                issue: None,
            },
        })
    }
    fn list(&mut self, _: &Path) -> io::Result<Vec<PathBuf>> {
        Err(io::Error::other("변경 파일만 조회해야 하는 합성 시험"))
    }
}
pub fn run(count: &str, width: &str, output: &Path) -> io::Result<()> {
    let count: usize = count
        .parse()
        .map_err(|_| io::Error::other("항목 수 형식 오류"))?;
    let width: usize = width
        .parse()
        .map_err(|_| io::Error::other("이름 폭 형식 오류"))?;
    if count == 0 || count > 1_000_000 || !(15..=240).contains(&width) {
        return Err(io::Error::other("합성 시험 범위 초과"));
    }
    let owned = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/stream-benchmark");
    if output.parent().map(fs::canonicalize).transpose()? != Some(fs::canonicalize(owned)?) {
        return Err(io::Error::other(
            "합성 결과는 지정한 시험 폴더에만 저장 가능",
        ));
    }
    let record = baseline(count, width)?;
    let root = record.scan.folders[0].path.clone();
    let mut results = vec![];
    let repetitions = if width > 200 { 1 } else { 3 };
    for round in 0..repetitions {
        let mut index = Index::restore(&record)?;
        let mut sender = crate::usn::stream::Sender::new(17, Default::default());
        let mut receiver =
            crate::usn::stream::Receiver::new(root.clone(), true, Default::default());
        let mut old = None;
        let mut new = None;
        for kind in if round % 2 == 0 { [0, 1] } else { [1, 0] } {
            if kind == 0 {
                old = Some(match legacy(&record) {
                    Ok((_, bytes, cost)) => {
                        serde_json::json!({"ok":true,"bytes":bytes,"cost":cost})
                    }
                    Err(e) => serde_json::json!({"ok":false,"error":e.to_string()}),
                });
            } else {
                new = Some(transfer(&mut sender, &mut index, &mut receiver)?);
            }
        }
        let initial = receiver.snapshot(&|| false)?;
        let first_key = initial.entries.as_ref().unwrap().path_key(Path::new(&name(0, width))).unwrap();
        assert_eq!(initial.scan.files.len(), count);
        assert_eq!(initial.scan.folders[0].logical_bytes, count as u64 * 8);
        let mut source = Synthetic {
            name: name(0, width),
            time: SystemTime::UNIX_EPOCH + Duration::from_secs(1000),
        };
        let changed = Instant::now();
        index.refresh(&mut source, &|| false)?;
        let refresh = changed.elapsed().as_secs_f64();
        let mut old_update = None;
        let mut new_update = None;
        for kind in if round % 2 == 0 { [0, 1] } else { [1, 0] } {
            if kind == 0 {
                old_update = Some(match legacy(&index.snapshot(&|| false)?) {
                    Ok((_, bytes, cost)) => {
                        serde_json::json!({"ok":true,"bytes":bytes,"cost":cost})
                    }
                    Err(e) => serde_json::json!({"ok":false,"error":e.to_string()}),
                });
            } else {
                new_update = Some(transfer(&mut sender, &mut index, &mut receiver)?);
            }
        }
        let (delta_bytes, delta_cost) = new_update.unwrap();
        let result = receiver.snapshot(&|| false)?;
        assert_eq!(result.entries.as_ref().unwrap().path_key(Path::new(&name(0, width))), Some(first_key));
        let identities = result.entries.as_ref().unwrap();
        let queried = Instant::now();
        for row in 0..count {
            let item = crate::explorer::Item::File(row);
            let key = std::hint::black_box(identities.key(item).unwrap());
            assert_eq!(identities.item(key), Some(item));
        }
        let id_lookup_seconds = queried.elapsed().as_secs_f64();
        assert_eq!(result.scan.folders[0].logical_bytes, count as u64 * 8 + 9);
        result.tracking.as_ref().unwrap().validate(&result.scan)?;
        let (bytes, cost) = new.unwrap();
        results.push(serde_json::json!({"round":round,"old_initial":old,"new_initial":{"ok":true,"bytes":bytes,"cost":cost},"old_changed_full":old_update,"new_changed_delta":{"ok":true,"bytes":delta_bytes,"cost":delta_cost},"candidate_refresh_seconds":refresh,"id_roundtrip_queries":count,"id_lookup_seconds":id_lookup_seconds,"changed_entry_id_preserved":true,"files":count,"expected_logical_bytes":count as u64*8+9}));
    }
    let cancel_ms = cancellation(&mut Index::restore(&record)?, &root)?;
    let report = serde_json::json!({"count":count,"name_width":width,"bounded_channel_bytes":4*BUFFER,"frame_limit_bytes":1024*1024,"cancel_ms":cancel_ms,"note":"같은 프로세스의 제한된 채널에서 전송과 부모 결과 구성을 측정함. CPU는 프로세스 시간 차이, RAM은 해당 단계에서 이미 유지 중인 원본/인덱스/이전 결과를 포함한 25ms 간격 working set 표본 최대값임. 실제 관리자 IPC/UAC·OS 수집·GUI 게시 시간은 포함하지 않음. 재구성·원본 수집의 전체 메모리 상한을 의미하지 않음.","results":results});
    serde_json::to_writer(fs::File::create(output)?, &report).map_err(crate::output::json_error)?;
    Ok(())
}
