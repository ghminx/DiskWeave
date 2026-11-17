use crate::snapshot::SavedScan;
use crate::usn::{Advance, WatchUpdate};
use std::collections::HashSet;
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
/// 동일한 완료 결과에 대해 검증된 증분 확인. 외부 입력의 직접 생성 방지.
pub struct Confirmation {
    scan: Arc<crate::scanner::ScanResult>,
    advance: Advance,
}
impl Confirmation {
    pub(crate) fn apply(self, complete: &mut crate::scan_job::CompletedScan) -> io::Result<()> {
        if !Arc::ptr_eq(&self.scan, &complete.result)
            || complete
                .tracking
                .as_ref()
                .is_none_or(|c| self.advance.next < c.next)
        {
            return Err(io::Error::other(
                "확인 응답과 현재 분석 결과가 일치하지 않습니다",
            ));
        }
        let checkpoint = complete.tracking.as_mut().unwrap();
        checkpoint.next = self.advance.next;
        checkpoint.pending = self.advance.pending;
        checkpoint.confirmed_after = self.advance.confirmed_after;
        complete.finished = self.advance.finished;
        Ok(())
    }
}
pub enum Update {
    Record(Box<SavedScan>),
    Cursor(Confirmation),
}
struct Current {
    scan: Arc<crate::scanner::ScanResult>,
    ids: HashSet<u64>,
    next: i64,
}
#[derive(Default)]
struct Output {
    update: Option<Update>,
    finished: Option<Result<(), String>>,
}
struct Active {
    cancel: Arc<AtomicBool>,
    output: Arc<Mutex<Output>>,
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
    publication_ready: Arc<AtomicBool>,
}
impl Controller {
    pub fn start(
        &mut self,
        id: u64,
        root: PathBuf,
        detailed: bool,
        resume: Option<Arc<SavedScan>>,
    ) -> io::Result<()> {
        if self.running() {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "다른 위치를 추적하려면 현재 추적을 중지하세요",
            ));
        }
        crate::usn_native::target(&root)?;
        self.publication_ready = Arc::new(AtomicBool::new(true));
        let ready = Arc::clone(&self.publication_ready);
        let progress = Arc::new(Mutex::new(crate::engine::Progress::default()));
        let worker_progress = Arc::clone(&progress);
        self.start_with(
            id,
            root,
            detailed,
            resume,
            move |root, detailed, resume, cancel, publish| {
                crate::ntfs_helper::watch(
                    root,
                    detailed,
                    resume,
                    crate::ntfs_helper::WatchControl {
                        cancelled: cancel,
                        ready: &|| ready.load(Ordering::Relaxed),
                    },
                    publish,
                    |event| {
                        *worker_progress.lock().unwrap() = event;
                    },
                )
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
                &mut dyn FnMut(WatchUpdate) -> io::Result<()>,
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
        self.publication_ready.store(true, Ordering::Relaxed);
        let publication_ready = Arc::clone(&self.publication_ready);
        let path = root.clone();
        std::thread::spawn(move || {
            let stopped = || worker_cancel.load(Ordering::Relaxed);
            let mut current: Option<Current> = None;
            let result = run(&path, detailed, resume, &stopped, &mut |update| {
                crate::output::check(&stopped)?;
                let checked = match update {
                    WatchUpdate::Record(record) => {
                        if record.scan.folders.first().is_none_or(|f| f.path != path)
                            || record.scan.detailed != detailed
                        {
                            return Err(io::Error::other("변경 추적 응답 대상·수집 옵션 불일치"));
                        }
                        let checkpoint = record
                            .tracking
                            .as_ref()
                            .ok_or_else(|| io::Error::other("변경 추적 기준점 누락"))?;
                        checkpoint.validate(&record.scan)?;
                        current = Some(Current {
                            scan: Arc::clone(&record.scan),
                            ids: checkpoint.files.iter().copied().collect(),
                            next: checkpoint.next,
                        });
                        Update::Record(record)
                    }
                    WatchUpdate::Cursor(advance) => {
                        let current = current
                            .as_mut()
                            .ok_or_else(|| io::Error::other("기준 결과 없는 확인 응답"))?;
                        let mut pending = HashSet::new();
                        if advance.next < current.next
                            || advance.pending.len() > current.ids.len()
                            || advance
                                .pending
                                .iter()
                                .any(|id| !current.ids.contains(id) || !pending.insert(*id))
                        {
                            return Err(io::Error::other(
                                "확인 응답의 변경 위치·대기 파일 ID 오류",
                            ));
                        }
                        current.next = advance.next;
                        Update::Cursor(Confirmation {
                            scan: Arc::clone(&current.scan),
                            advance,
                        })
                    }
                };
                crate::output::check(&stopped)?;
                let mut output = worker_output.lock().unwrap();
                match checked {
                    Update::Cursor(cursor) => {
                        if let Some(Update::Record(record)) = output.update.as_mut() {
                            // 미수신 완료 결과를 유지하면서 같은 기준의 최신 확인만 병합.
                            let c = record.tracking.as_mut().unwrap();
                            c.next = cursor.advance.next;
                            c.pending = cursor.advance.pending;
                            c.confirmed_after = cursor.advance.confirmed_after;
                            record.finished = cursor.advance.finished;
                        } else {
                            output.update = Some(Update::Cursor(cursor));
                        }
                    }
                    update => output.update = Some(update),
                }
                publication_ready.store(false, Ordering::Relaxed);
                Ok(())
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
        self.active = Some(Active { cancel, output });
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
    pub fn poll(&mut self) -> Option<Update> {
        let active = self.active.as_ref()?;
        let mut output = active.output.lock().unwrap();
        let update = output.update.take();
        let finish = output.finished.take();
        if update.is_some() {
            self.publication_ready.store(true, Ordering::Relaxed);
        }
        drop(output);
        if let Some(update) = &update {
            let (finished, confirmed_after) = match update {
                Update::Record(record) => {
                    self.excluded = record.tracking.as_ref().unwrap().unavailable.len();
                    (
                        record.finished,
                        record.tracking.as_ref().unwrap().confirmed_after,
                    )
                }
                Update::Cursor(cursor) => (cursor.advance.finished, cursor.advance.confirmed_after),
            };
            self.checked = Some(finished);
            self.confirmed_after = confirmed_after;
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
        update
    }
    pub fn label(&self) -> &str {
        match &self.status {
            Status::Stopped => "실시간 갱신 중지됨",
            Status::Starting
                if self.progress.lock().unwrap().stage == crate::engine::Stage::Transferring =>
            {
                "초기 분석 목록 받는 중…"
            }
            Status::Starting => "권한 승인·기준 수집·변경 내용 확인 중…",
            Status::Following if self.excluded > 0 && self.applying() => "변경 반영 중 · 일부 제외",
            Status::Following if self.excluded > 0 => "갱신 중 · 일부 제외",
            Status::Following if self.applying() => "변경 내용 반영 중",
            Status::Following => "변경 확인 중",
            Status::Failed(_) => "실시간 갱신 중단 · 갱신 필요",
        }
    }
    pub fn applying(&self) -> bool {
        self.running()
            && matches!(
                self.progress.lock().unwrap().stage,
                crate::engine::Stage::Metadata | crate::engine::Stage::Transferring
            )
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
            files: (2..2 + r.scan.files.len() as u64).collect(),
            excluded: vec![],
            pending: vec![],
            unavailable: vec![],
            confirmed_after: None,
        });
        r
    }
    #[test]
    fn unread_results_survive_cursor_updates_and_idle_updates_preserve_snapshot_metadata() {
        use crate::usn::{Advance, WatchUpdate};
        let root =
            std::env::temp_dir().join(format!("diskweave-idle-updates-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("kept.bin"), b"123").unwrap();
        let now = SystemTime::now();
        let scan = Arc::new(crate::scanner::scan(&root, || false).unwrap());
        let mut value = SavedScan::new(Arc::clone(&scan), now, now, Duration::from_secs(7));
        value.engine = Some(crate::engine::Report::default());
        value.tracking = Some(crate::usn::Checkpoint {
            version: 3,
            volume: "test".into(),
            root: 1,
            journal: 7,
            next: 100,
            folders: vec![1],
            files: vec![2],
            excluded: vec![],
            pending: vec![],
            unavailable: vec![],
            confirmed_after: Some(now),
        });
        let mut locations = crate::locations::Locations::default();
        let id = locations.add(&root).unwrap();
        let (send, receive) = mpsc::channel();
        let (ready, wait) = mpsc::channel();
        let mut controller = Controller::default();
        controller
            .start_with(
                id,
                root.clone(),
                false,
                None,
                move |_, _, _, cancel, publish| {
                    while !cancel() {
                        if let Ok(update) = receive.recv_timeout(Duration::from_millis(5)) {
                            publish(update)?;
                            ready.send(()).unwrap();
                        }
                    }
                    Ok(())
                },
            )
            .unwrap();
        send.send(WatchUpdate::Record(Box::new(value))).unwrap();
        wait.recv_timeout(Duration::from_secs(2)).unwrap();
        send.send(WatchUpdate::Cursor(Advance {
            next: 108,
            pending: vec![2],
            confirmed_after: Some(now),
            finished: now,
        }))
        .unwrap();
        wait.recv_timeout(Duration::from_secs(2)).unwrap();
        let Some(Update::Record(value)) = controller.poll() else {
            panic!("읽지 않은 완료 결과 보존");
        };
        assert_eq!(value.tracking.as_ref().unwrap().next, 108);
        assert_eq!(value.tracking.as_ref().unwrap().pending, vec![2]);
        locations.accept_tracking(id, &value).unwrap();
        let generation = locations.get(id).unwrap().generation;
        let epoch = locations.epoch;
        let later = now + Duration::from_secs(1);
        send.send(WatchUpdate::Cursor(Advance {
            next: 116,
            pending: vec![],
            confirmed_after: Some(later),
            finished: later,
        }))
        .unwrap();
        wait.recv_timeout(Duration::from_secs(2)).unwrap();
        let Some(Update::Cursor(cursor)) = controller.poll() else {
            panic!("유휴 응답은 결과 교체와 구분");
        };
        locations.accept_cursor(id, cursor).unwrap();
        assert_eq!(locations.get(id).unwrap().generation, generation);
        assert_eq!(locations.epoch, epoch);
        let complete = locations.completed().unwrap();
        assert!(Arc::ptr_eq(&complete.result, &scan));
        assert_eq!(complete.result.folders[0].logical_bytes, 3);
        assert_eq!(complete.started, now);
        assert_eq!(complete.finished, later);
        assert_eq!(complete.elapsed, Duration::from_secs(7));
        assert!(complete.engine.is_some());
        let mut encoded = vec![];
        crate::snapshot::write_to(&complete.saved_result(), &mut encoded, || false).unwrap();
        let restored = crate::snapshot::read_from(encoded.as_slice(), || false).unwrap();
        let checkpoint = restored.tracking.unwrap();
        assert_eq!(checkpoint.next, 116);
        assert!(checkpoint.pending.is_empty());
        assert_eq!(checkpoint.confirmed_after, Some(later));
        assert_eq!(controller.confirmed_after, Some(later));
        controller.stop();
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn latest_full_result_replaces_queued_updates_and_late_cursor_cannot_change_another_result() {
        let root =
            std::env::temp_dir().join(format!("diskweave-coalesced-cursor-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("known"), b"123").unwrap();
        let first = record(&root);
        let mut controller = Controller::default();
        let mut locations = crate::locations::Locations::default();
        let id = locations.add(&root).unwrap();
        let (send, receive) = mpsc::channel();
        let (ready, ack) = mpsc::channel();
        controller
            .start_with(
                id,
                root.clone(),
                false,
                None,
                move |_, _, _, cancel, publish| {
                    while !cancel() {
                        if let Ok(update) = receive.recv_timeout(Duration::from_millis(5)) {
                            publish(update)?;
                            ready.send(()).unwrap();
                        }
                    }
                    Ok(())
                },
            )
            .unwrap();
        send.send(WatchUpdate::Record(Box::new(first))).unwrap();
        ack.recv_timeout(Duration::from_secs(2)).unwrap();
        let now = SystemTime::now();
        send.send(WatchUpdate::Cursor(Advance {
            next: 108,
            pending: vec![2],
            confirmed_after: Some(now),
            finished: now,
        }))
        .unwrap();
        ack.recv_timeout(Duration::from_secs(2)).unwrap();
        fs::write(root.join("known"), b"12345").unwrap();
        let mut second = record(&root);
        second.tracking.as_mut().unwrap().files = vec![3];
        second.tracking.as_mut().unwrap().next = 200;
        send.send(WatchUpdate::Record(Box::new(second))).unwrap();
        ack.recv_timeout(Duration::from_secs(2)).unwrap();
        send.send(WatchUpdate::Cursor(Advance {
            next: 208,
            pending: vec![3],
            confirmed_after: Some(now),
            finished: now,
        }))
        .unwrap();
        ack.recv_timeout(Duration::from_secs(2)).unwrap();
        let Some(Update::Record(latest)) = controller.poll() else {
            panic!("최신 완료 결과 누락")
        };
        assert_eq!(latest.scan.folders[0].logical_bytes, 5);
        assert_eq!(latest.tracking.as_ref().unwrap().next, 208);
        assert_eq!(latest.tracking.as_ref().unwrap().pending, vec![3]);
        locations.accept_tracking(id, &latest).unwrap();
        locations.tracking_failed(id);
        let stale_epoch = locations.epoch;
        let generation = locations.get(id).unwrap().generation;
        send.send(WatchUpdate::Cursor(Advance {
            next: 216,
            pending: vec![],
            confirmed_after: Some(now),
            finished: now,
        }))
        .unwrap();
        ack.recv_timeout(Duration::from_secs(2)).unwrap();
        let Some(Update::Cursor(cursor)) = controller.poll() else {
            panic!("확인 응답 누락")
        };
        locations.accept_cursor(id, cursor).unwrap();
        assert!(!locations.get(id).unwrap().stale);
        assert!(locations.epoch > stale_epoch);
        assert_eq!(locations.get(id).unwrap().generation, generation);
        send.send(WatchUpdate::Cursor(Advance {
            next: 224,
            pending: vec![],
            confirmed_after: Some(now),
            finished: now,
        }))
        .unwrap();
        ack.recv_timeout(Duration::from_secs(2)).unwrap();
        let Some(Update::Cursor(late)) = controller.poll() else {
            panic!("확인 응답 누락")
        };
        fs::write(root.join("known"), b"1234567").unwrap();
        let replacement = SavedScan::new(
            Arc::new(crate::scanner::scan(&root, || false).unwrap()),
            now,
            now,
            Duration::ZERO,
        );
        locations
            .accept_file_refresh(id, generation, &replacement)
            .unwrap();
        let next_generation = locations.get(id).unwrap().generation;
        assert!(locations.accept_cursor(id, late).is_err());
        assert!(Arc::ptr_eq(
            &locations.completed().unwrap().result,
            &replacement.scan
        ));
        assert_eq!(locations.get(id).unwrap().generation, next_generation);
        assert_eq!(
            locations.completed().unwrap().result.folders[0].logical_bytes,
            7
        );
        controller.stop();
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn invalid_update_is_returned_to_the_producer_before_it_continues() {
        let root =
            std::env::temp_dir().join(format!("diskweave-update-rejection-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        let (send, receive) = mpsc::channel();
        let mut controller = Controller::default();
        controller
            .start_with(1, root.clone(), false, None, move |_, _, _, _, publish| {
                let rejected = publish(WatchUpdate::Cursor(Advance {
                    next: 100,
                    pending: vec![],
                    confirmed_after: None,
                    finished: SystemTime::now(),
                }));
                send.send(rejected.is_err()).unwrap();
                rejected
            })
            .unwrap();
        assert!(receive.recv_timeout(Duration::from_secs(2)).unwrap());
        let until = Instant::now() + Duration::from_secs(2);
        while controller.running() {
            assert!(controller.poll().is_none());
            assert!(Instant::now() < until);
            std::thread::yield_now();
        }
        assert!(matches!(controller.status, Status::Failed(_)));
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn invalid_cursors_stop_tracking_without_replacing_the_last_complete_result() {
        let root =
            std::env::temp_dir().join(format!("diskweave-invalid-cursor-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("known"), b"123").unwrap();
        let now = SystemTime::now();
        for (next, pending, has_base) in [
            (108, vec![], false),
            (99, vec![], true),
            (108, vec![99], true),
            (108, vec![2, 2], true),
        ] {
            let mut controller = Controller::default();
            let mut locations = crate::locations::Locations::default();
            let id = locations.add(&root).unwrap();
            let (send, receive) = mpsc::channel();
            let (ready, ack) = mpsc::channel();
            controller
                .start_with(
                    id,
                    root.clone(),
                    false,
                    None,
                    move |_, _, _, cancel, publish| {
                        while !cancel() {
                            if let Ok(update) = receive.recv_timeout(Duration::from_millis(5)) {
                                publish(update)?;
                                ready.send(()).unwrap();
                            }
                        }
                        Ok(())
                    },
                )
                .unwrap();
            if has_base {
                send.send(WatchUpdate::Record(Box::new(record(&root))))
                    .unwrap();
                ack.recv_timeout(Duration::from_secs(2)).unwrap();
                let Some(Update::Record(r)) = controller.poll() else {
                    panic!("기준 결과 누락")
                };
                locations.accept_tracking(id, &r).unwrap();
            }
            let before = locations.completed().map(|r| Arc::clone(&r.result));
            send.send(WatchUpdate::Cursor(Advance {
                next,
                pending,
                confirmed_after: Some(now),
                finished: now,
            }))
            .unwrap();
            let until = Instant::now() + Duration::from_secs(2);
            while controller.running() {
                assert!(controller.poll().is_none());
                assert!(Instant::now() < until);
                std::thread::yield_now();
            }
            assert!(matches!(controller.status, Status::Failed(_)));
            assert!(controller.confirmed_after.is_none());
            if let Some(before) = before {
                let complete = locations.completed().unwrap();
                assert!(Arc::ptr_eq(&before, &complete.result));
                assert_eq!(complete.tracking.as_ref().unwrap().next, 100);
            } else {
                assert!(locations.completed().is_none());
            }
        }
        fs::remove_dir_all(root).unwrap();
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
                    publish(WatchUpdate::Record(Box::new(value)))?;
                    wait_bad.recv().unwrap();
                    let mut bad = record(&root);
                    bad.tracking.as_mut().unwrap().root = 0;
                    publish(WatchUpdate::Record(Box::new(bad)))?;
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
                wait.recv_timeout(Duration::from_secs(2)).unwrap();
                publish(WatchUpdate::Record(Box::new(record(&path))))?;
                publish(WatchUpdate::Cursor(Advance {
                    next: 108,
                    pending: vec![],
                    confirmed_after: None,
                    finished: SystemTime::now(),
                }))?;
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
                publish(WatchUpdate::Record(Box::new(record(&path))))?;
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
