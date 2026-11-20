use crate::{locations::Locations, scanner::ScanResult, snapshot::SavedScan};
use std::{
    collections::HashMap,
    io,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, TryRecvError},
        Arc,
    },
    time::{Instant, SystemTime},
};
pub struct Replaced {
    pub id: u64,
    pub old: Option<Arc<ScanResult>>,
    pub new: Arc<ScanResult>,
}
struct ResultItem {
    id: u64,
    generation: u64,
    result: io::Result<SavedScan>,
}
#[derive(Default)]
pub struct Controller {
    receiver: Option<Receiver<ResultItem>>,
    pending: HashMap<u64, u64>,
    waiting: Option<(u64, SystemTime)>,
    cancel: Arc<AtomicBool>,
    pub errors: Vec<String>,
}
impl Controller {
    pub fn busy(&self) -> bool {
        self.receiver.is_some() || self.waiting.is_some()
    }
    pub fn label(&self) -> &str {
        if self.busy() {
            "분석 목록 갱신 중"
        } else if !self.errors.is_empty() {
            "목록 갱신 실패 · 갱신 필요"
        } else {
            "목록 갱신 완료"
        }
    }
    pub fn begin(
        &mut self,
        locations: &Locations,
        affected: &[PathBuf],
        tracked: Option<u64>,
    ) -> io::Result<()> {
        if self.busy() {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "이전 목록 갱신이 진행 중입니다",
            ));
        }
        self.errors.clear();
        self.pending.clear();
        let mut targets = vec![];
        for entry in &locations.entries {
            let mut touched = false;
            for path in affected {
                touched |= crate::file_ops::contains_path(&entry.path, path)?
                    || crate::file_ops::contains_path(path, &entry.path)?;
            }
            if !touched {
                continue;
            }
            if tracked == Some(entry.id) {
                self.waiting = Some((entry.id, SystemTime::now()));
                continue;
            }
            self.pending.insert(entry.id, entry.generation);
            targets.push((entry.id, entry.generation, entry.path.clone()));
        }
        if targets.is_empty() {
            return Ok(());
        }
        self.cancel = Arc::new(AtomicBool::new(false));
        let cancel = Arc::clone(&self.cancel);
        let parallelism = locations.parallelism();
        let (sender, receiver) = mpsc::sync_channel(1);
        std::thread::spawn(move || {
            for (id, generation, path) in targets {
                let stop = || cancel.load(Ordering::Relaxed);
                if stop() {
                    break;
                }
                let started = SystemTime::now();
                let clock = Instant::now();
                let result = (|| {
                    let root = crate::file_ops::Target::capture(&path)?;
                    let scan =
                        crate::scanner::scan_unified(&path, parallelism, stop, |_| {}, None)?;
                    root.verify()?;
                    let mut record =
                        SavedScan::new(Arc::new(scan), started, SystemTime::now(), clock.elapsed());
                    record.engine = Some(crate::engine::Report {
                        requested: crate::engine::Method::Basic,
                        actual: crate::engine::Method::Basic,
                        fallback: None,
                        ..Default::default()
                    });
                    Ok(record)
                })();
                if sender
                    .send(ResultItem {
                        id,
                        generation,
                        result,
                    })
                    .is_err()
                {
                    break;
                }
            }
        });
        self.receiver = Some(receiver);
        Ok(())
    }
    pub fn cancel(&mut self, locations: &mut Locations) {
        self.cancel.store(true, Ordering::Relaxed);
        for id in self.pending.keys() {
            locations.tracking_failed(*id);
        }
        if let Some((id, _)) = self.waiting.take() {
            locations.tracking_failed(id);
        }
        self.pending.clear();
        self.receiver = None;
        self.errors
            .push("파일 작업 후 목록 갱신을 취소했습니다. 다시 스캔하세요".into());
    }
    pub fn poll(
        &mut self,
        locations: &mut Locations,
        tracking: Option<(u64, Option<SystemTime>, bool)>,
    ) -> Vec<Replaced> {
        let mut updates = vec![];
        if let Some((id, after)) = self.waiting {
            match tracking {
                Some((active, confirmed_after, true)) if active == id => {
                    if confirmed_after.is_some_and(|when| when >= after) {
                        self.waiting = None;
                    }
                }
                _ => {
                    locations.tracking_failed(id);
                    self.errors
                        .push("실시간 갱신이 중단되었습니다. 해당 위치를 다시 스캔하세요".into());
                    self.waiting = None;
                }
            }
        }
        while let Some(receiver) = &self.receiver {
            let response = match receiver.try_recv() {
                Ok(r) => r,
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    self.receiver = None;
                    for id in self.pending.keys() {
                        locations.tracking_failed(*id);
                    }
                    if !self.pending.is_empty() {
                        self.errors
                            .push("분석 목록 갱신 작업이 중단되었습니다. 다시 스캔하세요".into());
                    }
                    self.pending.clear();
                    break;
                }
            };
            self.pending.remove(&response.id);
            let old = locations
                .get(response.id)
                .and_then(|e| e.scan.completed())
                .map(|c| Arc::clone(&c.result));
            match response.result.and_then(|record| {
                locations
                    .accept_file_refresh(response.id, response.generation, &record)
                    .map(|accepted| (accepted, record))
            }) {
                Ok((true, record)) => updates.push(Replaced {
                    id: response.id,
                    old,
                    new: record.scan,
                }),
                Ok((false, _)) => {}
                Err(error) => {
                    locations.tracking_failed(response.id);
                    if self.errors.len() < 20 {
                        self.errors.push(error.to_string());
                    }
                }
            }
        }
        updates
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
    use std::{
        fs,
        time::{Duration, Instant},
    };
    #[test]
    fn a_cycle_started_before_the_file_operation_does_not_release_refresh_waiting() {
        let root =
            std::env::temp_dir().join(format!("diskweave-confirmation-{}", std::process::id()));
        fs::create_dir(&root).unwrap();
        let mut locations = Locations::default();
        let id = locations.add(&root).unwrap();
        locations.start_selected(&[id]).unwrap();
        let until = Instant::now() + Duration::from_secs(5);
        while locations.is_running() {
            locations.poll();
            assert!(Instant::now() < until);
            std::thread::yield_now();
        }
        let old = Arc::clone(&locations.completed().unwrap().result);
        let cycle_before_operation = SystemTime::now();
        fs::write(root.join("new.bin"), b"12345").unwrap();
        let mut refresh = Controller::default();
        refresh
            .begin(&locations, &[root.join("new.bin")], Some(id))
            .unwrap();
        let make_record = |scan: Arc<ScanResult>, since| {
            let mut record = SavedScan::new(
                scan,
                cycle_before_operation,
                SystemTime::now(),
                Duration::ZERO,
            );
            record.tracking = Some(crate::usn::Checkpoint {
                version: 3,
                volume: "test".into(),
                root: 1,
                journal: 7,
                next: 100,
                folders: vec![1],
                files: (2..2 + record.scan.files.len() as u64).collect(),
                excluded: vec![],
                pending: vec![],
                unavailable: vec![],
                confirmed_after: Some(since),
            });
            record
        };
        let (send, receive) = mpsc::channel();
        let mut tracking = crate::tracking::Controller::default();
        tracking
            .start_with(
                id,
                root.clone(),
                true,
                None,
                move |_, _, _, cancel, publish| {
                    while !cancel() {
                        if let Ok(record) = receive.recv_timeout(Duration::from_millis(5)) {
                            publish(record)?;
                        }
                    }
                    Ok(())
                },
            )
            .unwrap();
        send.send(crate::usn::WatchUpdate::Record(Box::new(make_record(
            old,
            cycle_before_operation,
        ))))
        .unwrap();
        loop {
            if let Some(crate::tracking::Update::Record(record)) = tracking.poll() {
                locations.accept_tracking(id, &record).unwrap();
                break;
            }
            assert!(Instant::now() < until);
            std::thread::yield_now();
        }
        refresh.poll(
            &mut locations,
            Some((id, tracking.confirmed_after, tracking.running())),
        );
        assert!(refresh.busy());
        assert_eq!(
            locations.completed().unwrap().result.folders[0].logical_bytes,
            0
        );
        let since = SystemTime::now();
        let updated = Arc::new(
            crate::scanner::scan_unified(&root, Default::default(), || false, |_| {}, None)
                .unwrap(),
        );
        send.send(crate::usn::WatchUpdate::Record(Box::new(make_record(
            updated,
            cycle_before_operation,
        ))))
        .unwrap();
        loop {
            if let Some(crate::tracking::Update::Record(record)) = tracking.poll() {
                locations.accept_tracking(id, &record).unwrap();
                break;
            }
            assert!(Instant::now() < until);
            std::thread::yield_now();
        }
        refresh.poll(
            &mut locations,
            Some((id, tracking.confirmed_after, tracking.running())),
        );
        assert!(refresh.busy());
        send.send(crate::usn::WatchUpdate::Cursor(crate::usn::Advance {
            next: 108,
            pending: vec![],
            confirmed_after: Some(since),
            finished: SystemTime::now(),
        }))
        .unwrap();
        loop {
            if let Some(crate::tracking::Update::Cursor(cursor)) = tracking.poll() {
                locations.accept_cursor(id, cursor).unwrap();
                break;
            }
            assert!(Instant::now() < until);
            std::thread::yield_now();
        }
        refresh.poll(
            &mut locations,
            Some((id, tracking.confirmed_after, tracking.running())),
        );
        assert!(!refresh.busy());
        assert_eq!(
            locations.completed().unwrap().result.folders[0].logical_bytes,
            5
        );
        tracking.stop();
        fs::remove_file(root.join("new.bin")).unwrap();
        fs::remove_dir(root).unwrap();
    }
    #[test]
    fn failed_post_operation_refresh_preserves_the_previous_result_and_marks_it_stale() {
        let root = std::env::temp_dir().join(format!(
            "diskweave-refresh-failure-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&root).unwrap();
        fs::write(root.join("keep"), b"123").unwrap();
        let mut locations = Locations::default();
        let id = locations.add(&root).unwrap();
        locations.start_selected(&[id]).unwrap();
        let until = Instant::now() + Duration::from_secs(5);
        while locations.is_running() {
            assert!(Instant::now() < until);
            locations.poll();
            std::thread::yield_now();
        }
        let generation = locations.get(id).unwrap().generation;
        fs::rename(&root, root.with_extension("moved")).unwrap();
        let mut refresh = Controller::default();
        refresh
            .begin(&locations, std::slice::from_ref(&root), None)
            .unwrap();
        while refresh.busy() {
            assert!(Instant::now() < until);
            refresh.poll(&mut locations, None);
            std::thread::yield_now();
        }
        assert!(!refresh.errors.is_empty());
        let entry = locations.get(id).unwrap();
        assert!(entry.stale);
        assert_eq!(entry.generation, generation);
        assert_eq!(
            entry.scan.completed().unwrap().result.folders[0].logical_bytes,
            3
        );
        fs::remove_dir_all(root.with_extension("moved")).unwrap();
    }
    #[test]
    fn completed_file_changes_refresh_only_affected_untracked_locations() {
        let root =
            std::env::temp_dir().join(format!("diskweave-file-refresh-{}", std::process::id()));
        fs::create_dir_all(root.join("a")).unwrap();
        fs::create_dir_all(root.join("b")).unwrap();
        let mut locations = crate::locations::Locations::default();
        let a = locations.add(&root.join("a")).unwrap();
        let b = locations.add(&root.join("b")).unwrap();
        locations.start_selected(&[a, b]).unwrap();
        let start = Instant::now();
        while locations.is_running() && start.elapsed() < Duration::from_secs(3) {
            locations.poll();
            std::thread::yield_now();
        }
        let generation = locations.get(b).unwrap().generation;
        fs::write(root.join("a/new"), b"12345").unwrap();
        let mut refresh = Controller::default();
        refresh
            .begin(&locations, &[root.join("a/new")], None)
            .unwrap();
        while refresh.busy() && start.elapsed() < Duration::from_secs(3) {
            refresh.poll(&mut locations, None);
            std::thread::yield_now();
        }
        assert_eq!(
            locations
                .get(a)
                .unwrap()
                .scan
                .completed()
                .unwrap()
                .result
                .folders[0]
                .logical_bytes,
            5
        );
        assert_eq!(locations.get(b).unwrap().generation, generation);
        fs::remove_dir_all(root).unwrap();
    }
}
