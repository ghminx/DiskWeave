use crate::file_ops::{self, Plan, Progress, Report, Request};
use std::{
    io,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, TryRecvError},
        Arc, Mutex,
    },
    time::Instant,
};
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Phase {
    #[default]
    Idle,
    Preparing,
    Confirming,
    Running,
    Cancelling,
    Finished,
    Cancelled,
    Failed,
}
impl Phase {
    pub fn label(self) -> &'static str {
        match self {
            Self::Idle => "파일 작업 대기",
            Self::Preparing => "대상 확인 중",
            Self::Confirming => "실행 전 확인",
            Self::Running => "파일 작업 중",
            Self::Cancelling => "취소 중",
            Self::Finished => "파일 작업 종료",
            Self::Cancelled => "취소됨",
            Self::Failed => "파일 작업 실패",
        }
    }
}
enum Response {
    Prepared(io::Result<Plan>),
    Finished(Report),
}
#[derive(Default)]
pub struct Controller {
    pub phase: Phase,
    pub plan: Option<Plan>,
    pub report: Option<Report>,
    pub error: Option<String>,
    progress: Arc<Mutex<Progress>>,
    cancel: Arc<AtomicBool>,
    receiver: Option<Receiver<Response>>,
    started: Option<Instant>,
}
impl Controller {
    pub fn working(&self) -> bool {
        self.receiver.is_some()
    }
    pub fn busy(&self) -> bool {
        self.working() || self.phase == Phase::Confirming
    }
    pub fn progress(&self) -> Progress {
        self.progress.lock().unwrap().clone()
    }
    pub fn elapsed(&self) -> f32 {
        self.started.map_or(0.0, |t| t.elapsed().as_secs_f32())
    }
    pub fn prepare(&mut self, request: Request) -> io::Result<()> {
        self.prepare_expected(request, None)
    }
    pub fn prepare_expected(
        &mut self,
        request: Request,
        expected: Option<(crate::metadata::Identity, bool)>,
    ) -> io::Result<()> {
        if self.busy() {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "이전 파일 작업을 마치거나 취소하세요",
            ));
        }
        let cancel = Arc::new(AtomicBool::new(false));
        let worker_cancel = Arc::clone(&cancel);
        let progress = Arc::new(Mutex::new(Progress::default()));
        let worker_progress = Arc::clone(&progress);
        let (sender, receiver) = mpsc::channel();
        std::thread::spawn(move || {
            let cancel = || worker_cancel.load(Ordering::Relaxed);
            let mut progress = |p| *worker_progress.lock().unwrap() = p;
            let result = if expected.is_some() {
                file_ops::prepare_expected(request, expected, &cancel, &mut progress)
            } else {
                file_ops::prepare(request, &cancel, &mut progress)
            };
            let _ = sender.send(Response::Prepared(result));
        });
        self.cancel = cancel;
        self.progress = progress;
        self.receiver = Some(receiver);
        self.phase = Phase::Preparing;
        self.plan = None;
        self.report = None;
        self.error = None;
        self.started = Some(Instant::now());
        Ok(())
    }
    pub fn confirm(&mut self) -> io::Result<()> {
        if self.phase != Phase::Confirming {
            return Err(io::Error::other("실행할 작업을 먼저 확인하세요"));
        }
        let plan = self
            .plan
            .take()
            .ok_or_else(|| io::Error::other("준비한 작업이 없습니다"))?;
        let cancel = Arc::clone(&self.cancel);
        let progress = Arc::clone(&self.progress);
        let (sender, receiver) = mpsc::channel();
        std::thread::spawn(move || {
            let report = file_ops::execute(plan, &|| cancel.load(Ordering::Relaxed), &mut |p| {
                *progress.lock().unwrap() = p
            });
            let _ = sender.send(Response::Finished(report));
        });
        self.receiver = Some(receiver);
        self.phase = Phase::Running;
        self.started = Some(Instant::now());
        Ok(())
    }
    pub fn cancel(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
        if self.working() {
            self.phase = Phase::Cancelling;
        } else if self.phase == Phase::Confirming {
            self.plan = None;
            self.phase = Phase::Cancelled;
        }
    }
    pub fn poll(&mut self) -> bool {
        let Some(receiver) = &self.receiver else {
            return false;
        };
        let response = match receiver.try_recv() {
            Ok(r) => r,
            Err(TryRecvError::Empty) => return false,
            Err(TryRecvError::Disconnected) => {
                self.receiver = None;
                self.phase = Phase::Failed;
                self.error = Some("파일 작업이 예기치 않게 종료됐습니다".into());
                return false;
            }
        };
        self.receiver = None;
        match response {
            Response::Prepared(result) => {
                if self.cancel.load(Ordering::Relaxed) {
                    self.phase = Phase::Cancelled;
                } else {
                    match result {
                        Ok(plan) => {
                            self.plan = Some(plan);
                            self.phase = Phase::Confirming;
                        }
                        Err(e) => {
                            self.error = Some(e.to_string());
                            self.phase = Phase::Failed;
                        }
                    }
                }
            }
            Response::Finished(report) => {
                self.phase = if report.cancelled {
                    Phase::Cancelled
                } else {
                    Phase::Finished
                };
                self.report = Some(report);
                return true;
            }
        }
        false
    }
}
impl Drop for Controller {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::file_ops::{Action, Request};
    use std::{
        fs,
        time::{Duration, Instant},
    };
    #[test]
    fn background_file_jobs_require_confirmation_and_reject_duplicate_starts() {
        let root = std::env::temp_dir().join(format!("diskweave-file-job-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        let request = Request {
            action: Action::NewFolder {
                parent: root.clone(),
                name: "created".into(),
            },
            paths: vec![],
            protected: vec![root.clone()],
        };
        let mut job = Controller::default();
        job.prepare(request.clone()).unwrap();
        assert!(job.prepare(request.clone()).is_err());
        let start = Instant::now();
        while job.working() && start.elapsed() < Duration::from_secs(3) {
            job.poll();
            std::thread::yield_now();
        }
        assert_eq!(job.phase, Phase::Confirming);
        assert!(!root.join("created").exists());
        job.cancel();
        assert_eq!(job.phase, Phase::Cancelled);
        assert!(!root.join("created").exists());
        job.prepare(request).unwrap();
        while job.working() && start.elapsed() < Duration::from_secs(3) {
            job.poll();
            std::thread::yield_now();
        }
        job.confirm().unwrap();
        while job.working() && start.elapsed() < Duration::from_secs(3) {
            job.poll();
            std::thread::yield_now();
        }
        assert_eq!(job.phase, Phase::Finished);
        assert!(job.report.as_ref().unwrap().items[0].succeeded());
        assert!(root.join("created").is_dir());
        fs::remove_dir_all(root).unwrap();
    }
}
