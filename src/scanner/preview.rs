use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    io,
    os::windows::ffi::{OsStrExt, OsStringExt},
    path::PathBuf,
    time::{Duration, Instant},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Calculating,
    Complete,
    Partial,
}
impl State {
    pub fn label(self) -> &'static str {
        match self {
            Self::Calculating => "계산 중",
            Self::Complete => "확인 완료",
            Self::Partial => "일부 누락",
        }
    }
}
#[derive(Clone, Copy, Default, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Values {
    pub bytes: u64,
    pub files: u64,
    pub missing: u64,
    pub done: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Update {
    Finished,
    Folder {
        id: usize,
        parent: Option<usize>,
        name: Vec<u16>,
    },
    Values {
        id: usize,
        value: Values,
    },
}
#[derive(Clone, Default, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Batch {
    pub reset: bool,
    pub updates: Vec<Update>,
}
pub type Observer<'a> = dyn FnMut(Batch) -> io::Result<()> + 'a;

pub struct Folder {
    pub path: PathBuf,
    pub parent: Option<usize>,
    pub children: Vec<usize>,
    pub bytes: u64,
    pub files: u64,
    pub folders: u64,
    missing: u64,
    complete: bool,
    own: Values,
    names: HashSet<std::ffi::OsString>,
}
impl Folder {
    pub fn state(&self) -> State {
        if self.missing > 0 {
            State::Partial
        } else if self.complete {
            State::Complete
        } else {
            State::Calculating
        }
    }
}
/// 완료 결과와 분리된 폴더 전용 중간 모델.
static NEXT_EPOCH: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
pub struct Model {
    pub root: PathBuf,
    pub folders: Vec<Folder>,
    pub revision: u64,
    epoch: u64,
    sealed: bool,
}
fn invalid() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "잘못된 폴더 미리보기")
}
impl Model {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            folders: Vec::new(),
            revision: 0,
            epoch: 0,
            sealed: false,
        }
    }
    pub fn apply(&mut self, batch: Batch) -> io::Result<()> {
        if batch.reset {
            self.folders.clear();
            self.sealed = false;
            self.epoch = NEXT_EPOCH.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        let mut dirty = HashSet::new();
        for update in batch.updates {
            if self.sealed {
                return Err(invalid());
            }
            let id = match update {
                Update::Finished => {
                    if self.folders.is_empty() {
                        return Err(invalid());
                    }
                    for (id, folder) in self.folders.iter_mut().enumerate() {
                        folder.own.done = true;
                        dirty.insert(id);
                    }
                    self.sealed = true;
                    continue;
                }

                Update::Folder { id, parent, name } => {
                    if id != self.folders.len()
                        || (id == 0 && (parent.is_some() || !name.is_empty()))
                        || (id > 0 && parent.is_none_or(|p| p >= id))
                    {
                        return Err(invalid());
                    }
                    let path = if let Some(parent) = parent {
                        if name.is_empty()
                            || name.len() > 32767
                            || name.iter().any(|c| [0, 47, 58, 92].contains(c))
                        {
                            return Err(invalid());
                        }
                        let name = std::ffi::OsString::from_wide(&name);
                        if name == "." || name == ".." {
                            return Err(invalid());
                        }
                        if self.folders[parent].own.done
                            || !self.folders[parent].names.insert(name.clone())
                        {
                            return Err(invalid());
                        }
                        let path = self.folders[parent].path.join(name);
                        self.folders[parent].children.push(id);
                        path
                    } else {
                        self.root.clone()
                    };
                    self.folders.push(Folder {
                        path,
                        parent,
                        children: Vec::new(),
                        bytes: 0,
                        files: 0,
                        folders: 0,
                        missing: 0,
                        complete: false,
                        own: Values::default(),
                        names: HashSet::new(),
                    });
                    id
                }
                Update::Values { id, value } => {
                    let folder = self.folders.get_mut(id).ok_or_else(invalid)?;
                    if value.bytes < folder.own.bytes
                        || value.files < folder.own.files
                        || value.missing < folder.own.missing
                        || (folder.own.done && !value.done)
                    {
                        return Err(invalid());
                    }
                    folder.own = value;
                    id
                }
            };
            let mut cursor = Some(id);
            while let Some(id) = cursor {
                if !dirty.insert(id) {
                    break;
                }
                cursor = self.folders[id].parent;
            }
        }
        let mut dirty: Vec<_> = dirty.into_iter().collect();
        dirty.sort_unstable_by(|a, b| b.cmp(a));
        for id in dirty {
            let f = &self.folders[id];
            let (mut bytes, mut files, mut folders, mut missing, mut complete) =
                (f.own.bytes, f.own.files, 0u64, f.own.missing, f.own.done);
            for child in &f.children {
                let child = &self.folders[*child];
                bytes = bytes.checked_add(child.bytes).ok_or_else(invalid)?;
                files = files.checked_add(child.files).ok_or_else(invalid)?;
                folders = folders
                    .checked_add(child.folders)
                    .and_then(|v| v.checked_add(1))
                    .ok_or_else(invalid)?;
                missing = missing.checked_add(child.missing).ok_or_else(invalid)?;
                complete &= child.complete;
            }
            let f = &mut self.folders[id];
            (f.bytes, f.files, f.folders, f.missing, f.complete) =
                (bytes, files, folders, missing, complete);
        }
        self.revision += 1;
        Ok(())
    }
}

#[derive(Default)]
pub struct View {
    pub folder: usize,
    pub selected: usize,
    pub previous: bool,
    expanded: HashSet<usize>,
    epoch: u64,
    revision: u64,
    visible: Vec<(usize, usize)>,
}
impl View {
    pub fn sync(&mut self, model: &Model) {
        if self.epoch != model.epoch {
            *self = Self {
                epoch: model.epoch,
                expanded: HashSet::from([0]),
                ..Default::default()
            };
        }
        if self.revision == model.revision {
            return;
        }
        self.revision = model.revision;
        self.visible.clear();
        if model.folders.is_empty() {
            return;
        }
        let mut pending = vec![(0, 0)];
        while let Some((id, depth)) = pending.pop() {
            self.visible.push((id, depth));
            if self.expanded.contains(&id) {
                pending.extend(
                    model.folders[id]
                        .children
                        .iter()
                        .rev()
                        .map(|id| (*id, depth + 1)),
                );
            }
        }
    }
    pub fn visible(&self) -> &[(usize, usize)] {
        &self.visible
    }
    pub fn expanded(&self, id: usize) -> bool {
        self.expanded.contains(&id)
    }
    pub fn toggle(&mut self, id: usize) {
        if !self.expanded.remove(&id) {
            self.expanded.insert(id);
        }
        self.revision = 0;
    }
    pub fn open(&mut self, model: &Model, id: usize) {
        self.folder = id;
        self.selected = id;
        let mut parent = model.folders[id].parent;
        while let Some(id) = parent {
            self.expanded.insert(id);
            parent = model.folders[id].parent;
        }
        self.revision = 0;
    }
    pub fn complete(&self, model: &Model, result: &super::ScanResult) -> crate::explorer::Explorer {
        let mut view = crate::explorer::Explorer::new(result);
        let ids: HashMap<_, _> = result
            .folders
            .iter()
            .enumerate()
            .map(|(id, f)| (&f.path, id))
            .collect();
        if let Some(id) = model
            .folders
            .get(self.folder)
            .and_then(|f| ids.get(&f.path))
        {
            view.open_folder(result, *id);
        }
        for id in &self.expanded {
            if let Some(id) = model.folders.get(*id).and_then(|f| ids.get(&f.path)) {
                if !view.is_expanded(*id) {
                    view.toggle_folder(result, *id);
                }
            }
        }
        if let Some(id) = model
            .folders
            .get(self.selected)
            .and_then(|f| ids.get(&f.path))
        {
            view.select(crate::explorer::Item::Folder(*id));
        }
        view
    }
}

#[derive(Default)]
struct Pending {
    batch: Batch,
    values: HashMap<usize, Values>,
    finished: bool,
}
/// 관리자 IPC의 단일 시작·부모 우선·범위·집계 계약 검증.
pub struct ValidatedStream {
    model: Model,
    started: bool,
}
impl ValidatedStream {
    pub fn new(root: PathBuf) -> Self {
        Self {
            model: Model::new(root),
            started: false,
        }
    }
    pub fn accept(&mut self, batch: Batch) -> io::Result<()> {
        if batch.reset == self.started || batch.updates.len() > 128 {
            return Err(invalid());
        }
        self.started = true;
        self.model.apply(batch)
    }
}
/// 폴더별 미표시 갱신을 덮어써 파일별 대기열 축적을 방지하는 전달 경계.
#[derive(Clone, Default)]
pub struct Mailbox(std::sync::Arc<std::sync::Mutex<Pending>>);
impl Mailbox {
    pub fn publish(&self, batch: Batch) -> io::Result<()> {
        let mut pending = self.0.lock().unwrap();
        if batch.reset {
            *pending = Pending::default();
            pending.batch.reset = true;
        }
        for update in batch.updates {
            match update {
                Update::Finished => pending.finished = true,
                Update::Values { id, value } => {
                    pending.values.insert(id, value);
                }
                other => pending.batch.updates.push(other),
            }
        }
        Ok(())
    }
    pub fn drain(&self) -> Option<Batch> {
        let pending = std::mem::take(&mut *self.0.lock().unwrap());
        if !pending.batch.reset
            && pending.batch.updates.is_empty()
            && pending.values.is_empty()
            && !pending.finished
        {
            return None;
        }
        let mut batch = pending.batch;
        batch.updates.extend(
            pending
                .values
                .into_iter()
                .map(|(id, value)| Update::Values { id, value }),
        );
        if pending.finished {
            batch.updates.push(Update::Finished);
        }
        Some(batch)
    }
}
struct Own {
    value: Values,
    pending: u64,
    enumerated: bool,
}
pub(crate) struct Recorder<'a> {
    observer: Option<&'a mut Observer<'a>>,
    own: Vec<Own>,
    batch: Batch,
    changed: HashSet<usize>,
    last: Instant,
}
impl<'a> Recorder<'a> {
    pub fn new(observer: Option<&'a mut Observer<'a>>) -> io::Result<Self> {
        let mut s = Self {
            observer,
            own: Vec::new(),
            batch: Batch {
                reset: true,
                updates: Vec::new(),
            },
            changed: HashSet::new(),
            last: Instant::now(),
        };
        s.folder(None, std::ffi::OsStr::new(""));
        s.flush(true)?;
        Ok(s)
    }
    pub fn folder(&mut self, parent: Option<usize>, name: &std::ffi::OsStr) {
        if self.observer.is_none() {
            return;
        }
        let id = self.own.len();
        self.own.push(Own {
            value: Values::default(),
            pending: 0,
            enumerated: false,
        });
        self.batch.updates.push(Update::Folder {
            id,
            parent,
            name: name.encode_wide().collect(),
        });
    }
    pub fn file(&mut self, parent: usize, bytes: u64, pending: bool) {
        if self.observer.is_none() {
            return;
        }
        let own = &mut self.own[parent];
        own.value.bytes += bytes;
        own.value.files += 1;
        own.pending += u64::from(pending);
        self.changed.insert(parent);
    }
    pub fn metadata(&mut self, parent: usize, missing: bool) {
        if self.observer.is_none() {
            return;
        }
        let own = &mut self.own[parent];
        own.pending -= 1;
        own.value.missing += u64::from(missing);
        self.changed.insert(parent);
    }
    pub fn error(&mut self, parent: usize) {
        if self.observer.is_none() {
            return;
        }
        self.own[parent].value.missing += 1;
        self.changed.insert(parent);
    }
    pub fn done(&mut self, id: usize) {
        if self.observer.is_none() {
            return;
        }
        self.own[id].enumerated = true;
        self.changed.insert(id);
    }
    pub fn finish(&mut self) -> io::Result<()> {
        self.flush(true)?;
        if let Some(observer) = self.observer.as_mut() {
            observer(Batch {
                reset: false,
                updates: vec![Update::Finished],
            })?;
        }
        Ok(())
    }
    pub fn flush(&mut self, force: bool) -> io::Result<()> {
        if self.observer.is_none()
            || (!force
                && self.last.elapsed() < Duration::from_millis(100)
                && self.batch.updates.len() < 128)
        {
            return Ok(());
        }
        for id in self.changed.drain() {
            let own = &mut self.own[id];
            own.value.done = own.enumerated && own.pending == 0;
            self.batch.updates.push(Update::Values {
                id,
                value: own.value,
            });
        }
        if self.batch.reset || !self.batch.updates.is_empty() {
            self.observer.as_mut().unwrap()(std::mem::take(&mut self.batch))?;
        }
        self.last = Instant::now();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn completed_navigation_survives_ntfs_drive_letter_normalization() {
        let root =
            std::env::temp_dir().join(format!("diskweave-preview-case-{}", std::process::id()));
        std::fs::create_dir_all(root.join("child")).unwrap();
        let mut spelling: Vec<_> = root.as_os_str().encode_wide().collect();
        spelling[0] = (spelling[0] as u8).to_ascii_lowercase() as u16;
        let mut model = Model::new(PathBuf::from(std::ffi::OsString::from_wide(&spelling)));
        model
            .apply(Batch {
                reset: true,
                updates: vec![
                    Update::Folder {
                        id: 0,
                        parent: None,
                        name: vec![],
                    },
                    Update::Folder {
                        id: 1,
                        parent: Some(0),
                        name: "child".encode_utf16().collect(),
                    },
                ],
            })
            .unwrap();
        let mut view = View::default();
        view.sync(&model);
        view.open(&model, 1);
        let result = super::super::scan(&root, || false).unwrap();
        let next = view.complete(&model, &result);
        assert_eq!(result.folders[next.folder()].path, root.join("child"));
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn cancellation_from_the_last_preview_never_returns_a_completed_result_or_open_workers() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let root =
            std::env::temp_dir().join(format!("diskweave-preview-cancel-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("known"), b"123").unwrap();
        let cancel = AtomicBool::new(false);
        let result = super::super::scan_preview(
            &root,
            true,
            super::super::Parallelism::Two,
            || cancel.load(Ordering::Relaxed),
            |_| {},
            &mut |batch| {
                if batch.updates.iter().any(|u| matches!(u, Update::Finished)) {
                    cancel.store(true, Ordering::Relaxed);
                }
                Ok(())
            },
        );
        assert_eq!(result.err().unwrap().kind(), io::ErrorKind::Interrupted);
        std::fs::rename(root.join("known"), root.join("released")).unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn one_completion_marker_seals_latest_coalesced_totals_and_rejects_late_updates() {
        let mail = Mailbox::default();
        mail.publish(Batch {
            reset: true,
            updates: vec![
                Update::Folder {
                    id: 0,
                    parent: None,
                    name: vec![],
                },
                Update::Folder {
                    id: 1,
                    parent: Some(0),
                    name: vec![65],
                },
            ],
        })
        .unwrap();
        mail.publish(Batch {
            reset: false,
            updates: vec![Update::Values {
                id: 1,
                value: Values {
                    bytes: 7,
                    files: 1,
                    ..Default::default()
                },
            }],
        })
        .unwrap();
        mail.publish(Batch {
            reset: false,
            updates: vec![Update::Finished],
        })
        .unwrap();
        let batch = mail.drain().unwrap();
        assert_eq!(batch.updates.len(), 4);
        let mut model = Model::new(PathBuf::from("C:\\fixture"));
        model.apply(batch).unwrap();
        assert_eq!(
            (model.folders[0].bytes, model.folders[0].state()),
            (7, State::Complete)
        );
        assert!(model
            .apply(Batch {
                reset: false,
                updates: vec![Update::Values {
                    id: 1,
                    value: Values {
                        bytes: 8,
                        files: 2,
                        done: true,
                        missing: 0
                    }
                }]
            })
            .is_err());
    }
    #[test]
    fn an_enumerated_parent_stays_calculating_until_descendants_finish_and_errors_propagate() {
        let mut model = Model::new(PathBuf::from("C:\\fixture"));
        model
            .apply(Batch {
                reset: true,
                updates: vec![
                    Update::Folder {
                        id: 0,
                        parent: None,
                        name: vec![],
                    },
                    Update::Folder {
                        id: 1,
                        parent: Some(0),
                        name: vec![65],
                    },
                    Update::Values {
                        id: 0,
                        value: Values {
                            done: true,
                            ..Default::default()
                        },
                    },
                    Update::Values {
                        id: 1,
                        value: Values {
                            bytes: 3,
                            files: 1,
                            ..Default::default()
                        },
                    },
                ],
            })
            .unwrap();
        assert_eq!(model.folders[0].state(), State::Calculating);
        assert_eq!((model.folders[0].bytes, model.folders[0].files), (3, 1));
        model
            .apply(Batch {
                reset: false,
                updates: vec![Update::Values {
                    id: 1,
                    value: Values {
                        bytes: 3,
                        files: 1,
                        missing: 1,
                        done: true,
                    },
                }],
            })
            .unwrap();
        assert_eq!(model.folders[0].state(), State::Partial);
        assert_eq!(model.folders[0].bytes, 3);
        let mail = Mailbox::default();
        for bytes in 0..10_000 {
            mail.publish(Batch {
                reset: false,
                updates: vec![Update::Values {
                    id: 0,
                    value: Values {
                        bytes,
                        ..Default::default()
                    },
                }],
            })
            .unwrap();
        }
        let batch = mail.drain().unwrap();
        assert_eq!(batch.updates.len(), 1);
        let Update::Values { value, .. } = batch.updates[0] else {
            panic!("최신 집계")
        };
        assert_eq!(value.bytes, 9999);
        assert!(mail.drain().is_none());
    }
    #[test]
    fn navigation_survives_updates_and_hands_paths_to_sorted_completed_view() {
        let root =
            std::env::temp_dir().join(format!("diskweave-preview-nav-{}", std::process::id()));
        std::fs::create_dir_all(root.join("z/deep")).unwrap();
        std::fs::create_dir_all(root.join("a")).unwrap();
        let mut model = Model::new(root.clone());
        model
            .apply(Batch {
                reset: true,
                updates: vec![
                    Update::Folder {
                        id: 0,
                        parent: None,
                        name: vec![],
                    },
                    Update::Folder {
                        id: 1,
                        parent: Some(0),
                        name: vec![122],
                    },
                    Update::Folder {
                        id: 2,
                        parent: Some(1),
                        name: "deep".encode_utf16().collect(),
                    },
                ],
            })
            .unwrap();
        let mut view = View::default();
        view.sync(&model);
        view.open(&model, 1);
        view.toggle(1);
        view.selected = 2;
        model
            .apply(Batch {
                reset: false,
                updates: vec![Update::Folder {
                    id: 3,
                    parent: Some(0),
                    name: vec![97],
                }],
            })
            .unwrap();
        view.sync(&model);
        assert_eq!(view.folder, 1);
        assert_eq!(view.selected, 2);
        assert_eq!(model.folders[0].children, vec![1, 3]);
        let result = super::super::scan(&root, || false).unwrap();
        let final_view = view.complete(&model, &result);
        assert_eq!(result.folders[final_view.folder()].path, root.join("z"));
        let crate::explorer::Item::Folder(id) = final_view.selected() else {
            panic!("폴더 선택 보존")
        };
        assert_eq!(result.folders[id].path, root.join("z/deep"));
        assert!(final_view.is_expanded(result.folder_index(std::path::Path::new("z")).unwrap()));
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn helper_stream_rejects_escape_unknown_parent_duplicate_reset_and_overflow() {
        let root = PathBuf::from("C:\\preview");
        for bad in [
            Update::Folder {
                id: 1,
                parent: Some(99),
                name: vec![65],
            },
            Update::Folder {
                id: 1,
                parent: Some(0),
                name: "..".encode_utf16().collect(),
            },
            Update::Values {
                id: 9,
                value: Values::default(),
            },
        ] {
            let mut stream = ValidatedStream::new(root.clone());
            stream
                .accept(Batch {
                    reset: true,
                    updates: vec![Update::Folder {
                        id: 0,
                        parent: None,
                        name: vec![],
                    }],
                })
                .unwrap();
            assert_eq!(
                stream
                    .accept(Batch {
                        reset: false,
                        updates: vec![bad]
                    })
                    .unwrap_err()
                    .kind(),
                io::ErrorKind::InvalidData
            );
        }
        let mut stream = ValidatedStream::new(root);
        stream
            .accept(Batch {
                reset: true,
                updates: vec![Update::Folder {
                    id: 0,
                    parent: None,
                    name: vec![],
                }],
            })
            .unwrap();
        assert!(stream
            .accept(Batch {
                reset: true,
                updates: vec![]
            })
            .is_err());
        assert!(stream
            .accept(Batch {
                reset: false,
                updates: vec![
                    Update::Folder {
                        id: 1,
                        parent: Some(0),
                        name: vec![65]
                    },
                    Update::Values {
                        id: 0,
                        value: Values {
                            bytes: u64::MAX,
                            ..Default::default()
                        }
                    },
                    Update::Values {
                        id: 1,
                        value: Values {
                            bytes: 1,
                            ..Default::default()
                        }
                    },
                ]
            })
            .is_err());
    }
    #[test]
    fn discovered_folders_are_visible_before_return_and_match_final_totals() {
        let root = std::env::temp_dir().join(format!("diskweave-preview-{}", std::process::id()));
        std::fs::create_dir_all(root.join("child/deep")).unwrap();
        std::fs::write(root.join("child/deep/known"), b"12345").unwrap();
        let mut model = Model::new(root.clone());
        let mut discovered = false;
        let result = super::super::scan_preview(
            &root,
            true,
            super::super::Parallelism::Two,
            || false,
            |_| {},
            &mut |batch| {
                model.apply(batch)?;
                discovered |= model.folders.iter().any(|f| f.path == root.join("child"));
                Ok(())
            },
        )
        .unwrap();
        assert!(discovered);
        assert_eq!(model.folders[0].bytes, 5);
        assert_eq!(model.folders[0].files, 1);
        assert_eq!(model.folders[0].folders, 2);
        assert_eq!(model.folders[0].state(), State::Complete);
        for folder in &model.folders {
            let final_folder = result
                .folders
                .iter()
                .find(|f| f.path == folder.path)
                .unwrap();
            assert_eq!(
                (folder.bytes, folder.files, folder.folders),
                (
                    final_folder.logical_bytes,
                    final_folder.file_count,
                    final_folder.folder_count
                )
            );
        }
        std::fs::remove_dir_all(root).unwrap();
    }
}
