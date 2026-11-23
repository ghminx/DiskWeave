use super::{interrupted, match_page, Input, Page, Results, SearchRoot};
use crate::scanner::directory;
use std::{
    io,
    panic::{self, AssertUnwindSafe},
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, SyncSender},
    },
    thread,
};

const COMPUTER_SEARCH_WORKERS: usize = 8;

struct Folder {
    path: PathBuf,
    expected: PathBuf,
    root: usize,
}

enum Event {
    Page(usize, Page),
    Finished {
        worker: usize,
        path: PathBuf,
        result: io::Result<()>,
    },
    WorkerFailed,
}

pub(super) fn walk(
    roots: &[SearchRoot],
    input: &Input,
    needle: &str,
    cancelled: &(impl Fn() -> bool + Sync),
    results: &mut Results<'_>,
) -> io::Result<()> {
    if roots.is_empty() {
        return Ok(());
    }
    let stopping = AtomicBool::new(false);
    let stop = || cancelled() || stopping.load(Ordering::Relaxed);
    thread::scope(|scope| {
        let (tx, rx) = mpsc::sync_channel(COMPUTER_SEARCH_WORKERS * 2);
        let mut workers = Vec::with_capacity(COMPUTER_SEARCH_WORKERS);
        for worker in 0..COMPUTER_SEARCH_WORKERS {
            let (tasks, work) = mpsc::sync_channel(1);
            let tx = tx.clone();
            let stop = &stop;
            thread::Builder::new().spawn_scoped(scope, move || {
                let outcome = panic::catch_unwind(AssertUnwindSafe(|| {
                    while let Ok(folder) = work.recv() {
                        if !read_folder(worker, folder, input, needle, stop, &tx) {
                            break;
                        }
                    }
                }));
                // 작업자 종료 원인을 조정 스레드의 수신 대기에서 확인.
                if outcome.is_err() {
                    let _ = tx.send(Event::WorkerFailed);
                }
            })?;
            workers.push(tasks);
        }
        drop(tx);
        let result = dispatch(roots, &workers, &rx, &stop, results);
        stopping.store(true, Ordering::Relaxed);
        // scoped join 전에 대기 중인 송수신 채널 해제.
        drop(rx);
        drop(workers);
        result
    })
}

fn read_folder(
    worker: usize,
    folder: Folder,
    input: &Input,
    needle: &str,
    stop: &impl Fn() -> bool,
    tx: &SyncSender<Event>,
) -> bool {
    let result = directory::collect_scoped(&folder.path, &folder.expected, stop, &mut |entries| {
        let page = match_page(input, needle, entries);
        if !page.folders.is_empty() || !page.hits.is_empty() || page.coverage.skipped != 0 {
            tx.send(Event::Page(folder.root, page))
                .map_err(|_| interrupted())?;
        }
        Ok(())
    });
    tx.send(Event::Finished {
        worker,
        path: folder.path,
        result,
    })
    .is_ok()
}

fn dispatch(
    roots: &[SearchRoot],
    workers: &[SyncSender<Folder>],
    events: &Receiver<Event>,
    stop: &impl Fn() -> bool,
    results: &mut Results<'_>,
) -> io::Result<()> {
    let worker_error = || io::Error::other("검색 작업이 예기치 않게 종료됐습니다");
    let mut pending: Vec<_> = roots
        .iter()
        .enumerate()
        .map(|(index, root)| (root.path.clone(), index))
        .collect();
    let mut idle: Vec<_> = (0..workers.len()).collect();
    while !pending.is_empty() || idle.len() != workers.len() {
        crate::output::check(stop)?;
        while !pending.is_empty() && !idle.is_empty() {
            let (path, root) = pending.pop().unwrap();
            let expected = roots[root].expected(&path)?;
            let worker = idle.pop().unwrap();
            workers[worker]
                .send(Folder {
                    path,
                    expected,
                    root,
                })
                .map_err(|_| worker_error())?;
        }
        match events.recv().map_err(|_| worker_error())? {
            Event::Page(root, page) => {
                pending.extend(page.folders.into_iter().map(|path| (path, root)));
                results.add_page(page.hits, page.coverage)?;
                if results.coverage.limited {
                    break;
                }
            }
            Event::Finished {
                worker,
                path,
                result,
            } => {
                idle.push(worker);
                if let Err(error) = result {
                    if error.kind() == io::ErrorKind::Interrupted {
                        return Err(error);
                    }
                    results.coverage.skip(&path, error);
                }
            }
            Event::WorkerFailed => return Err(worker_error()),
        }
    }
    Ok(())
}
