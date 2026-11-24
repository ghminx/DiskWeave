mod parallel;

use crate::{
    file_open::TargetKind,
    metadata::Identity,
    scanner::directory,
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
    pub kind: EntryKind,
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
    recycled: Vec<PathBuf>,
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
}

impl LiveSearch {
    pub fn running(&self) -> bool {
        self.worker.is_some()
    }

    pub fn remove_recycled(&mut self, path: &Path) -> bool {
        let path = crate::file_ops::comparable_path(path).unwrap_or_else(|_| path.to_owned());
        let before = self.hits.len();
        retain_unrecycled(&mut self.hits, std::slice::from_ref(&path));
        if let Some(worker) = self.worker.as_mut().filter(|_| !self.stopped) {
            worker.recycled.push(path);
        }
        before != self.hits.len()
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
            worker: Some(Worker {
                messages,
                cancel,
                recycled: Vec::new(),
            }),
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
        self.coverage = Coverage::default();
        self.error = None;
    }

    pub fn poll(&mut self) {
        let Some(worker) = &self.worker else { return };
        for _ in 0..8 {
            match worker.messages.try_recv() {
                Ok(Message::Hits(mut hits)) => {
                    if !self.stopped {
                        retain_unrecycled(&mut hits, &worker.recycled);
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

fn retain_unrecycled(hits: &mut Vec<Hit>, roots: &[PathBuf]) {
    if roots.is_empty() {
        return;
    }
    hits.retain(|hit| {
        let normalized;
        let path = if matches!(hit.path.components().next(),
            Some(std::path::Component::Prefix(prefix)) if prefix.kind().is_verbatim())
        {
            normalized = crate::file_ops::comparable_path(&hit.path);
            normalized.as_deref().unwrap_or(&hit.path)
        } else {
            &hit.path
        };
        !roots.iter().any(|root| path.starts_with(root))
    });
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
    let roots = match &input.folder {
        Some(root) => vec![std::path::absolute(root)?],
        None => fixed_roots()?,
    };
    if roots.is_empty() {
        return Err(io::Error::other("검색할 로컬 고정 드라이브가 없습니다"));
    }
    let mut results = Results::new(sender);
    let mut scoped = Vec::with_capacity(roots.len());
    for path in roots {
        match directory::scoped_root(&path, &cancelled) {
            Ok(resolved) => scoped.push(SearchRoot { path, resolved }),
            Err(error) if error.kind() == io::ErrorKind::Interrupted || input.folder.is_some() => {
                return Err(error)
            }
            Err(error) => results.coverage.skip(&path, error),
        }
    }
    let needle = input.text.to_lowercase();
    if input.folder.is_none() {
        parallel::walk(&scoped, &input, &needle, &cancelled, &mut results)?;
    } else if let Some(root) = scoped.first() {
        walk_folder(root, &input, &needle, &cancelled, &mut results)?;
    }
    results.finish()
}

fn interrupted() -> io::Error {
    io::Error::from(io::ErrorKind::Interrupted)
}

struct SearchRoot {
    path: PathBuf,
    resolved: PathBuf,
}

impl SearchRoot {
    fn expected(&self, folder: &Path) -> io::Result<PathBuf> {
        let relative = folder.strip_prefix(&self.path).map_err(io::Error::other)?;
        Ok(self.resolved.join(relative))
    }
}

#[derive(Default)]
struct Page {
    folders: Vec<PathBuf>,
    hits: Vec<Hit>,
    coverage: Coverage,
}

fn match_page(input: &Input, needle: &str, entries: Vec<directory::Item>) -> Page {
    let mut page = Page::default();
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err((path, error)) => {
                page.coverage.skip(&path, error);
                continue;
            }
        };
        if entry.attributes & 0x400 != 0 {
            page.coverage.skip(&entry.path, "특수 경로 제외");
            continue;
        }
        let folder = entry.attributes & 0x10 != 0;
        if folder {
            page.folders.push(entry.path.clone());
        }
        if matches!(
            (input.kind, folder),
            (EntryKind::Files, true) | (EntryKind::Folders, false)
        ) {
            continue;
        }
        let value = match input.mode {
            SearchMode::Name => entry.name.to_string_lossy(),
            SearchMode::Path => entry.path.to_string_lossy(),
        };
        if value.to_lowercase().contains(needle) {
            page.hits.push(Hit {
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
    page
}

struct Results<'a> {
    sender: &'a SyncSender<Message>,
    batch: Vec<Hit>,
    sent: usize,
    emitted_at: Instant,
    coverage: Coverage,
}

impl<'a> Results<'a> {
    fn new(sender: &'a SyncSender<Message>) -> Self {
        Self {
            sender,
            batch: Vec::with_capacity(64),
            sent: 0,
            emitted_at: Instant::now(),
            coverage: Coverage::default(),
        }
    }

    fn add_page(&mut self, hits: Vec<Hit>, coverage: Coverage) -> io::Result<()> {
        self.coverage.skipped += coverage.skipped;
        if self.coverage.first_issue.is_none() {
            self.coverage.first_issue = coverage.first_issue;
        }
        let remaining = MAX_HITS - self.sent;
        let accepted = remaining.min(hits.len());
        self.coverage.limited = hits.len() > remaining;
        self.sent += accepted;
        self.batch.extend(hits.into_iter().take(accepted));
        if !self.batch.is_empty()
            && (self.sent == self.batch.len()
                || self.batch.len() >= 64
                || self.emitted_at.elapsed() >= Duration::from_millis(50))
        {
            self.sender
                .send(Message::Hits(std::mem::replace(
                    &mut self.batch,
                    Vec::with_capacity(64),
                )))
                .map_err(|_| interrupted())?;
            self.emitted_at = Instant::now();
        }
        Ok(())
    }

    fn finish(self) -> io::Result<Coverage> {
        if !self.batch.is_empty() {
            self.sender
                .send(Message::Hits(self.batch))
                .map_err(|_| interrupted())?;
        }
        Ok(self.coverage)
    }
}

fn walk_folder(
    root: &SearchRoot,
    input: &Input,
    needle: &str,
    cancelled: &impl Fn() -> bool,
    results: &mut Results<'_>,
) -> io::Result<()> {
    let mut pending = vec![root.path.clone()];
    while let Some(folder) = pending.pop() {
        crate::output::check(cancelled)?;
        let expected = root.expected(&folder)?;
        let outcome = directory::collect_scoped(&folder, &expected, cancelled, &mut |entries| {
            let page = match_page(input, needle, entries);
            pending.extend(page.folders);
            results.add_page(page.hits, page.coverage)?;
            crate::output::check(&|| results.coverage.limited)
        });
        if results.coverage.limited {
            break;
        }
        if let Err(error) = outcome {
            if error.kind() == io::ErrorKind::Interrupted {
                return Err(error);
            }
            results.coverage.skip(&folder, error);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scanner::directory::{collect_scoped, scoped_root};

    #[test]
    fn scoped_enumeration_rejects_a_handle_to_a_different_directory() {
        let fixture = crate::duplicates::tests::Fixture::new();
        let selected = fixture.0.join("selected");
        let other = fixture.0.join("other");
        fs::create_dir(&selected).unwrap();
        fs::create_dir(&other).unwrap();
        fs::write(other.join("outside.txt"), []).unwrap();
        let expected = scoped_root(&selected, &|| false).unwrap();
        let mut found = Vec::new();
        let error = collect_scoped(&other, &expected, &|| false, &mut |rows| {
            found.extend(rows);
            Ok(())
        })
        .unwrap_err();
        assert!(error.to_string().contains("검색 범위"));
        assert!(found.is_empty());
        collect_scoped(
            &other,
            &scoped_root(&other, &|| false).unwrap(),
            &|| false,
            &mut |rows| {
                found.extend(rows);
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].as_ref().unwrap().name, "outside.txt");
    }

    #[test]
    fn scoped_enumeration_does_not_follow_a_replaced_ancestor() {
        use std::os::windows::process::CommandExt;

        let fixture = crate::duplicates::tests::Fixture::new();
        let selected = fixture.0.join("selected");
        let ancestor = selected.join("ancestor");
        let pending = ancestor.join("pending");
        let outside = fixture.0.join("outside");
        fs::create_dir_all(&pending).unwrap();
        fs::create_dir_all(outside.join("pending")).unwrap();
        fs::write(outside.join("pending/outside.txt"), []).unwrap();
        let expected = scoped_root(&selected, &|| false)
            .unwrap()
            .join("ancestor")
            .join("pending");

        fs::remove_dir(&pending).unwrap();
        fs::remove_dir(&ancestor).unwrap();
        let link = std::process::Command::new("cmd.exe")
            .args(["/d", "/c", "mklink", "/J"])
            .arg(&ancestor)
            .arg(&outside)
            .creation_flags(0x08000000)
            .output()
            .unwrap();
        assert!(link.status.success(), "{:?}", link.stderr);

        let mut emitted = false;
        let result = collect_scoped(&pending, &expected, &|| false, &mut |_| {
            emitted = true;
            Ok(())
        });
        let root_result = scoped_root(&pending, &|| false);
        fs::remove_dir(&ancestor).unwrap();
        assert!(result.unwrap_err().to_string().contains("검색 범위"));
        assert!(root_result.is_err());
        assert!(!emitted);
    }
    #[test]
    fn late_recycled_path_stays_removed() {
        let deleted = PathBuf::from(r"C:\test-only\removed");
        let keep = PathBuf::from(r"C:\test-only\keep.bin");
        let hit = |path| Hit {
            path,
            kind: TargetKind::File,
            bytes: Some(1),
            identity: None,
        };
        let (sender, messages) = mpsc::sync_channel(8);
        let mut search = LiveSearch {
            hits: vec![hit(deleted.join("first.bin"))],
            worker: Some(Worker {
                messages,
                cancel: Arc::new(AtomicBool::new(false)),
                recycled: Vec::new(),
            }),
            ..Default::default()
        };
        assert!(search.remove_recycled(&deleted));
        sender
            .send(Message::Hits(vec![
                hit(PathBuf::from(r"\\?\C:\test-only\removed\late.bin")),
                hit(keep.clone()),
            ]))
            .unwrap();
        search.poll();
        let paths: Vec<_> = search.hits.into_iter().map(|hit| hit.path).collect();
        assert_eq!(paths, vec![keep]);
    }
}
