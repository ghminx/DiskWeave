use super::profile::{cpu_scope, scope, thread_cpu, Stage, Trace};
use super::*;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc::{self, SyncSender},
    Arc,
};
use std::time::{Duration, Instant};
const BATCH: usize = 128;
pub(super) struct Collection<'a> {
    pub strategy: directory::Strategy,
    pub metrics: Option<&'a ScanMetrics>,
}
enum Task {
    Folder(usize, PathBuf),
    Metadata(Vec<(usize, PathBuf)>),
}
type Entry = Result<directory::Entry, (PathBuf, io::Error)>;
enum Event {
    Entries(usize, Vec<Entry>),
    FolderDone(usize, usize, io::Result<()>, Option<directory::Entry>),
    Metadata(usize, Vec<(usize, crate::metadata::Report)>),
    Failed,
}
fn send(tx: &SyncSender<Event>, event: Event, trace: Option<&Trace>) -> bool {
    let _send = scope(trace, Stage::ResultSend);
    tx.send(event).is_ok()
}
fn folder(
    id: usize,
    index: usize,
    path: PathBuf,
    stop: &AtomicBool,
    tx: &SyncSender<Event>,
    trace: Option<&Trace>,
) {
    let checking = scope(trace, Stage::FolderCheck);
    let current = fs::symlink_metadata(&path).and_then(|m| {
        if m.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 || !m.is_dir() {
            Err(io::Error::other(
                "열거할 폴더의 종류가 변경되었거나 Reparse Point임",
            ))
        } else {
            Ok(m)
        }
    });
    drop(checking);
    if let Err(error) = current {
        send(tx, Event::FolderDone(id, index, Err(error), None), trace);
        return;
    }
    let _enumeration = scope(trace, Stage::Enumerate);
    let entries = match fs::read_dir(&path) {
        Ok(v) => v,
        Err(e) => {
            send(tx, Event::FolderDone(id, index, Err(e), None), trace);
            return;
        }
    };
    let mut batch = Vec::with_capacity(BATCH);
    for entry in entries {
        if stop.load(Ordering::Relaxed) {
            return;
        }
        let item = match entry {
            Ok(entry) => {
                let path = entry.path();
                let metadata = entry.metadata().and_then(|m| {
                    if m.is_dir() && m.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT == 0 {
                        fs::symlink_metadata(&path)
                    } else {
                        Ok(m)
                    }
                });
                match metadata {
                    Ok(metadata) => Ok((path, metadata)),
                    Err(error) => Err((path, error)),
                }
            }
            Err(e) => {
                batch.push(Err((path.clone(), e)));
                break;
            }
        };
        batch.push(item.map(|(path, m)| directory::Entry::from_metadata(path, m)));
        if batch.len() == BATCH
            && !send(
                tx,
                Event::Entries(
                    index,
                    std::mem::replace(&mut batch, Vec::with_capacity(BATCH)),
                ),
                trace,
            )
        {
            return;
        }
    }
    if !batch.is_empty() && !send(tx, Event::Entries(index, batch), trace) {
        return;
    }
    send(tx, Event::FolderDone(id, index, Ok(()), None), trace);
}
pub(super) fn scan(
    root: &Path,
    detailed: bool,
    count: usize,
    cancelled: impl Fn() -> bool,
    progress: impl FnMut(ScanProgress),
    probe: &(impl Fn(&Path) -> crate::metadata::Report + Sync),
) -> io::Result<ScanResult> {
    scan_collected(
        root,
        detailed,
        count,
        Collection {
            strategy: directory::Strategy::Reference,
            metrics: None,
        },
        cancelled,
        progress,
        probe,
    )
}
pub(super) fn scan_collected(
    root: &Path,
    detailed: bool,
    count: usize,
    collection: Collection<'_>,
    cancelled: impl Fn() -> bool,
    mut progress: impl FnMut(ScanProgress),
    probe: &(impl Fn(&Path) -> crate::metadata::Report + Sync),
) -> io::Result<ScanResult> {
    let Collection { strategy, metrics } = collection;
    let trace = metrics.map(|m| &m.trace);
    let mut result = empty_result(root, detailed)?;
    let stop = Arc::new(AtomicBool::new(false));
    std::thread::scope(|threads| {
        let (tx, rx) = mpsc::sync_channel(count * 2);
        let mut senders = vec![];
        let mut handles = vec![];
        for id in 0..count {
            let (tasks, work) = mpsc::sync_channel(1);
            senders.push(tasks);
            let tx = tx.clone();
            let stop = Arc::clone(&stop);
            handles.push(threads.spawn(move || {
                let _cpu = cpu_scope(trace, true, thread_cpu);
                let run = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    while !stop.load(Ordering::Relaxed) {
                        let waiting = scope(trace, Stage::WorkerIdle);
                        let task = match work.recv_timeout(Duration::from_millis(20)) {
                            Ok(v) => v,
                            Err(mpsc::RecvTimeoutError::Timeout) => continue,
                            Err(_) => break,
                        };
                        drop(waiting);
                        match task {
                            Task::Folder(index, path) => {
                                let clock = metrics.map(|_| Instant::now());
                                if strategy == directory::Strategy::Reference {
                                    folder(id, index, path, &stop, &tx, trace);
                                } else {
                                    let checking = scope(trace, Stage::FolderCheck);
                                    let current = fs::symlink_metadata(&path).and_then(|m| {
                                        if m.is_dir()
                                            && m.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT
                                                == 0
                                        {
                                            Ok(directory::Entry::from_metadata(path.clone(), m))
                                        } else {
                                            Err(io::Error::other(
                                                "열거 대상의 종류가 변경되었습니다",
                                            ))
                                        }
                                    });
                                    drop(checking);
                                    let _enumeration = scope(trace, Stage::Enumerate);
                                    let mut observed = None;
                                    let value = current.and_then(|entry| {
                                        observed = Some(entry);
                                        directory::collect(
                                            &path,
                                            strategy,
                                            &|| stop.load(Ordering::Relaxed),
                                            &mut |entries| {
                                                if send(&tx, Event::Entries(index, entries), trace)
                                                {
                                                    Ok(())
                                                } else {
                                                    Err(io::Error::new(
                                                        io::ErrorKind::BrokenPipe,
                                                        "열거 결과 전달 중단",
                                                    ))
                                                }
                                            },
                                        )
                                    });
                                    send(&tx, Event::FolderDone(id, index, value, observed), trace);
                                }
                                if let (Some(metrics), Some(clock)) = (metrics, clock) {
                                    metrics.enumeration_ns.fetch_add(
                                        clock.elapsed().as_nanos() as u64,
                                        Ordering::Relaxed,
                                    );
                                }
                            }
                            Task::Metadata(paths) => {
                                let _metadata = scope(trace, Stage::MetadataTask);
                                let mut reports = Vec::with_capacity(paths.len());
                                for (index, path) in paths {
                                    if stop.load(Ordering::Relaxed) {
                                        return;
                                    }
                                    reports.push((index, probe(&path)));
                                }
                                if !send(&tx, Event::Metadata(id, reports), trace) {
                                    return;
                                }
                            }
                        }
                    }
                }));
                if run.is_err() {
                    send(&tx, Event::Failed, trace);
                }
            }));
        }
        drop(tx);
        let outcome = (|| {
            let mut idle = vec![true; count];
            // 대기 폴더와 파일은 결과 레코드의 커서로 관리하여 작업 경로 목록의 추가 축적 방지.
            let mut next_folder = 0;
            let mut next_file = 0;
            let mut alternate = false;
            let mut counts = ScanProgress::default();
            let mut last = Instant::now();
            loop {
                crate::output::check(&cancelled)?;
                for id in 0..count {
                    if !idle[id] {
                        continue;
                    }
                    let building = scope(trace, Stage::TaskBuild);
                    let metadata_ready = detailed && next_file < result.files.len();
                    let task =
                        if next_folder < result.folders.len() && (!metadata_ready || alternate) {
                            let index = next_folder;
                            next_folder += 1;
                            Task::Folder(index, result.folders[index].path.clone())
                        } else if metadata_ready {
                            let end = (next_file + BATCH).min(result.files.len());
                            let paths = (next_file..end)
                                .map(|index| {
                                    let f = &result.files[index];
                                    (index, result.folders[f.parent].path.join(&f.name))
                                })
                                .collect();
                            next_file = end;
                            Task::Metadata(paths)
                        } else {
                            continue;
                        };
                    alternate = !alternate;
                    drop(building);
                    let dispatching = scope(trace, Stage::Dispatch);
                    senders[id]
                        .send(task)
                        .map_err(|_| io::Error::other("스캔 작업 스레드가 중단되었습니다"))?;
                    drop(dispatching);
                    idle[id] = false;
                }
                if idle.iter().all(|v| *v)
                    && next_folder == result.folders.len()
                    && (!detailed || next_file == result.files.len())
                {
                    break;
                }
                let receiving = scope(trace, Stage::ResultReceive);
                let event = match rx.recv_timeout(Duration::from_millis(20)) {
                    Ok(v) => Some(v),
                    Err(mpsc::RecvTimeoutError::Timeout) => None,
                    Err(_) => return Err(io::Error::other("스캔 작업 응답이 끊겼습니다")),
                };
                drop(receiving);
                let _coordinating = scope(trace, Stage::Coordinator);
                match event {
                    Some(Event::Entries(parent, entries)) => {
                        for entry in entries {
                            let m = match entry {
                                Ok(v) => v,
                                Err((path, e)) => {
                                    result.record_error(path, e);
                                    continue;
                                }
                            };
                            let path = m.path;
                            if m.attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
                                result.skipped_reparse_points += 1;
                                continue;
                            }
                            if m.attributes & 0x10 != 0 {
                                result.folders.push(Folder {
                                    allocation: Default::default(),
                                    created: m.created,
                                    attributes: m.attributes,
                                    path,
                                    parent: Some(parent),
                                    logical_bytes: 0,
                                    file_count: 0,
                                    folder_count: 0,
                                    modified: m.modified,
                                });
                            } else {
                                let extra = if detailed {
                                    m.extra
                                } else {
                                    crate::metadata::Extra::default()
                                };
                                result.files.push(FileEntry {
                                    extra,
                                    created: m.created,
                                    attributes: m.attributes,
                                    name: m.name,
                                    parent,
                                    logical_bytes: m.logical_bytes,
                                    modified: m.modified,
                                });
                                let folder = &mut result.folders[parent];
                                folder.logical_bytes = folder
                                    .logical_bytes
                                    .checked_add(m.logical_bytes)
                                    .ok_or_else(|| {
                                        io::Error::other("논리 크기 합계가 표현 범위를 넘습니다")
                                    })?;
                                folder.file_count += 1;
                                if !detailed {
                                    folder.allocation.add(extra.allocation)?;
                                    counts.files_scanned += 1;
                                }
                            }
                        }
                    }
                    Some(Event::FolderDone(id, index, value, observed)) => {
                        idle[id] = true;
                        if let Some(observed) = observed {
                            let folder = &mut result.folders[index];
                            folder.created = observed.created;
                            folder.modified = observed.modified;
                            folder.attributes = observed.attributes;
                        }
                        match value {
                            Ok(()) => counts.folders_scanned += 1,
                            Err(e) if index == 0 || e.kind() == io::ErrorKind::InvalidData => {
                                return Err(e)
                            }
                            Err(e) => result.record_error(result.folders[index].path.clone(), e),
                        }
                    }
                    Some(Event::Metadata(id, reports)) => {
                        idle[id] = true;
                        for (index, report) in reports {
                            if let Some(error) = report.issue {
                                result.metadata_error_count += 1;
                                if result.metadata_issues.len() < MAX_REPORTED_ERRORS {
                                    let f = &result.files[index];
                                    result.metadata_issues.push(ScanIssue {
                                        path: result.folders[f.parent].path.join(&f.name),
                                        error,
                                    });
                                }
                            }
                            let f = &mut result.files[index];
                            f.extra = report.values;
                            result.folders[f.parent]
                                .allocation
                                .add(report.values.allocation)?;
                            counts.files_scanned += 1;
                        }
                    }
                    Some(Event::Failed) => {
                        return Err(io::Error::other(
                            "스캔 작업 스레드에서 예기치 않은 오류가 발생했습니다",
                        ))
                    }
                    None => {}
                }
                if last.elapsed() >= Duration::from_millis(30) {
                    progress(counts);
                    last = Instant::now();
                }
            }
            crate::output::check(&cancelled)?;
            let _finish = scope(trace, Stage::Finish);
            let clock = metrics.map(|_| Instant::now());
            finish(&mut result)?;
            if let (Some(metrics), Some(clock)) = (metrics, clock) {
                metrics
                    .finish_ns
                    .fetch_add(clock.elapsed().as_nanos() as u64, Ordering::Relaxed);
            }
            crate::output::check(&cancelled)?;
            progress(counts);
            Ok(result)
        })();
        stop.store(true, Ordering::Relaxed);
        drop(senders);
        drop(rx);
        let mut failed = false;
        for handle in handles {
            failed |= handle.join().is_err();
        }
        if failed {
            Err(io::Error::other("스캔 작업 스레드 종료 실패"))
        } else {
            outcome
        }
    })
}
