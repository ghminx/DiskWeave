use crate::comparison::Comparison;
use crate::snapshot::SavedScan;
use std::io;
use std::path::PathBuf;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc::{self, Receiver, TryRecvError},
    Arc, Mutex,
};

pub enum Task {
    Save {
        record: Arc<SavedScan>,
        path: PathBuf,
        overwrite: bool,
    },
    Export {
        sources: Vec<crate::locations::Snapshot>,
        scope: crate::export::ExportScope,
        path: PathBuf,
        overwrite: bool,
    },
    Load(PathBuf),
    Compare {
        before: PathBuf,
        after: PathBuf,
    },
}
#[derive(Default)]
pub enum View {
    #[default]
    Live,
    Saved(Arc<SavedScan>),
    Comparison(Arc<Comparison>),
}
#[derive(Default)]
pub enum Status {
    #[default]
    Idle,
    Running,
    Cancelling,
    Completed(String),
    Cancelled,
    Failed(String),
}
enum Outcome {
    Written(PathBuf),
    Loaded(Arc<SavedScan>),
    Compared(Arc<Comparison>),
}
struct Active {
    cancelled: Arc<AtomicBool>,
    stage: Arc<Mutex<&'static str>>,
    receiver: Receiver<io::Result<Outcome>>,
}
#[derive(Default)]
pub struct Documents {
    pub status: Status,
    pub view: View,
    active: Option<Active>,
}
impl Documents {
    pub fn running(&self) -> bool {
        self.active.is_some()
    }
    pub fn stage(&self) -> String {
        self.active
            .as_ref()
            .map(|a| a.stage.lock().unwrap().to_string())
            .unwrap_or_default()
    }
    pub fn start(&mut self, task: Task) -> io::Result<()> {
        if self.running() {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "진행 중인 결과 작업이 있습니다",
            ));
        }
        let cancelled = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&cancelled);
        let stage = Arc::new(Mutex::new("준비 중…"));
        let progress = Arc::clone(&stage);
        let (sender, receiver) = mpsc::channel();
        std::thread::spawn(move || {
            let cancel = || flag.load(Ordering::Relaxed);
            let result = (|| match task {
                Task::Save {
                    record,
                    path,
                    overwrite,
                } => {
                    *progress.lock().unwrap() = "분석 결과 저장 중…";
                    crate::snapshot::save(&record, &path, overwrite, cancel)?;
                    Ok(Outcome::Written(path))
                }
                Task::Export {
                    sources,
                    scope,
                    path,
                    overwrite,
                } => {
                    *progress.lock().unwrap() = "파일 목록 내보내는 중…";
                    crate::export::write(&sources, &scope, &path, overwrite, cancel)?;
                    Ok(Outcome::Written(path))
                }
                Task::Load(path) => {
                    *progress.lock().unwrap() = "저장된 결과 읽는 중…";
                    Ok(Outcome::Loaded(Arc::new(crate::snapshot::load(
                        &path, cancel,
                    )?)))
                }
                Task::Compare { before, after } => {
                    *progress.lock().unwrap() = "기준 결과 읽는 중…";
                    let a = crate::snapshot::load(&before, cancel)?;
                    *progress.lock().unwrap() = "새 결과 읽는 중…";
                    let b = crate::snapshot::load(&after, cancel)?;
                    *progress.lock().unwrap() = "두 결과 비교 중…";
                    Ok(Outcome::Compared(Arc::new(crate::comparison::compare(
                        &a, &b, cancel,
                    )?)))
                }
            })();
            let _ = sender.send(result);
        });
        self.active = Some(Active {
            cancelled,
            stage,
            receiver,
        });
        self.status = Status::Running;
        Ok(())
    }
    pub fn cancel(&mut self) {
        if let Some(a) = &self.active {
            a.cancelled.store(true, Ordering::Relaxed);
            self.status = Status::Cancelling;
        }
    }
    pub fn poll(&mut self) -> bool {
        let Some(active) = &self.active else {
            return false;
        };
        let result = match active.receiver.try_recv() {
            Ok(r) => r,
            Err(TryRecvError::Empty) => return false,
            Err(TryRecvError::Disconnected) => {
                Err(io::Error::other("결과 작업이 예기치 않게 종료되었습니다"))
            }
        };
        let active = self.active.take().unwrap();
        // 최종 교체가 끝난 저장 작업은 이후 취소 요청과 무관하게 완료로 보고.
        if active.cancelled.load(Ordering::Relaxed) && !matches!(&result, Ok(Outcome::Written(_))) {
            self.status = Status::Cancelled;
            return false;
        }
        match result {
            Ok(Outcome::Written(path)) => {
                self.status = Status::Completed(format!("저장 완료 · {}", path.display()))
            }
            Ok(Outcome::Loaded(record)) => {
                self.view = View::Saved(record);
                self.status = Status::Completed("저장된 결과를 열었습니다".into());
                return true;
            }
            Ok(Outcome::Compared(result)) => {
                self.view = View::Comparison(result);
                self.status = Status::Completed("비교 완료".into());
                return true;
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => self.status = Status::Cancelled,
            Err(e) => self.status = Status::Failed(e.to_string()),
        }
        false
    }
    pub fn back(&mut self) -> bool {
        if self.running() {
            return false;
        }
        self.view = View::Live;
        self.status = Status::Idle;
        true
    }
}
impl Drop for Documents {
    fn drop(&mut self) {
        if let Some(active) = &self.active {
            active.cancelled.store(true, Ordering::Relaxed);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::time::{Duration, Instant, SystemTime};
    fn wait(job: &mut Documents) {
        let end = Instant::now() + Duration::from_secs(5);
        while job.running() && Instant::now() < end {
            job.poll();
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(!job.running());
    }
    #[test]
    fn offline_saved_workspace_searches_without_replacing_live_locations() {
        let root =
            std::env::temp_dir().join(format!("diskweave-offline-view-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("model.bin"), [0; 5]).unwrap();
        let mut live = crate::locations::Locations::default();
        let id = live.add(&root).unwrap();
        let now = SystemTime::now();
        let saved = crate::snapshot::SavedScan::new(
            Arc::new(crate::scanner::scan(&root, || false).unwrap()),
            now,
            now,
            Duration::ZERO,
        );
        std::fs::remove_dir_all(&root).unwrap();
        let archived = crate::locations::Locations::from_saved(&saved);
        assert!(!archived.is_running());
        assert_eq!(archived.completed().unwrap().finished, now);
        let query = crate::search::SearchInput {
            text: "model".into(),
            ..Default::default()
        };
        let results =
            crate::global_search::search_all(archived.snapshots(), &query, || false).unwrap();
        assert_eq!(results.hits.len(), 1);
        assert_eq!(live.selected_id(), Some(id));
        assert_eq!(live.entries.len(), 1);
        assert!(live.completed().is_none());
    }
    #[test]
    fn file_jobs_block_duplicates_and_preserve_view_on_failure_cancel_and_return() {
        let base = std::env::temp_dir().join(format!("diskweave-documents-{}", std::process::id()));
        std::fs::create_dir_all(base.join("data")).unwrap();
        std::fs::write(base.join("data/file.bin"), [0; 3]).unwrap();
        let now = SystemTime::now();
        let record = Arc::new(crate::snapshot::SavedScan::new(
            Arc::new(crate::scanner::scan(&base.join("data"), || false).unwrap()),
            now,
            now,
            Duration::ZERO,
        ));
        let path = base.join("saved.dwsnap");
        let mut job = Documents::default();
        job.start(Task::Save {
            record,
            path: path.clone(),
            overwrite: false,
        })
        .unwrap();
        assert!(job.running());
        assert!(job.start(Task::Load(path.clone())).is_err());
        wait(&mut job);
        assert!(matches!(job.status, Status::Completed(_)));
        job.start(Task::Load(path.clone())).unwrap();
        wait(&mut job);
        assert!(matches!(job.view, View::Saved(_)));
        job.start(Task::Load(base.join("absent"))).unwrap();
        wait(&mut job);
        assert!(matches!(job.status, Status::Failed(_)));
        assert!(matches!(job.view, View::Saved(_)));
        job.start(Task::Load(path.clone())).unwrap();
        job.cancel();
        wait(&mut job);
        assert!(matches!(job.status, Status::Cancelled));
        assert!(matches!(job.view, View::Saved(_)));
        job.back();
        assert!(matches!(job.view, View::Live));
        job.start(Task::Compare {
            before: path.clone(),
            after: path,
        })
        .unwrap();
        wait(&mut job);
        assert!(matches!(job.view, View::Comparison(_)));
        job.back();
        assert!(matches!(job.view, View::Live));
        std::fs::remove_dir_all(base).unwrap();
    }
}
