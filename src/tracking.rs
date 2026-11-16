use crate::snapshot::SavedScan;
use std::{
    io,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::SystemTime,
};
#[derive(Default)]
pub enum Status {
    #[default]
    Stopped,
    Starting,
    Following,
    Failed(String),
}
#[derive(Default)]
struct Output {
    record: Option<SavedScan>,
    finished: Option<Result<(), String>>,
}
struct Active {
    cancel: Arc<AtomicBool>,
    output: Arc<Mutex<Output>>,
    root: PathBuf,
    detailed: bool,
}
#[derive(Default)]
pub struct Controller {
    progress: Arc<Mutex<crate::engine::Progress>>,
    pub location: Option<u64>,
    pub status: Status,
    pub checked: Option<SystemTime>,
    pub confirmed_after: Option<SystemTime>,
    pub excluded: usize,
    active: Option<Active>,
}
impl Controller {
    pub fn start(
        &mut self,
        id: u64,
        root: PathBuf,
        detailed: bool,
        resume: Option<Arc<SavedScan>>,
    ) -> io::Result<()> {
        crate::usn_native::target(&root)?;
        let progress = Arc::new(Mutex::new(crate::engine::Progress::default()));
        let worker_progress = Arc::clone(&progress);
        self.start_with(
            id,
            root,
            detailed,
            resume,
            move |root, detailed, resume, cancel, publish| {
                crate::ntfs_helper::watch(root, detailed, resume, cancel, publish, |event| {
                    *worker_progress.lock().unwrap() = event;
                })
            },
        )?;
        self.progress = progress;
        Ok(())
    }
    pub fn start_with(
        &mut self,
        id: u64,
        root: PathBuf,
        detailed: bool,
        resume: Option<Arc<SavedScan>>,
        run: impl FnOnce(
                &Path,
                bool,
                Option<Arc<SavedScan>>,
                &dyn Fn() -> bool,
                &mut dyn FnMut(SavedScan),
            ) -> io::Result<()>
            + Send
            + 'static,
    ) -> io::Result<()> {
        if self.running() {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "다른 위치를 추적하려면 현재 추적을 중지하세요",
            ));
        }
        let cancel = Arc::new(AtomicBool::new(false));
        let output = Arc::new(Mutex::new(Output::default()));
        let worker_cancel = Arc::clone(&cancel);
        let worker_output = Arc::clone(&output);
        let path = root.clone();
        std::thread::spawn(move || {
            let stopped = || worker_cancel.load(Ordering::Relaxed);
            let result = run(&path, detailed, resume, &stopped, &mut |record| {
                if !stopped() {
                    worker_output.lock().unwrap().record = Some(record);
                }
            });
            if !stopped() {
                worker_output.lock().unwrap().finished = Some(result.map_err(|e| e.to_string()));
            }
        });
        self.location = Some(id);
        self.checked = None;
        self.confirmed_after = None;
        self.status = Status::Starting;
        self.excluded = 0;
        self.active = Some(Active {
            cancel,
            output,
            root,
            detailed,
        });
        Ok(())
    }
    pub fn running(&self) -> bool {
        self.active.is_some()
    }
    pub fn stop(&mut self) {
        if let Some(active) = self.active.take() {
            active.cancel.store(true, Ordering::Relaxed);
        }
        self.status = Status::Stopped;
    }
    pub fn poll(&mut self) -> Option<SavedScan> {
        let active = self.active.as_ref()?;
        let mut output = active.output.lock().unwrap();
        let record = output.record.take();
        let mut finish = output.finished.take();
        drop(output);
        let record = record.filter(|r| {
            let valid = r.scan.folders[0].path == active.root
                && r.scan.detailed == active.detailed
                && r.tracking
                    .as_ref()
                    .is_some_and(|c| c.validate(&r.scan).is_ok());
            if !valid {
                finish = Some(Err("변경 추적 응답 대상·기준점 불일치".into()));
            }
            valid
        });
        if let Some(record) = &record {
            self.excluded = record.tracking.as_ref().map_or(0, |c| c.unavailable.len());
            self.checked = Some(record.finished);
            self.confirmed_after = record.tracking.as_ref().unwrap().confirmed_after;
            self.status = Status::Following;
        }
        if let Some(result) = finish {
            self.active
                .take()
                .unwrap()
                .cancel
                .store(true, Ordering::Relaxed);
            self.status = match result {
                Ok(()) => Status::Stopped,
                Err(error) => Status::Failed(error),
            };
        }
        record
    }
    pub fn label(&self) -> &str {
        match &self.status {
            Status::Stopped => "실시간 갱신 중지됨",
            Status::Starting => "권한 승인·기준 수집·변경 내용 확인 중…",
            Status::Following if self.excluded > 0 && self.applying() => "변경 반영 중 · 일부 제외",
            Status::Following if self.excluded > 0 => "갱신 중 · 일부 제외",
            Status::Following if self.applying() => "변경 내용 반영 중",
            Status::Following => "변경 확인 중",
            Status::Failed(_) => "실시간 갱신 중단 · 갱신 필요",
        }
    }
    pub fn applying(&self) -> bool {
        self.running() && self.progress.lock().unwrap().stage == crate::engine::Stage::Metadata
    }
    pub fn progress(&self) -> crate::scanner::ScanProgress {
        self.progress.lock().unwrap().counts
    }
}
impl Drop for Controller {
    fn drop(&mut self) {
        if let Some(active) = &self.active {
            active.cancel.store(true, Ordering::Relaxed);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        sync::mpsc,
        time::{Duration, Instant, SystemTime},
    };
    fn record(path: &Path) -> SavedScan {
        let mut r = SavedScan::new(
            Arc::new(crate::scanner::scan(path, || false).unwrap()),
            SystemTime::now(),
            SystemTime::now(),
            Duration::ZERO,
        );
        r.tracking = Some(crate::usn::Checkpoint {
            version: 1,
            volume: "test".into(),
            root: 1,
            journal: 7,
            next: 100,
            folders: vec![1],
            files: vec![],
            excluded: vec![],
            pending: vec![],
            unavailable: vec![],
            confirmed_after: None,
        });
        r
    }
    #[test]
    fn partial_following_state_is_visible_and_bad_records_do_not_replace_it() {
        let root =
            std::env::temp_dir().join(format!("diskweave-partial-ui-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        let mut value = record(&root);
        let scan = Arc::get_mut(&mut value.scan).unwrap();
        scan.error_count = 1;
        scan.folders[0].access = crate::scanner::FolderAccess::Partial;
        let state = value.tracking.as_mut().unwrap();
        state.version = 3;
        state.unavailable.push(crate::usn::Unavailable {
            path: "locked.bin".encode_utf16().collect(),
            parent: 1,
            previous_id: None,
            directory: false,
            reason: crate::usn::UnavailableReason::Os(32),
            listing: false,
            checked: SystemTime::now(),
        });
        let cleanup = root.clone();
        let (publish_bad, wait_bad) = mpsc::channel();
        let mut controller = Controller::default();
        controller
            .start_with(
                1,
                root.clone(),
                false,
                None,
                move |_, _, _, cancel, publish| {
                    publish(value);
                    wait_bad.recv().unwrap();
                    let mut bad = record(&root);
                    bad.tracking.as_mut().unwrap().root = 0;
                    publish(bad);
                    while !cancel() {
                        std::thread::sleep(Duration::from_millis(1));
                    }
                    Ok(())
                },
            )
            .unwrap();
        let until = Instant::now() + Duration::from_secs(2);
        loop {
            if controller.poll().is_some() {
                break;
            }
            assert!(Instant::now() < until);
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(controller.label().contains("일부 제외"));
        publish_bad.send(()).unwrap();
        let until = Instant::now() + Duration::from_secs(2);
        while !matches!(controller.status, Status::Failed(_)) {
            assert!(controller.poll().is_none());
            assert!(Instant::now() < until);
            std::thread::sleep(Duration::from_millis(1));
        }
        controller.stop();
        fs::remove_dir_all(cleanup).unwrap();
    }
    #[test]
    fn tracking_start_stop_and_late_responses_cannot_replace_the_current_session() {
        let root = std::env::temp_dir().join(format!("diskweave-tracking-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        let (ready, wait) = mpsc::channel();
        let mut controller = Controller::default();
        let path = root.clone();
        controller
            .start_with(1, root.clone(), false, None, move |_, _, _, _, publish| {
                wait.recv().unwrap();
                publish(record(&path));
                Ok(())
            })
            .unwrap();
        controller.stop();
        ready.send(()).unwrap();
        std::thread::sleep(Duration::from_millis(5));
        assert!(controller.poll().is_none());
        assert!(matches!(controller.status, Status::Stopped));
        let path = root.clone();
        controller
            .start_with(2, root.clone(), false, None, move |_, _, _, _, publish| {
                publish(record(&path));
                Err(io::Error::other("분리됨"))
            })
            .unwrap();
        let time = Instant::now();
        while controller.running() && time.elapsed() < Duration::from_secs(3) {
            controller.poll();
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(matches!(controller.status, Status::Failed(_)));
        assert_eq!(controller.location, Some(2));
        fs::remove_dir_all(root).unwrap();
    }
}
