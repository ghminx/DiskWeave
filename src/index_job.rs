use crate::index_store::{Catalog, Loaded, Phase, Repository};
use crate::locations::Locations;
use std::{
    io,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc, Mutex,
    },
    thread,
};

#[derive(Default, Debug)]
pub enum Status {
    #[default]
    Dormant,
    Loading,
    Saving,
    Ready,
    Disabled,
    ReadOnly,
    Failed(String),
}
#[derive(Default, Debug, PartialEq, Eq)]
pub enum CloseState {
    #[default]
    Open,
    Waiting,
    Saved,
    Discarded,
}
impl CloseState {
    pub fn request(&mut self) {
        if *self == Self::Open {
            *self = Self::Waiting;
        }
    }
    pub fn resume(&mut self) {
        *self = Self::Open;
    }
    pub fn discard(&mut self) {
        *self = Self::Discarded;
    }
    pub fn poll(&mut self, index: &IndexController, locations: &Locations) -> bool {
        if *self == Self::Waiting && index.close_ready(locations) {
            *self = Self::Saved;
        }
        matches!(self, Self::Saved | Self::Discarded)
    }
}
#[derive(Clone, PartialEq, Eq)]
struct Key {
    selected: Option<u64>,
    entries: Vec<(u64, u64, bool, bool, u64)>,
}
impl Key {
    fn of(locations: &Locations) -> Self {
        Self {
            selected: locations.selected_id(),
            entries: locations
                .entries
                .iter()
                .map(|e| {
                    (
                        e.id,
                        e.generation,
                        e.enabled,
                        e.stale,
                        e.checkpoint_revision,
                    )
                })
                .collect(),
        }
    }
}
enum Action {
    Load,
    Save(Catalog, Key),
    Configure(bool),
    Clear,
}
struct Request {
    id: u64,
    action: Action,
    cancel: Arc<AtomicBool>,
}
struct Reply {
    id: u64,
    result: io::Result<Option<Loaded>>,
    saved: Option<Key>,
    clear: bool,
    enabled: bool,
    writable: bool,
    notice: Option<String>,
}
#[derive(Default)]
pub struct IndexController {
    pub saving_paused: bool,
    pub status: Status,
    pub enabled: bool,
    pub writable: bool,
    pub notice: Option<String>,
    pub root: PathBuf,
    sender: Option<mpsc::Sender<Request>>,
    receiver: Option<mpsc::Receiver<Reply>>,
    worker: Option<thread::JoinHandle<()>>,
    stage: Arc<Mutex<String>>,
    active: Option<(u64, Arc<AtomicBool>)>,
    pending: Option<Request>,
    latest: u64,
    saved: Option<Key>,
    ready: bool,
}
impl IndexController {
    pub fn start(root: PathBuf) -> Self {
        let (tx, commands) = mpsc::channel::<Request>();
        let (replies, rx) = mpsc::channel();
        let stage = Arc::new(Mutex::new(String::new()));
        let progress = Arc::clone(&stage);
        let path = root.clone();
        let worker = thread::spawn(move || {
            let mut repository: Option<Repository> = None;
            for request in commands {
                let cancelled = || request.cancel.load(Ordering::Relaxed);
                let report = |phase: Phase| {
                    *progress.lock().unwrap() = match phase {
                        Phase::Location { current, total } => {
                            format!("분석 위치 {current}/{total}")
                        }
                        Phase::Publishing => "저장 마무리 중".into(),
                    };
                };
                let mut saved = None;
                let clear = matches!(request.action, Action::Clear);
                let result = (|| {
                    if repository.is_none() {
                        repository = Some(Repository::open(&path)?);
                    }
                    let store = repository.as_mut().unwrap();
                    match request.action {
                        Action::Load => store.load_with_progress(cancelled, report).map(Some),
                        Action::Save(catalog, key) => {
                            store.save(&catalog, cancelled, report)?;
                            saved = Some(key);
                            Ok(None)
                        }
                        Action::Configure(enabled) => {
                            store.configure(enabled, cancelled)?;
                            store.load(cancelled).map(Some)
                        }
                        Action::Clear => {
                            store.clear(cancelled, report)?;
                            Ok(None)
                        }
                    }
                })();
                let enabled = repository.as_ref().is_some_and(Repository::enabled);
                let writable = repository.as_ref().is_some_and(Repository::writable);
                let notice = repository
                    .as_ref()
                    .and_then(|s| s.notice().map(str::to_owned));
                if replies
                    .send(Reply {
                        id: request.id,
                        result,
                        saved,
                        clear,
                        enabled,
                        writable,
                        notice,
                    })
                    .is_err()
                {
                    break;
                }
            }
        });
        let mut controller = Self::default();
        controller.root = root;
        controller.sender = Some(tx);
        controller.receiver = Some(rx);
        controller.worker = Some(worker);
        controller.stage = stage;
        controller.request(Action::Load);
        controller
    }
    pub fn start_default() -> Self {
        match default_path() {
            Ok(path) => Self::start(path),
            Err(error) => {
                let mut controller = Self::default();
                controller.status =
                    Status::Failed(format!("저장 위치를 확인할 수 없습니다: {error}"));
                controller
            }
        }
    }
    fn request(&mut self, action: Action) {
        if self.sender.is_none() {
            return;
        }
        if matches!(action, Action::Load | Action::Configure(_)) {
            self.ready = false;
        }
        self.latest += 1;
        self.status = if matches!(action, Action::Load | Action::Configure(_)) {
            Status::Loading
        } else {
            Status::Saving
        };
        *self.stage.lock().unwrap() = String::new();
        if let Some((_, cancel)) = &self.active {
            cancel.store(true, Ordering::Relaxed);
        }
        self.pending = Some(Request {
            id: self.latest,
            action,
            cancel: Arc::new(AtomicBool::new(false)),
        });
        self.dispatch();
    }
    fn dispatch(&mut self) {
        if self.active.is_none() {
            if let Some(request) = self.pending.take() {
                self.active = Some((request.id, Arc::clone(&request.cancel)));
                if self.sender.as_ref().unwrap().send(request).is_err() {
                    self.active = None;
                    self.status = Status::Failed("저장 작업을 시작할 수 없습니다".into());
                }
            }
        }
    }
    pub fn configure(&mut self, enabled: bool) {
        self.request(Action::Configure(enabled));
    }
    pub fn clear(&mut self) {
        self.request(Action::Clear);
    }
    pub fn retry(&mut self, locations: &Locations) {
        if self.sender.is_none() && matches!(self.status, Status::Failed(_)) {
            *self = Self::start_default();
            return;
        }
        if self.ready && self.enabled && self.writable {
            self.request(Action::Save(locations.export_catalog(), Key::of(locations)));
        } else {
            self.request(Action::Load);
        }
    }
    pub fn tick(&mut self, locations: &mut Locations) -> bool {
        let mut changed = false;
        while let Some(reply) = self.receiver.as_ref().and_then(|rx| rx.try_recv().ok()) {
            self.active = None;
            if reply.id != self.latest {
                self.dispatch();
                continue;
            }
            self.enabled = reply.enabled;
            self.writable = reply.writable;
            self.notice = reply.notice;
            match reply.result {
                Ok(loaded) => {
                    if let Some(loaded) = loaded {
                        let live = locations.entries.iter().any(|e| !e.restored);
                        match locations.restore_catalog(&loaded.catalog) {
                            Ok(value) => changed |= value,
                            Err(error) => {
                                self.status = Status::Failed(error.to_string());
                                continue;
                            }
                        }
                        self.enabled = loaded.enabled;
                        self.writable = loaded.writable;
                        if loaded.notice.is_some() {
                            self.notice = loaded.notice;
                        }
                        self.saved = if live && self.enabled && self.writable {
                            None
                        } else {
                            Some(Key::of(locations))
                        };
                    } else if reply.clear {
                        self.saved = Some(Key::of(locations));
                    } else {
                        self.saved = reply.saved;
                    }
                    self.ready = true;
                    self.status = if !self.enabled {
                        Status::Disabled
                    } else if !self.writable {
                        Status::ReadOnly
                    } else {
                        Status::Ready
                    };
                }
                Err(error) => {
                    self.status = Status::Failed(error.to_string());
                }
            }
            self.dispatch();
        }
        if self.ready
            && !self.saving_paused
            && self.enabled
            && self.writable
            && !self.busy()
            && matches!(self.status, Status::Ready)
            && self.saved.as_ref() != Some(&Key::of(locations))
        {
            self.request(Action::Save(locations.export_catalog(), Key::of(locations)));
        }
        changed
    }
    pub fn busy(&self) -> bool {
        self.active.is_some() || self.pending.is_some()
    }
    pub fn close_ready(&self, locations: &Locations) -> bool {
        if self.busy() || matches!(self.status, Status::Failed(_)) {
            return false;
        }
        matches!(self.status, Status::Dormant)
            || !self.enabled
            || !self.writable
            || self.saved.as_ref() == Some(&Key::of(locations))
    }
    pub fn label(&self) -> String {
        match &self.status {
            Status::Dormant => String::new(),
            Status::Loading => "분석 목록 불러오는 중…".into(),
            Status::Saving => "분석 목록 저장 중…".into(),
            Status::Ready if self.saving_paused => "추적 중 저장 대기 · 중지·종료할 때 저장".into(),
            Status::Ready => "분석 목록 자동 저장".into(),
            Status::Disabled => "분석 목록 자동 저장 꺼짐".into(),
            Status::ReadOnly => "다른 창에서 목록 저장 중 · 이 창은 조회만 가능".into(),
            Status::Failed(error) => format!("분석 목록 저장·복원 오류: {error}"),
        }
    }
    pub fn stage(&self) -> String {
        self.stage.lock().unwrap().clone()
    }
    pub fn shutdown(&mut self) {
        if let Some((_, cancel)) = &self.active {
            cancel.store(true, Ordering::Relaxed);
        }
        self.pending = None;
        self.sender = None;
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        self.active = None;
    }
}
impl Drop for IndexController {
    fn drop(&mut self) {
        if let Some((_, cancel)) = &self.active {
            cancel.store(true, Ordering::Relaxed);
        }
        self.sender = None;
    }
}
pub(crate) fn default_path() -> io::Result<PathBuf> {
    use std::os::windows::ffi::OsStringExt;
    use windows_sys::Win32::{
        System::Com::CoTaskMemFree,
        UI::Shell::{FOLDERID_LocalAppData, SHGetKnownFolderPath},
    };
    let mut value = std::ptr::null_mut();
    // Windows가 할당한 경로를 복사한 뒤 동일 COM 할당자로 해제.
    let result = unsafe {
        SHGetKnownFolderPath(&FOLDERID_LocalAppData, 0, std::ptr::null_mut(), &mut value)
    };
    if result < 0 {
        unsafe {
            CoTaskMemFree(value.cast());
        }
        return Err(io::Error::other(format!(
            "로컬 앱 데이터 경로 조회 실패: {result:#x}"
        )));
    }
    let mut len = 0;
    unsafe {
        while *value.add(len) != 0 {
            len += 1;
        }
        let path = PathBuf::from(std::ffi::OsString::from_wide(std::slice::from_raw_parts(
            value, len,
        )));
        CoTaskMemFree(value.cast());
        Ok(path.join("DiskWeave").join("Index"))
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        time::{Duration, Instant, SystemTime, UNIX_EPOCH},
    };
    #[test]
    fn pausing_automatic_writes_keeps_dirty_results_until_tracking_stops() {
        let p = root("paused");
        let source = p.join("source");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("a"), b"12345").unwrap();
        let mut locations = Locations::default();
        locations.start(source).unwrap();
        let mut job = IndexController::start(p.join("cache"));
        job.saving_paused = true;
        settle(&mut job, &mut locations);
        assert!(!job.close_ready(&locations));
        job.saving_paused = false;
        settle(&mut job, &mut locations);
        assert!(job.close_ready(&locations));
        job.shutdown();
        let mut store = Repository::open(&p.join("cache")).unwrap();
        assert_eq!(
            store.load(|| false).unwrap().catalog.locations[0]
                .record
                .as_ref()
                .unwrap()
                .scan
                .folders[0]
                .logical_bytes,
            5
        );
        drop(store);
        fs::remove_dir_all(p).unwrap();
    }
    fn root(label: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "diskweave-index-job-{label}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&p).unwrap();
        p
    }
    fn settle(job: &mut IndexController, locations: &mut Locations) {
        let t = Instant::now();
        while t.elapsed() < Duration::from_secs(5) {
            locations.poll();
            job.tick(locations);
            if !locations.is_running() && !job.busy() {
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(!job.busy());
        assert!(!matches!(job.status, Status::Failed(_)));
    }
    #[test]
    fn automatic_jobs_reopen_searchable_locations_and_persist_removal() {
        let p = root("restart");
        let source = p.join("source");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("known"), b"1234567").unwrap();
        let mut locations = Locations::default();
        locations.start(source.clone()).unwrap();
        let mut job = IndexController::start(p.join("cache"));
        settle(&mut job, &mut locations);
        assert!(job.close_ready(&locations));
        job.shutdown();
        fs::remove_dir_all(source).unwrap();
        let mut reopened = Locations::default();
        let mut job = IndexController::start(p.join("cache"));
        settle(&mut job, &mut reopened);
        assert!(reopened.entries[0].restored);
        assert_eq!(
            reopened.completed().unwrap().result.folders[0].logical_bytes,
            7
        );
        reopened.remove(reopened.selected_id().unwrap()).unwrap();
        settle(&mut job, &mut reopened);
        job.shutdown();
        let mut repository = crate::index_store::Repository::open(&p.join("cache")).unwrap();
        assert!(repository
            .load(|| false)
            .unwrap()
            .catalog
            .locations
            .is_empty());
        drop(repository);
        fs::remove_dir_all(p).unwrap();
    }
    #[test]
    fn clearing_during_restore_ignores_the_old_reply_and_keeps_live_results() {
        let p = root("clear");
        let source = p.join("source");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("a"), b"12345").unwrap();
        let mut locations = Locations::default();
        locations.start(source.clone()).unwrap();
        let mut first = IndexController::start(p.join("cache"));
        settle(&mut first, &mut locations);
        first.shutdown();
        let mut empty = Locations::default();
        let mut job = IndexController::start(p.join("cache"));
        job.clear();
        settle(&mut job, &mut empty);
        assert!(empty.entries.is_empty());
        job.shutdown();
        let mut job = IndexController::start(p.join("cache"));
        settle(&mut job, &mut locations);
        job.clear();
        settle(&mut job, &mut locations);
        assert_eq!(
            locations.completed().unwrap().result.folders[0].logical_bytes,
            5
        );
        assert!(job.close_ready(&locations));
        job.shutdown();
        let mut store = Repository::open(&p.join("cache")).unwrap();
        assert!(store.load(|| false).unwrap().catalog.locations.is_empty());
        assert_eq!(fs::read(source.join("a")).unwrap(), b"12345");
        drop(store);
        fs::remove_dir_all(p).unwrap();
    }
    #[test]
    fn disabling_during_restore_and_second_window_never_publish_live_changes() {
        let p = root("role");
        let source = p.join("source");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("a"), b"12345").unwrap();
        let mut locations = Locations::default();
        locations.start(source).unwrap();
        let mut owner = IndexController::start(p.join("cache"));
        settle(&mut owner, &mut locations);
        let mut other_locations = Locations::default();
        let mut other = IndexController::start(p.join("cache"));
        settle(&mut other, &mut other_locations);
        assert!(matches!(other.status, Status::ReadOnly));
        other_locations
            .remove(other_locations.selected_id().unwrap())
            .unwrap();
        other.tick(&mut other_locations);
        assert!(other.close_ready(&other_locations));
        other.shutdown();
        owner.shutdown();
        let mut reopened = Locations::default();
        let mut off = IndexController::start(p.join("cache"));
        off.configure(false);
        settle(&mut off, &mut reopened);
        assert!(reopened.entries.is_empty());
        assert!(!off.enabled);
        assert!(off.close_ready(&reopened));
        off.configure(true);
        settle(&mut off, &mut reopened);
        assert!(reopened.entries[0].restored);
        off.shutdown();
        fs::remove_dir_all(p).unwrap();
    }
    #[test]
    fn close_waits_for_latest_result_and_save_failure_allows_retry() {
        use std::os::windows::fs::OpenOptionsExt;
        let p = root("close");
        let source = p.join("source");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("a"), b"12345").unwrap();
        let mut locations = Locations::default();
        locations.start(source.clone()).unwrap();
        let mut job = IndexController::start(p.join("cache"));
        settle(&mut job, &mut locations);
        let lock = fs::File::options()
            .read(true)
            .share_mode(1)
            .open(p.join("cache").join(crate::index_store::CATALOG_NAME))
            .unwrap();
        fs::write(source.join("a"), b"123456789").unwrap();
        locations.start(source).unwrap();
        let time = Instant::now();
        while locations.is_running() && time.elapsed() < Duration::from_secs(5) {
            locations.poll();
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(!job.close_ready(&locations));
        job.tick(&mut locations);
        let time = Instant::now();
        while job.busy() && time.elapsed() < Duration::from_secs(5) {
            job.tick(&mut locations);
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(matches!(job.status, Status::Failed(_)));
        assert!(!job.close_ready(&locations));
        drop(lock);
        job.retry(&locations);
        settle(&mut job, &mut locations);
        assert!(job.close_ready(&locations));
        job.shutdown();
        let mut store = Repository::open(&p.join("cache")).unwrap();
        assert_eq!(
            store.load(|| false).unwrap().catalog.locations[0]
                .record
                .as_ref()
                .unwrap()
                .scan
                .folders[0]
                .logical_bytes,
            9
        );
        drop(store);
        fs::remove_dir_all(p).unwrap();
    }
    #[test]
    fn close_flow_waits_and_can_return_or_discard_without_claiming_saved() {
        let p = root("exit");
        let mut locations = Locations::default();
        let mut job = IndexController::start(p.join("cache"));
        let mut closing = CloseState::default();
        closing.request();
        assert!(!closing.poll(&job, &locations));
        assert_eq!(closing, CloseState::Waiting);
        closing.resume();
        assert_eq!(closing, CloseState::Open);
        closing.request();
        settle(&mut job, &mut locations);
        assert!(closing.poll(&job, &locations));
        assert_eq!(closing, CloseState::Saved);
        job.shutdown();
        let blocker = p.join("blocked");
        fs::write(&blocker, b"not a directory").unwrap();
        let mut failed = IndexController::start(blocker);
        let time = Instant::now();
        while failed.busy() && time.elapsed() < Duration::from_secs(5) {
            failed.tick(&mut locations);
            std::thread::sleep(Duration::from_millis(1));
        }
        let mut closing = CloseState::default();
        closing.request();
        assert!(!closing.poll(&failed, &locations));
        closing.discard();
        assert_eq!(closing, CloseState::Discarded);
        failed.shutdown();
        fs::remove_dir_all(p).unwrap();
    }
}
