use crate::{
    duplicates::{self, Progress, Report, Scope},
    locations::Snapshot,
};
use std::{
    io,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc, Mutex,
    },
    thread::JoinHandle,
    time::Instant,
};

struct Active {
    cancel: Arc<AtomicBool>,
    receiver: mpsc::Receiver<io::Result<Report>>,
    worker: JoinHandle<()>,
    versions: Vec<(u64, u64)>,
}
#[derive(Default)]
pub struct Controller {
    active: Option<Active>,
    progress: Arc<Mutex<Progress>>,
    pub report: Option<Report>,
    pub status: String,
    pub scope_label: String,
    pub report_label: String,
    pub previous: bool,
    pub stale: bool,
    pub revision: u64,
    started: Option<Instant>,
    elapsed: f32,
}
impl Controller {
    pub fn running(&self) -> bool {
        self.active.is_some()
    }
    pub fn progress(&self) -> Progress {
        *self.progress.lock().unwrap()
    }
    pub fn elapsed(&self) -> f32 {
        self.started
            .map_or(self.elapsed, |t| t.elapsed().as_secs_f32())
    }
    pub fn start(
        &mut self,
        sources: Vec<Snapshot>,
        scope: Scope,
        label: String,
        blocked: bool,
    ) -> io::Result<()> {
        if blocked || self.running() {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "스캔·파일 작업·실시간 갱신을 마치거나 중지한 뒤 검사하세요",
            ));
        }
        if sources.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "먼저 폴더를 스캔하세요",
            ));
        }
        let versions = sources.iter().map(|s| (s.id, s.generation)).collect();
        let cancel = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&cancel);
        self.progress = Arc::default();
        let progress = Arc::clone(&self.progress);
        let (sender, receiver) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            let result =
                duplicates::inspect(sources, scope, &|| flag.load(Ordering::Relaxed), &mut |p| {
                    *progress.lock().unwrap() = p
                });
            let _ = sender.send(result);
        });
        self.active = Some(Active {
            cancel,
            receiver,
            worker,
            versions,
        });
        self.scope_label = label;
        self.status = "검사 중".into();
        self.previous = self.report.is_some();
        self.started = Some(Instant::now());
        Ok(())
    }
    pub fn cancel(&mut self) {
        if let Some(a) = &self.active {
            a.cancel.store(true, Ordering::Relaxed);
            self.status = "취소 중".into();
        }
    }
    pub fn poll(&mut self, current: &[Snapshot]) {
        let matches = |versions: &[(u64, u64)]| {
            versions
                .iter()
                .all(|(id, g)| current.iter().any(|s| s.id == *id && s.generation == *g))
        };
        if let Some(r) = &self.report {
            self.stale = !r.sources.iter().all(|s| {
                current
                    .iter()
                    .any(|c| c.id == s.id && c.generation == s.generation)
            });
        }
        let Some(a) = &self.active else { return };
        let valid = matches(&a.versions);
        if !valid {
            a.cancel.store(true, Ordering::Relaxed);
        }
        let result = match a.receiver.try_recv() {
            Ok(v) => v,
            Err(mpsc::TryRecvError::Empty) => return,
            Err(mpsc::TryRecvError::Disconnected) => Err(io::Error::other(
                "중복 검사 작업이 예기치 않게 종료됐습니다",
            )),
        };
        let a = self.active.take().unwrap();
        let cancelled = a.cancel.load(Ordering::Relaxed);
        let joined = a.worker.join();
        self.elapsed = self
            .started
            .take()
            .map_or(0.0, |t| t.elapsed().as_secs_f32());
        if joined.is_err() {
            self.status = "중복 검사 작업 실패".into();
            return;
        }
        if !valid {
            self.status = "분석 결과가 바뀌었습니다 · 재검사 필요".into();
            return;
        }
        if cancelled {
            self.status = "취소됨".into();
            return;
        }
        match result {
            Ok(r) => {
                self.report = Some(r);
                self.report_label = self.scope_label.clone();
                self.previous = false;
                self.stale = false;
                self.status = "검사 완료".into();
                self.revision += 1;
            }
            Err(e) => {
                self.status = if e.kind() == io::ErrorKind::Interrupted {
                    "취소됨".into()
                } else {
                    format!("검사 실패: {e}")
                }
            }
        }
    }
}
impl Drop for Controller {
    fn drop(&mut self) {
        if let Some(a) = self.active.take() {
            a.cancel.store(true, Ordering::Relaxed);
            let _ = a.worker.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_empty_or_blocked_work() {
        let mut job = Controller::default();
        assert!(job
            .start(vec![], crate::duplicates::Scope::All, "전체".into(), false)
            .is_err());
        assert!(job
            .start(vec![], crate::duplicates::Scope::All, "범위".into(), true)
            .is_err());
        assert!(!job.running());
    }
    #[test]
    fn cancellation_preserves_previous_result_and_scope_and_new_generation_is_rejected() {
        let fixture = crate::duplicates::tests::Fixture::new();
        std::fs::write(fixture.0.join("a"), b"same").unwrap();
        std::fs::write(fixture.0.join("b"), b"same").unwrap();
        let sources = vec![fixture.snapshot()];
        let mut job = Controller::default();
        job.start(sources.clone(), Scope::All, "첫 범위".into(), false)
            .unwrap();
        assert!(job
            .start(sources.clone(), Scope::All, "중복 시작".into(), false)
            .is_err());
        let limit = Instant::now() + std::time::Duration::from_secs(5);
        while job.running() {
            job.poll(&sources);
            assert!(Instant::now() < limit);
            std::thread::yield_now();
        }
        assert_eq!(job.report.as_ref().unwrap().groups.len(), 1);
        let elapsed = job.elapsed();
        std::thread::sleep(std::time::Duration::from_millis(5));
        assert_eq!(job.elapsed(), elapsed, "완료 후 경과 시간은 고정");
        job.start(sources.clone(), Scope::All, "새 범위".into(), false)
            .unwrap();
        job.cancel();
        while job.running() {
            job.poll(&sources);
            assert!(Instant::now() < limit);
            std::thread::yield_now();
        }
        assert_eq!(job.report_label, "첫 범위");
        assert_eq!(job.report.as_ref().unwrap().groups.len(), 1);
        assert!(job.previous);
        job.start(
            sources.clone(),
            Scope::Folder {
                location: 99,
                folder: 0,
            },
            "잘못된 범위".into(),
            false,
        )
        .unwrap();
        while job.running() {
            job.poll(&sources);
            assert!(Instant::now() < limit);
            std::thread::yield_now();
        }
        assert!(job.status.starts_with("검사 실패:"));
        assert_eq!(job.report_label, "첫 범위");
        assert_eq!(job.report.as_ref().unwrap().groups.len(), 1);
        job.start(sources.clone(), Scope::All, "다음 검사".into(), false)
            .unwrap();
        let mut changed = sources.clone();
        changed[0].generation += 1;
        while job.running() {
            job.poll(&changed);
            assert!(Instant::now() < limit);
            std::thread::yield_now();
        }
        assert!(job.stale);
        assert_eq!(job.revision, 1);
        assert_eq!(job.report_label, "첫 범위");
    }
}
