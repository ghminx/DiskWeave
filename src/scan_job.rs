use crate::scanner::{self, ScanProgress, ScanResult};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub struct CompletedScan {
    pub path: PathBuf,
    pub result: ScanResult,
    pub elapsed: Duration,
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
    path: PathBuf,
    started: Instant,
    cancelled: Arc<AtomicBool>,
    progress: Arc<Mutex<ScanProgress>>,
    receiver: Receiver<std::io::Result<ScanResult>>,
}

#[derive(Default)]
pub struct ScanController {
    active: Option<ActiveScan>,
    completed: Option<CompletedScan>,
    status: ScanStatus,
}

impl ScanController {
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
        std::thread::spawn(move || {
            let result = scanner::scan_with_progress(
                &worker_path,
                || worker_cancelled.load(Ordering::Relaxed),
                |update| *worker_progress.lock().unwrap() = update,
            );
            let _ = sender.send(result);
        });
        self.status = ScanStatus::Running;
        self.active = Some(ActiveScan {
            path,
            started: Instant::now(),
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

    pub fn poll(&mut self) {
        let Some(active) = self.active.as_ref() else {
            return;
        };
        let result = match active.receiver.try_recv() {
            Ok(result) => result,
            Err(TryRecvError::Empty) => return,
            Err(TryRecvError::Disconnected) => {
                self.status = ScanStatus::Failed("스캔 작업이 예기치 않게 종료됐습니다".to_owned());
                self.active = None;
                return;
            }
        };
        let active = self.active.take().expect("수신 중인 스캔 작업");
        if active.cancelled.load(Ordering::Relaxed) {
            self.status = ScanStatus::Cancelled;
            return;
        }
        match result {
            Ok(result) => {
                self.status = ScanStatus::Completed;
                self.completed = Some(CompletedScan {
                    path: active.path,
                    result,
                    elapsed: active.started.elapsed(),
                });
            }
            Err(error) => self.status = ScanStatus::Failed(error.to_string()),
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

    pub fn completed(&self) -> Option<&CompletedScan> {
        self.completed.as_ref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

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
