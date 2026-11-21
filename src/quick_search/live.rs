use crate::{
    file_open::TargetKind,
    metadata::Identity,
    scanner::directory::{self, Strategy},
    search::{EntryKind, SearchMode},
};
use std::{
    fs, io,
    os::windows::fs::MetadataExt,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, SyncSender, TryRecvError},
        Arc,
    },
    time::{Duration, Instant},
};

const MAX_HITS: usize = 10_000;

#[derive(Clone)]
pub struct Hit {
    pub path: PathBuf,
    pub kind: TargetKind,
    pub bytes: Option<u64>,
    pub identity: Option<Identity>,
}

#[derive(Clone)]
pub struct Input {
    pub folder: Option<PathBuf>,
    pub text: String,
    pub mode: SearchMode,
}

#[derive(Default)]
pub struct Coverage {
    pub skipped: usize,
    pub first_issue: Option<String>,
    pub limited: bool,
}

impl Coverage {
    fn skip(&mut self, path: &Path, error: impl std::fmt::Display) {
        self.skipped += 1;
        if self.first_issue.is_none() {
            self.first_issue = Some(format!("{} · {error}", path.display()));
        }
    }
}

enum Message {
    Hits(Vec<Hit>),
    Done(io::Result<Coverage>),
}

struct Worker {
    messages: Receiver<Message>,
    cancel: Arc<AtomicBool>,
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

#[derive(Default)]
pub struct LiveSearch {
    pub input: Option<Input>,
    pub hits: Vec<Hit>,
    pub stopped: bool,
    pub coverage: Coverage,
    pub error: Option<String>,
    worker: Option<Worker>,
    files: Vec<usize>,
    folders: Vec<usize>,
}

impl LiveSearch {
    pub fn running(&self) -> bool {
        self.worker.is_some()
    }

    pub fn count(&self, kind: EntryKind) -> usize {
        match kind {
            EntryKind::All => self.hits.len(),
            EntryKind::Files => self.files.len(),
            EntryKind::Folders => self.folders.len(),
        }
    }

    pub fn row(&self, kind: EntryKind, row: usize) -> Option<&Hit> {
        let index = match kind {
            EntryKind::All => row,
            EntryKind::Files => *self.files.get(row)?,
            EntryKind::Folders => *self.folders.get(row)?,
        };
        self.hits.get(index)
    }

    pub fn remove_recycled(&mut self, path: &Path) -> bool {
        let before = self.hits.len();
        self.hits.retain(|hit| !hit.path.starts_with(path));
        if before == self.hits.len() {
            return false;
        }
        self.files.clear();
        self.folders.clear();
        index_hits(&self.hits, 0, &mut self.files, &mut self.folders);
        true
    }

    pub fn start(
        &mut self,
        input: Input,
        fixed_roots: impl FnOnce() -> io::Result<Vec<PathBuf>> + Send + 'static,
    ) -> io::Result<()> {
        self.poll();
        if self.running() {
            return Err(io::Error::other(
                "이전 검색이 끝나거나 중지된 뒤 다시 시도해 주세요",
            ));
        }
        let (sender, messages) = mpsc::sync_channel(8);
        let cancel = Arc::new(AtomicBool::new(false));
        let work_cancel = Arc::clone(&cancel);
        let work_input = input.clone();
        std::thread::spawn(move || {
            let result = search(work_input, fixed_roots, &work_cancel, &sender);
            let _ = sender.send(Message::Done(result));
        });
        *self = Self {
            input: Some(input),
            worker: Some(Worker { messages, cancel }),
            ..Self::default()
        };
        Ok(())
    }

    pub fn stop(&mut self) {
        if let Some(worker) = &self.worker {
            worker.cancel.store(true, Ordering::Relaxed);
            self.stopped = true;
        }
    }

    pub fn clear(&mut self) {
        self.stop();
        self.input = None;
        self.hits.clear();
        self.files.clear();
        self.folders.clear();
        self.coverage = Coverage::default();
        self.error = None;
    }

    pub fn poll(&mut self) {
        let Some(worker) = &self.worker else { return };
        for _ in 0..8 {
            match worker.messages.try_recv() {
                Ok(Message::Hits(mut hits)) => {
                    if !self.stopped {
                        index_hits(&hits, self.hits.len(), &mut self.files, &mut self.folders);
                        self.hits.append(&mut hits);
                    }
                }
                Ok(Message::Done(result)) => {
                    if !self.stopped {
                        match result {
                            Ok(coverage) => self.coverage = coverage,
                            Err(e) => self.error = Some(e.to_string()),
                        }
                    }
                    self.worker = None;
                    break;
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    if !self.stopped {
                        self.error = Some("검색 작업이 예기치 않게 종료됐습니다".into());
                    }
                    self.worker = None;
                    break;
                }
            }
        }
    }
}

fn index_hits(hits: &[Hit], offset: usize, files: &mut Vec<usize>, folders: &mut Vec<usize>) {
    for (row, hit) in hits.iter().enumerate() {
        match hit.kind {
            TargetKind::File => files.push(offset + row),
            TargetKind::Folder => folders.push(offset + row),
        }
    }
}

pub fn check_root(path: &Path) -> io::Result<()> {
    if !path.is_absolute() {
        return Err(io::Error::other("검색할 폴더의 전체 경로를 입력하세요"));
    }
    for component in path.ancestors() {
        let info = fs::symlink_metadata(component)?;
        if !info.is_dir() || info.file_attributes() & 0x400 != 0 {
            return Err(io::Error::other("일반 폴더를 하나 지정하세요"));
        }
    }
    Ok(())
}

fn search(
    input: Input,
    fixed_roots: impl FnOnce() -> io::Result<Vec<PathBuf>>,
    cancel: &AtomicBool,
    sender: &SyncSender<Message>,
) -> io::Result<Coverage> {
    let cancelled = || cancel.load(Ordering::Relaxed);
    let interrupted = || io::Error::from(io::ErrorKind::Interrupted);
    let roots = if let Some(root) = input.folder {
        let root = std::path::absolute(root)?;
        check_root(&root)?;
        vec![root]
    } else {
        fixed_roots()?
    };
    if roots.is_empty() {
        return Err(io::Error::other("검색할 로컬 고정 드라이브가 없습니다"));
    }
    let mut pending = roots;
    let needle = input.text.to_lowercase();
    let mut coverage = Coverage::default();
    let mut batch = Vec::with_capacity(64);
    let mut sent = 0;
    let mut emitted_at = Instant::now();
    while let Some(folder) = pending.pop() {
        crate::output::check(&cancelled)?;
        let result = directory::collect(&folder, Strategy::Bulk, &cancelled, &mut |entries| {
            for entry in entries {
                let entry = match entry {
                    Ok(entry) => entry,
                    Err((path, error)) => {
                        coverage.skip(&path, error);
                        continue;
                    }
                };
                if entry.attributes & 0x400 != 0 {
                    coverage.skip(&entry.path, "특수 경로 제외");
                    continue;
                }
                let folder = entry.attributes & 0x10 != 0;
                if folder {
                    pending.push(entry.path.clone());
                }
                let value = match input.mode {
                    SearchMode::Name => entry.name.to_string_lossy(),
                    SearchMode::Path => entry.path.to_string_lossy(),
                };
                if value.to_lowercase().contains(&needle) {
                    if sent == MAX_HITS {
                        coverage.limited = true;
                        return Err(interrupted());
                    }
                    sent += 1;
                    batch.push(Hit {
                        path: entry.path,
                        kind: if folder {
                            TargetKind::Folder
                        } else {
                            TargetKind::File
                        },
                        bytes: (!folder).then_some(entry.logical_bytes),
                        identity: entry.extra.identity.value().filter(|id| id.file != [0; 16]),
                    });
                }
            }
            if !batch.is_empty()
                && (sent == batch.len()
                    || batch.len() >= 64
                    || emitted_at.elapsed() >= Duration::from_millis(50))
            {
                sender
                    .send(Message::Hits(std::mem::replace(
                        &mut batch,
                        Vec::with_capacity(64),
                    )))
                    .map_err(|_| interrupted())?;
                emitted_at = Instant::now();
            }
            Ok(())
        });
        if let Err(error) = result {
            if coverage.limited {
                break;
            }
            if error.kind() == io::ErrorKind::Interrupted {
                return Err(error);
            }
            coverage.skip(&folder, error);
        }
    }
    if !batch.is_empty() {
        sender
            .send(Message::Hits(batch))
            .map_err(|_| interrupted())?;
    }
    Ok(coverage)
}
