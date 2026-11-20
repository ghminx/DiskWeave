#[cfg(test)]
use crate::scanner;
use crate::scanner::{ScanProgress, ScanResult};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

pub struct CompletedScan {
    pub entries: Option<Arc<crate::usn::entries::Projection>>,
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
        record.entries = self.entries.clone();
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
    progress: Arc<Mutex<ScanProgress>>,
    receiver: Receiver<(SystemTime, std::io::Result<crate::engine::Outcome>)>,
}

#[derive(Default)]
pub struct ScanController {
    preview: Option<crate::scanner::preview::Model>,
    parallelism: crate::scanner::Parallelism,
    active: Option<ActiveScan>,
    completed: Option<CompletedScan>,
    status: ScanStatus,
}

impl ScanController {
    pub(crate) fn from_saved(record: &crate::snapshot::SavedScan) -> Self {
        Self {
            completed: Some(CompletedScan {
                entries: record.entries.clone(),
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

    #[cfg(test)]
    pub fn parallelism(&self) -> crate::scanner::Parallelism {
        self.parallelism
    }
    pub fn configure_parallelism(
        &mut self,
        value: crate::scanner::Parallelism,
    ) -> Result<(), &'static str> {
        if self.is_running() {
            return Err("스캔 중에는 동시 작업 수를 변경할 수 없습니다");
        }
        self.parallelism = value;
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
        let progress = Arc::new(Mutex::new(ScanProgress::default()));
        let worker_progress = Arc::clone(&progress);
        let preview_mail = crate::scanner::preview::Mailbox::default();
        let worker_preview = preview_mail.clone();
        let parallelism = self.parallelism;
        let clock = SystemTime::now();
        std::thread::spawn(move || {
            let result = crate::engine::scan_selected(
                &worker_path,
                parallelism,
                || worker_cancelled.load(Ordering::Relaxed),
                |update| *worker_progress.lock().unwrap() = update,
                Some(&mut |batch| worker_preview.publish(batch)),
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
        // 완료 신호보다 먼저 게시한 마지막 폴더 묶음까지 반영.
        let message = active.receiver.try_recv();
        if let Some(batch) = active.preview_mail.drain() {
            if let Err(error) = self.preview.as_mut().expect("활성 미리보기").apply(batch) {
                active.preview_error = Some(error.to_string());
                active.cancelled.store(true, Ordering::Relaxed);
            }
        }
        let (finished, result) = match message {
            Ok(result) => result,
            Err(TryRecvError::Empty) => return false,
            Err(TryRecvError::Disconnected) => {
                self.status = ScanStatus::Failed("스캔 작업이 예기치 않게 종료됐습니다".to_owned());
                self.active = None;
                self.preview = None;
                return false;
            }
        };
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
                    entries: None,
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
            .map(|active| *active.progress.lock().unwrap())
    }

    pub fn elapsed(&self) -> Option<Duration> {
        self.active.as_ref().map(|active| active.started.elapsed())
    }

    pub fn accept_cursor(&mut self, cursor: crate::tracking::Confirmation) -> std::io::Result<()> {
        let complete = self
            .completed
            .as_mut()
            .ok_or_else(|| std::io::Error::other("확인할 완료 결과가 없습니다"))?;
        cursor.apply(complete)
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
    fn general_scan_preserves_the_previous_result_after_cancellation() {
        let root =
            std::env::temp_dir().join(format!("diskweave-method-job-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        let mut job = ScanController::default();
        job.start(root.clone()).unwrap();
        while job.is_running() {
            job.poll();
            std::thread::yield_now();
        }
        let completed = job.completed().unwrap();
        assert_eq!(
            completed.engine.as_ref().unwrap().actual,
            crate::engine::Method::Basic
        );
        assert!(completed.saved_result().engine.unwrap().fallback.is_none());
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
    fn parallelism_stays_fixed_and_cancellation_preserves_the_unified_result() {
        let root =
            std::env::temp_dir().join(format!("diskweave-scan-settings-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("data"), [0; 3]).unwrap();
        let mut controller = ScanController::default();
        controller
            .configure_parallelism(crate::scanner::Parallelism::Two)
            .unwrap();
        controller.start(root.clone()).unwrap();
        assert!(controller
            .configure_parallelism(crate::scanner::Parallelism::Eight)
            .is_err());
        assert_eq!(controller.parallelism(), crate::scanner::Parallelism::Two);
        let deadline = Instant::now() + Duration::from_secs(5);
        while controller.is_running() && Instant::now() < deadline {
            controller.poll();
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(controller.completed().unwrap().result.detailed);
        assert_eq!(
            controller.completed().unwrap().result.folders[0]
                .allocation
                .known,
            1
        );
        controller.start(root.clone()).unwrap();
        controller.cancel();
        while controller.is_running() && Instant::now() < deadline {
            controller.poll();
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(matches!(controller.status(), ScanStatus::Cancelled));
        assert!(controller.completed().unwrap().result.detailed);
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
    fn delayed_poll_keeps_progress_and_the_final_folder_batch() {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "diskweave-job-progress-{}-{stamp}",
            std::process::id()
        ));
        let child = root.join("child");
        fs::create_dir_all(&child).unwrap();
        fs::write(root.join("first.bin"), [1]).unwrap();
        fs::write(child.join("second.bin"), [2, 3]).unwrap();

        let mut controller = ScanController::default();
        controller.start(root.clone()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while controller.progress().unwrap().files_scanned < 2 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(
            controller.progress().unwrap(),
            scanner::ScanProgress {
                folders_scanned: 2,
                files_scanned: 2,
            }
        );

        while controller.is_running() && Instant::now() < deadline {
            controller.poll();
            std::thread::yield_now();
        }
        assert!(matches!(controller.status(), ScanStatus::Completed));
        let preview = controller.take_finished_preview().unwrap();
        assert_eq!(preview.folders.len(), 2);
        assert_eq!(preview.folders[0].bytes, 3);
        assert_eq!(preview.folders[1].path, child);
        assert_eq!(preview.folders[1].bytes, 2);
        assert!(preview
            .folders
            .iter()
            .all(|folder| folder.state() == crate::scanner::preview::State::Complete));
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
