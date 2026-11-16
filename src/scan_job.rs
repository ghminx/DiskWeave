#[cfg(test)]
use crate::scanner;
use crate::scanner::{ScanProgress, ScanResult};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

pub struct CompletedScan {
    pub tracking: Option<crate::usn::Checkpoint>,
    pub engine: Option<crate::engine::Report>,
    pub path: PathBuf,
    pub result: Arc<ScanResult>,
    pub elapsed: Duration,
    pub started: SystemTime,
    pub finished: SystemTime,
}
impl CompletedScan {
    pub fn saved_result(&self) -> crate::snapshot::SavedScan {
        let mut record = crate::snapshot::SavedScan::new(
            Arc::clone(&self.result),
            self.started,
            self.finished,
            self.elapsed,
        );
        record.engine = self.engine.clone();
        record.tracking = self.tracking.clone();
        record
    }
}

#[derive(Default)]
pub enum ScanStatus {
    #[default]
    Idle,
    Running,
    Cancelling,
    Completed,
    Cancelled,
    Failed(String),
}

struct ActiveScan {
    preview_mail: crate::scanner::preview::Mailbox,
    preview_error: Option<String>,
    path: PathBuf,
    started: Instant,
    clock: SystemTime,
    cancelled: Arc<AtomicBool>,
    progress: Arc<Mutex<crate::engine::Progress>>,
    receiver: Receiver<(SystemTime, std::io::Result<crate::engine::Outcome>)>,
}

#[derive(Default)]
pub struct ScanController {
    preview: Option<crate::scanner::preview::Model>,
    settings: crate::metadata::ScanSettings,
    active: Option<ActiveScan>,
    completed: Option<CompletedScan>,
    status: ScanStatus,
}

impl ScanController {
    pub(crate) fn from_saved(record: &crate::snapshot::SavedScan) -> Self {
        Self {
            settings: crate::metadata::ScanSettings {
                parallelism: Default::default(),
                detailed: record.scan.detailed,
                method: record.engine.as_ref().map_or_default(|r| r.requested),
                fast_mode: record
                    .engine
                    .as_ref()
                    .and_then(|r| r.fast_mode)
                    .unwrap_or_default(),
            },
            completed: Some(CompletedScan {
                tracking: record.tracking.clone(),
                engine: record.engine.clone(),
                path: record.scan.folders[0].path.clone(),
                result: Arc::clone(&record.scan),
                elapsed: record.elapsed,
                started: record.started,
                finished: record.finished,
            }),
            status: ScanStatus::Completed,
            ..Default::default()
        }
    }
    pub(crate) fn seed_saved(&mut self, record: &crate::snapshot::SavedScan) {
        if self.completed.is_none() {
            self.completed = Self::from_saved(record).completed;
        }
    }
    #[cfg(test)]
    pub fn parallelism(&self) -> crate::scanner::Parallelism {
        self.settings.parallelism
    }
    pub fn configure_parallelism(
        &mut self,
        value: crate::scanner::Parallelism,
    ) -> Result<(), &'static str> {
        if self.is_running() {
            return Err("스캔 중에는 동시 작업 수를 변경할 수 없습니다");
        }
        self.settings.parallelism = value;
        Ok(())
    }
    #[cfg(test)]
    pub fn method(&self) -> crate::engine::Method {
        self.settings.method
    }
    #[cfg(test)]
    pub fn fast_mode(&self) -> crate::engine::FastMode {
        self.settings.fast_mode
    }
    pub fn configure_fast_mode(
        &mut self,
        mode: crate::engine::FastMode,
    ) -> Result<(), &'static str> {
        if self.is_running() {
            return Err("스캔 중에는 고속 수집 방식을 변경할 수 없습니다");
        }
        self.settings.fast_mode = mode;
        Ok(())
    }
    pub fn configure_method(&mut self, method: crate::engine::Method) -> Result<(), &'static str> {
        if self.is_running() {
            return Err("스캔 중에는 방식을 변경할 수 없습니다");
        }
        self.settings.method = method;
        Ok(())
    }
    pub fn stage(&self) -> Option<crate::engine::Stage> {
        self.active
            .as_ref()
            .map(|a| a.progress.lock().unwrap().stage)
    }
    #[cfg(test)]
    pub fn details_requested(&self) -> bool {
        self.settings.detailed
    }
    pub fn configure(&mut self, detailed: bool) -> Result<(), &'static str> {
        if self.is_running() {
            return Err("스캔 중에는 수집 방식을 변경할 수 없습니다");
        }
        self.settings.detailed = detailed;
        Ok(())
    }
    pub fn start(&mut self, path: PathBuf) -> Result<(), &'static str> {
        if self.active.is_some() {
            return Err("스캔이 진행 중입니다");
        }
        let (sender, receiver) = mpsc::channel();
        let worker_path = path.clone();
        let cancelled = Arc::new(AtomicBool::new(false));
        let worker_cancelled = Arc::clone(&cancelled);
        let progress = Arc::new(Mutex::new(crate::engine::Progress::default()));
        let worker_progress = Arc::clone(&progress);
        let preview_mail = crate::scanner::preview::Mailbox::default();
        let worker_preview = preview_mail.clone();
        let settings = self.settings;
        let clock = SystemTime::now();
        std::thread::spawn(move || {
            let result = crate::engine::scan_selected(
                &worker_path,
                &settings,
                || worker_cancelled.load(Ordering::Relaxed),
                |update| *worker_progress.lock().unwrap() = update,
                Some(&mut |batch| worker_preview.publish(batch)),
                crate::ntfs_helper::scan_selected,
            );
            let _ = sender.send((SystemTime::now(), result));
        });
        self.status = ScanStatus::Running;
        self.preview = Some(crate::scanner::preview::Model::new(path.clone()));
        self.active = Some(ActiveScan {
            preview_mail,
            preview_error: None,
            path,
            started: Instant::now(),
            clock,
            cancelled,
            progress,
            receiver,
        });
        Ok(())
    }

    pub fn cancel(&mut self) {
        if let Some(active) = &self.active {
            active.cancelled.store(true, Ordering::Relaxed);
            self.status = ScanStatus::Cancelling;
        }
    }

    /// 새 완료 결과로 교체된 경우에만 true 반환.
    pub fn poll(&mut self) -> bool {
        let Some(active) = self.active.as_mut() else {
            return false;
        };
        if let Some(batch) = active.preview_mail.drain() {
            if let Err(error) = self.preview.as_mut().expect("활성 미리보기").apply(batch) {
                active.preview_error = Some(error.to_string());
                active.cancelled.store(true, Ordering::Relaxed);
            }
        }
        let (finished, result) = match active.receiver.try_recv() {
            Ok(result) => result,
            Err(TryRecvError::Empty) => return false,
            Err(TryRecvError::Disconnected) => {
                self.status = ScanStatus::Failed("스캔 작업이 예기치 않게 종료됐습니다".to_owned());
                self.active = None;
                self.preview = None;
                return false;
            }
        };
        // 완료 채널 수신 직전에 도착한 마지막 폴더 묶음까지 반영.
        if let Some(batch) = active.preview_mail.drain() {
            if let Err(error) = self.preview.as_mut().expect("활성 미리보기").apply(batch) {
                active.preview_error = Some(error.to_string());
            }
        }
        let active = self.active.take().expect("수신 중인 스캔 작업");
        if let Some(error) = active.preview_error {
            self.status = ScanStatus::Failed(error);
            self.preview = None;
            return false;
        }
        if active.cancelled.load(Ordering::Relaxed) {
            self.status = ScanStatus::Cancelled;
            self.preview = None;
            return false;
        }
        match result {
            Ok(result) => {
                self.status = ScanStatus::Completed;
                self.completed = Some(CompletedScan {
                    tracking: None,
                    path: active.path,
                    result: Arc::new(result.result),
                    engine: Some(result.report),
                    elapsed: active.started.elapsed(),
                    started: active.clock,
                    finished,
                });
                true
            }
            Err(error) => {
                self.status = ScanStatus::Failed(error.to_string());
                self.preview = None;
                false
            }
        }
    }

    pub fn is_running(&self) -> bool {
        self.active.is_some()
    }

    pub fn status(&self) -> &ScanStatus {
        &self.status
    }

    pub fn progress(&self) -> Option<ScanProgress> {
        self.active
            .as_ref()
            .map(|active| active.progress.lock().unwrap().counts)
    }

    pub fn elapsed(&self) -> Option<Duration> {
        self.active.as_ref().map(|active| active.started.elapsed())
    }

    pub fn completed(&self) -> Option<&CompletedScan> {
        self.completed.as_ref()
    }
    pub fn preview(&self) -> Option<&crate::scanner::preview::Model> {
        self.active
            .as_ref()
            .filter(|a| !a.cancelled.load(Ordering::Relaxed))?;
        self.preview.as_ref()
    }
    pub fn take_finished_preview(&mut self) -> Option<crate::scanner::preview::Model> {
        matches!(self.status, ScanStatus::Completed)
            .then(|| self.preview.take())
            .flatten()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    #[test]
    fn selected_engine_is_frozen_while_running_and_saved_with_fallback_reason() {
        let root =
            std::env::temp_dir().join(format!("diskweave-method-job-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        let mut job = ScanController::default();
        assert_eq!(job.method(), crate::engine::Method::Basic);
        job.configure_method(crate::engine::Method::Ntfs).unwrap();
        job.start(root.clone()).unwrap();
        assert!(job.configure_method(crate::engine::Method::Basic).is_err());
        while job.is_running() {
            job.poll();
            std::thread::yield_now();
        }
        let completed = job.completed().unwrap();
        assert_eq!(
            completed.engine.as_ref().unwrap().actual,
            crate::engine::Method::Basic
        );
        assert!(completed.saved_result().engine.unwrap().fallback.is_some());
        job.start(root.clone()).unwrap();
        job.cancel();
        while job.is_running() {
            job.poll();
            std::thread::yield_now();
        }
        assert!(matches!(job.status(), ScanStatus::Cancelled));
        assert!(job.completed().is_some());
        fs::remove_dir_all(root).unwrap();
    }
    use std::time::{SystemTime, UNIX_EPOCH};
    #[test]
    fn completed_scan_records_utc_interval_separately_from_save_time() {
        let root = std::env::temp_dir().join(format!("diskweave-scan-time-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("file"), [0; 3]).unwrap();
        let before = SystemTime::now();
        let mut job = ScanController::default();
        job.start(root.clone()).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while job.is_running() && std::time::Instant::now() < deadline {
            job.poll();
            std::thread::sleep(Duration::from_millis(1));
        }
        let complete = job.completed().unwrap();
        assert!(
            complete.started >= before
                && complete.finished >= complete.started
                && complete.finished <= SystemTime::now()
        );
        let saved = complete.saved_result();
        assert_eq!(saved.started, complete.started);
        assert_eq!(saved.finished, complete.finished);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn metadata_setting_applies_to_next_scan_and_cancel_preserves_previous_mode() {
        let root =
            std::env::temp_dir().join(format!("diskweave-scan-settings-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("data"), [0; 3]).unwrap();
        let mut controller = ScanController::default();
        assert!(controller.details_requested());
        controller
            .configure_parallelism(crate::scanner::Parallelism::Two)
            .unwrap();
        controller.configure(false).unwrap();
        controller
            .configure_fast_mode(crate::engine::FastMode::Mft)
            .unwrap();
        controller.start(root.clone()).unwrap();
        assert!(controller
            .configure_fast_mode(crate::engine::FastMode::Legacy)
            .is_err());
        assert_eq!(controller.fast_mode(), crate::engine::FastMode::Mft);
        assert!(controller.configure(true).is_err());
        assert!(controller
            .configure_parallelism(crate::scanner::Parallelism::Eight)
            .is_err());
        assert_eq!(controller.parallelism(), crate::scanner::Parallelism::Two);
        let deadline = Instant::now() + Duration::from_secs(5);
        while controller.is_running() && Instant::now() < deadline {
            controller.poll();
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(!controller.completed().unwrap().result.detailed);
        assert_eq!(
            controller.completed().unwrap().result.folders[0]
                .allocation
                .not_collected,
            1
        );
        controller.configure(true).unwrap();
        controller.start(root.clone()).unwrap();
        controller.cancel();
        while controller.is_running() && Instant::now() < deadline {
            controller.poll();
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(controller.details_requested());
        assert!(matches!(controller.status(), ScanStatus::Cancelled));
        assert!(!controller.completed().unwrap().result.detailed);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn completed_scan_becomes_available() {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root =
            std::env::temp_dir().join(format!("diskweave-job-{}-{stamp}", std::process::id()));
        fs::create_dir(&root).unwrap();
        fs::write(root.join("data.bin"), [1, 2, 3]).unwrap();

        let mut controller = ScanController::default();
        controller.start(root.clone()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while controller.is_running() && Instant::now() < deadline {
            controller.poll();
            std::thread::sleep(Duration::from_millis(2));
        }

        assert!(!controller.is_running());
        let completed = controller.completed().unwrap();
        assert_eq!(completed.result.folders[0].logical_bytes, 3);
        assert_eq!(completed.result.folders[0].file_count, 1);
        assert_eq!(completed.path, root);

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn cancelling_new_scan_keeps_previous_result() {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let base =
            std::env::temp_dir().join(format!("diskweave-cancel-{}-{stamp}", std::process::id()));
        let first = base.join("first");
        let second = base.join("second");
        fs::create_dir_all(&first).unwrap();
        fs::create_dir(&second).unwrap();
        fs::write(first.join("data.bin"), [1, 2, 3]).unwrap();
        fs::write(second.join("data.bin"), [4, 5, 6, 7]).unwrap();

        let mut controller = ScanController::default();
        controller.start(first.clone()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while controller.is_running() && Instant::now() < deadline {
            controller.poll();
            std::thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(controller.completed().unwrap().path, first);

        controller.start(second).unwrap();
        controller.cancel();
        let deadline = Instant::now() + Duration::from_secs(5);
        while controller.is_running() && Instant::now() < deadline {
            controller.poll();
            std::thread::sleep(Duration::from_millis(2));
        }
        assert!(!controller.is_running());
        assert_eq!(controller.completed().unwrap().path, first);
        assert_eq!(
            controller.completed().unwrap().result.folders[0].logical_bytes,
            3
        );

        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn progress_snapshot_updates_while_scan_runs() {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "diskweave-job-progress-{}-{stamp}",
            std::process::id()
        ));
        fs::create_dir(&root).unwrap();
        fs::write(root.join("first.bin"), [1]).unwrap();
        fs::write(root.join("second.bin"), [2]).unwrap();

        let mut controller = ScanController::default();
        controller.start(root.clone()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while controller.progress().unwrap().files_scanned < 2 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(
            controller.progress().unwrap(),
            scanner::ScanProgress {
                folders_scanned: 1,
                files_scanned: 2,
            }
        );

        controller.poll();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn failed_scan_reports_error_without_replacing_result() {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "diskweave-job-error-{}-{stamp}",
            std::process::id()
        ));
        fs::create_dir(&root).unwrap();

        let mut controller = ScanController::default();
        controller.start(root.clone()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while controller.is_running() && Instant::now() < deadline {
            controller.poll();
            std::thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(controller.completed().unwrap().path, root);

        controller.start(root.join("missing")).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while controller.is_running() && Instant::now() < deadline {
            controller.poll();
            std::thread::sleep(Duration::from_millis(2));
        }
        assert!(matches!(controller.status(), ScanStatus::Failed(_)));
        assert_eq!(controller.completed().unwrap().path, root);

        fs::remove_dir_all(root).unwrap();
    }
}
