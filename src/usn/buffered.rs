use super::{verify_continuity, Event, Journal, Source};
use std::{
    collections::HashMap,
    io,
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};

const MAX_HINTS: usize = 1_000_000;

struct State<S> {
    buffer: Buffered<S>,
    failure: Option<(io::ErrorKind, String)>,
    stop: bool,
}
/// 메타데이터·전송 대기와 독립적으로 기록을 확보하며 Drop에서 수집 스레드 회수.
pub struct Background<S: Source + Send + 'static> {
    state: std::sync::Arc<(std::sync::Mutex<State<S>>, std::sync::Condvar)>,
    worker: Option<std::thread::JoinHandle<()>>,
}
impl<S: Source + Send + 'static> Background<S> {
    pub fn new(source: S) -> io::Result<Self> {
        use std::sync::{Arc, Condvar, Mutex};
        let state = Arc::new((
            Mutex::new(State {
                buffer: Buffered::new(source)?,
                failure: None,
                stop: false,
            }),
            Condvar::new(),
        ));
        let shared = Arc::clone(&state);
        let worker = std::thread::Builder::new()
            .name("diskweave-usn-capture".into())
            .spawn(move || {
                let (lock, wake) = &*shared;
                let Ok(mut state) = lock.lock() else { return };
                while !state.stop {
                    let Ok((next, _)) = wake.wait_timeout(state, Duration::from_secs(1)) else {
                        return;
                    };
                    state = next;
                    if state.stop {
                        break;
                    }
                    if let Err(error) = state.buffer.capture(false) {
                        state.failure = Some((error.kind(), error.to_string()));
                        break;
                    }
                }
            })?;
        Ok(Self {
            state,
            worker: Some(worker),
        })
    }
    fn with<T>(
        &mut self,
        operation: impl FnOnce(&mut Buffered<S>) -> io::Result<T>,
    ) -> io::Result<T> {
        let mut state =
            self.state.0.lock().map_err(|_| {
                io::Error::other("변경 기록 수집 작업이 예기치 않게 종료되었습니다")
            })?;
        if let Some((kind, message)) = &state.failure {
            return Err(io::Error::new(*kind, message.clone()));
        }
        let result = operation(&mut state.buffer);
        if let Err(error) = &result {
            state.failure = Some((error.kind(), error.to_string()));
        }
        result
    }
    pub fn journal(&mut self) -> io::Result<Journal> {
        self.with(Source::journal)
    }
    pub fn read(&mut self, start: i64, end: i64) -> io::Result<Vec<Event>> {
        self.with(|buffer| buffer.read(start, end))
    }
}
impl<S: Source + Send + 'static> Drop for Background<S> {
    fn drop(&mut self) {
        {
            let mut state = self
                .state
                .0
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            state.stop = true;
        }
        self.state.1.notify_all();
        // 읽기 전용 OS 호출이 반환된 뒤 스레드와 저널 핸들을 함께 정리.
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

/// 긴 기준 수집·재조회 중 저널을 확보하고, 공개 읽기 경계를 넘어 합치지 않는 수집기.
pub struct Buffered<S> {
    source: S,
    journal: Journal,
    start: i64,
    cursor: i64,
    sealed: i64,
    ready: Vec<Event>,
    recent: HashMap<(u64, u64), Event>,
    checked: SystemTime,
    read_once: bool,
}
impl<S: Source> Buffered<S> {
    pub fn new(mut source: S) -> io::Result<Self> {
        let journal = source.journal()?;
        let cursor = journal.next;
        let checked = source.now();
        Ok(Self {
            source,
            journal,
            start: cursor,
            cursor,
            sealed: cursor,
            ready: vec![],
            recent: HashMap::new(),
            checked,
            read_once: false,
        })
    }
    fn capture(&mut self, force: bool) -> io::Result<()> {
        if !force
            && self
                .source
                .now()
                .duration_since(self.checked)
                .is_ok_and(|elapsed| elapsed < Duration::from_secs(2))
        {
            return Ok(());
        }
        let journal = self.source.journal()?;
        verify_continuity(&self.journal.volume, self.journal.id, self.cursor, &journal)?;
        if self.cursor < journal.next {
            let events = self.source.read(self.cursor, journal.next)?;
            let mut previous = self.cursor;
            for event in events {
                if event.usn < previous
                    || event.usn >= journal.next
                    || event.id == 0
                    || event.parent == 0
                {
                    return Err(io::Error::other("확보한 변경 기록의 순서·범위·ID 오류"));
                }
                previous = event.usn;
                let key = (event.id, event.parent);
                if let Some(old) = self.recent.get_mut(&key) {
                    // 구조 변경 이유는 누적하되 CLOSE는 최신 기록의 상태만 보존.
                    let reason = (old.reason & !0x80000000) | event.reason;
                    *old = Event { reason, ..event };
                } else {
                    if self.ready.len() + self.recent.len() >= MAX_HINTS {
                        return Err(io::Error::other(
                            "확보한 변경 기록이 보관 상한을 초과했습니다. 다시 스캔해 주세요",
                        ));
                    }
                    self.recent.insert(key, event);
                }
            }
            self.cursor = journal.next;
        }
        self.journal = journal;
        self.checked = self.source.now();
        Ok(())
    }
}
impl<S: Source> Source for Buffered<S> {
    fn journal(&mut self) -> io::Result<Journal> {
        self.capture(true)?;
        let mut batch: Vec<_> = self.recent.drain().map(|(_, event)| event).collect();
        batch.sort_unstable_by_key(|event| event.usn);
        self.ready.extend(batch);
        self.sealed = self.cursor;
        let mut journal = self.journal.clone();
        // 이미 확보한 구간을 포함한 읽기 가능 범위. 미확보 구간의 유실은 capture에서 거부.
        journal.first = journal.first.min(self.start);
        journal.lowest = journal.lowest.min(self.start);
        journal.next = self.cursor;
        Ok(journal)
    }
    fn read(&mut self, start: i64, end: i64) -> io::Result<Vec<Event>> {
        if start > end
            || end < self.start
            || end > self.sealed
            || self.read_once && start != self.start
        {
            return Err(io::Error::other("확보한 변경 기록의 요청 경계 오류"));
        }
        let mut events = if start < self.start {
            // 수동 Snapshot의 과거 기준점은 실제 저널에 남은 경우에만 복원 허용.
            let journal = self.source.journal()?;
            verify_continuity(&self.journal.volume, self.journal.id, start, &journal)?;
            self.source.read(start, self.start)?
        } else {
            vec![]
        };
        let count = self.ready.partition_point(|event| event.usn < end);
        if events.len() + count > MAX_HINTS {
            return Err(io::Error::other(
                "복원할 변경 기록이 보관 상한을 초과했습니다",
            ));
        }
        events.extend(self.ready.drain(..count).filter(|event| event.usn >= start));
        self.start = end;
        self.read_once = true;
        Ok(events)
    }
    fn inspect(&mut self, path: &Path, detailed: bool) -> io::Result<crate::ntfs::Info> {
        self.capture(false)?;
        let result = self.source.inspect(path, detailed);
        self.capture(false)?;
        result
    }
    fn list(&mut self, path: &Path) -> io::Result<Vec<PathBuf>> {
        self.capture(false)?;
        let result = self.source.list(path);
        self.capture(false)?;
        result
    }
    fn now(&self) -> SystemTime {
        self.source.now()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    struct Feed {
        journal: Journal,
        events: Vec<Event>,
        now: SystemTime,
        failure: Option<i32>,
    }
    struct Raw(
        Arc<Mutex<Feed>>,
        Option<std::sync::mpsc::Sender<()>>,
        Option<std::sync::mpsc::Sender<()>>,
    );
    impl Drop for Raw {
        fn drop(&mut self) {
            if let Some(notify) = &self.2 {
                let _ = notify.send(());
            }
        }
    }
    impl Source for Raw {
        fn journal(&mut self) -> io::Result<Journal> {
            let feed = self.0.lock().unwrap();
            if let Some(code) = feed.failure {
                if let Some(notify) = &self.1 {
                    let _ = notify.send(());
                }
                return Err(io::Error::from_raw_os_error(code));
            }
            Ok(feed.journal.clone())
        }
        fn read(&mut self, start: i64, end: i64) -> io::Result<Vec<Event>> {
            let feed = self.0.lock().unwrap();
            verify_continuity("test", 7, start, &feed.journal)?;
            if let Some(notify) = &self.1 {
                let _ = notify.send(());
            }
            Ok(feed
                .events
                .iter()
                .filter(|event| event.usn >= start && event.usn < end)
                .cloned()
                .collect())
        }
        fn inspect(&mut self, _: &Path, _: bool) -> io::Result<crate::ntfs::Info> {
            Err(io::Error::other("시험에서 사용하지 않는 파일 조회"))
        }
        fn list(&mut self, _: &Path) -> io::Result<Vec<PathBuf>> {
            Ok(vec![])
        }
        fn now(&self) -> SystemTime {
            self.0.lock().unwrap().now
        }
    }
    fn fixture() -> (Buffered<Raw>, Arc<Mutex<Feed>>) {
        let feed = Arc::new(Mutex::new(Feed {
            journal: Journal {
                volume: "test".into(),
                id: 7,
                first: 0,
                lowest: 0,
                next: 100,
            },
            events: vec![],
            now: SystemTime::now(),
            failure: None,
        }));
        (
            Buffered::new(Raw(Arc::clone(&feed), None, None)).unwrap(),
            feed,
        )
    }
    fn append(feed: &Arc<Mutex<Feed>>, parent: u64, reason: u32) {
        let mut feed = feed.lock().unwrap();
        let usn = feed.journal.next;
        feed.events.push(Event {
            usn,
            id: 9,
            parent,
            reason,
        });
        feed.journal.next += 8;
        feed.now += Duration::from_secs(2);
    }
    #[test]
    fn records_are_captured_even_while_the_caller_does_not_query_or_read() {
        let (_, feed) = fixture();
        let (notify, captured) = std::sync::mpsc::channel();
        let mut source = Background::new(Raw(Arc::clone(&feed), Some(notify), None)).unwrap();
        append(&feed, 2, 0x80000001);
        captured
            .recv_timeout(Duration::from_secs(3))
            .expect("전송 대기 중에도 기록 확보 필요");
        feed.lock().unwrap().journal.first = 108;
        let end = source.journal().unwrap().next;
        assert_eq!(source.read(100, end).unwrap()[0].reason, 0x80000001);
    }
    #[test]
    fn stopping_joins_the_collector_and_closes_its_source() {
        let (_, feed) = fixture();
        let (notify, closed) = std::sync::mpsc::channel();
        let source = Background::new(Raw(feed, None, Some(notify))).unwrap();
        drop(source);
        closed.recv_timeout(Duration::from_millis(100)).unwrap();
    }
    #[test]
    fn a_background_failure_remains_fatal_even_if_later_queries_would_succeed() {
        let (_, feed) = fixture();
        let (notify, failed) = std::sync::mpsc::channel();
        let mut source = Background::new(Raw(Arc::clone(&feed), Some(notify), None)).unwrap();
        {
            let mut feed = feed.lock().unwrap();
            feed.failure = Some(5);
            feed.now += Duration::from_secs(2);
        }
        failed.recv_timeout(Duration::from_secs(3)).unwrap();
        let first = source.journal().unwrap_err().to_string();
        feed.lock().unwrap().failure = None;
        assert_eq!(source.journal().unwrap_err().to_string(), first);
    }
    #[test]
    fn compaction_preserves_both_move_parents_and_latest_open_state() {
        let (mut source, feed) = fixture();
        append(&feed, 2, 0x80001000);
        source.list(Path::new("x")).unwrap();
        append(&feed, 3, 0x80002000);
        source.list(Path::new("x")).unwrap();
        append(&feed, 2, 1);
        source.list(Path::new("x")).unwrap();
        feed.lock().unwrap().journal.first = 124;
        let boundary = source.journal().unwrap();
        let events = source.read(100, boundary.next).unwrap();
        assert_eq!(
            events
                .iter()
                .map(|e| (e.usn, e.parent, e.reason))
                .collect::<Vec<_>>(),
            vec![(108, 3, 0x80002000), (116, 2, 0x1001)]
        );
    }
    #[test]
    fn events_arriving_after_a_reported_boundary_are_not_merged_into_that_read() {
        let (mut source, feed) = fixture();
        append(&feed, 2, 0x80000001);
        let first = source.journal().unwrap();
        append(&feed, 2, 2);
        source.list(Path::new("x")).unwrap();
        let old = source.read(100, first.next).unwrap();
        assert_eq!((old.len(), old[0].reason), (1, 0x80000001));
        let second = source.journal().unwrap();
        let new = source.read(first.next, second.next).unwrap();
        assert_eq!((new.len(), new[0].reason), (1, 2));
    }
    #[test]
    fn actual_uncaptured_loss_or_generation_change_is_not_hidden_by_the_buffer() {
        for generation in [false, true] {
            let (mut source, feed) = fixture();
            append(&feed, 2, 1);
            {
                let mut feed = feed.lock().unwrap();
                if generation {
                    feed.journal.id = 8;
                } else {
                    feed.journal.first = 108;
                }
            }
            let error = source.journal().unwrap_err().to_string();
            assert!(
                error.contains(if generation {
                    "generation"
                } else {
                    "retention"
                }),
                "{error}"
            );
        }
    }
    #[test]
    fn a_new_baseline_can_start_after_the_native_handle_was_opened() {
        let (mut source, feed) = fixture();
        append(&feed, 2, 1);
        let baseline = source.journal().unwrap().next;
        append(&feed, 3, 0x80000002);
        let end = source.journal().unwrap().next;
        let events = source.read(baseline, end).unwrap();
        assert_eq!(
            events.iter().map(|e| (e.usn, e.parent)).collect::<Vec<_>>(),
            vec![(108, 3)]
        );
    }
    #[test]
    fn snapshot_resume_reads_older_records_only_when_the_os_still_has_them() {
        let (mut source, feed) = fixture();
        {
            let mut feed = feed.lock().unwrap();
            feed.events.push(Event {
                usn: 92,
                id: 8,
                parent: 2,
                reason: 1,
            });
        }
        let boundary = source.journal().unwrap();
        assert_eq!(source.read(92, boundary.next).unwrap()[0].id, 8);
        assert!(source.read(92, boundary.next).is_err());
        let (mut source, feed) = fixture();
        feed.lock().unwrap().journal.first = 100;
        let boundary = source.journal().unwrap();
        assert!(source
            .read(92, boundary.next)
            .unwrap_err()
            .to_string()
            .contains("retention"));
    }
    #[test]
    fn hint_budget_and_malformed_order_fail_instead_of_dropping_changes() {
        let (mut source, feed) = fixture();
        {
            let mut feed = feed.lock().unwrap();
            for i in 0..=MAX_HINTS {
                feed.events.push(Event {
                    usn: 100 + i as i64 * 8,
                    id: i as u64 + 1,
                    parent: 2,
                    reason: 1,
                });
            }
            feed.journal.next = 100 + (MAX_HINTS as i64 + 1) * 8;
        }
        assert!(source
            .journal()
            .unwrap_err()
            .to_string()
            .contains("보관 상한"));
        let (mut source, feed) = fixture();
        append(&feed, 2, 1);
        append(&feed, 3, 2);
        feed.lock().unwrap().events.reverse();
        assert!(source.journal().unwrap_err().to_string().contains("순서"));
    }
}
