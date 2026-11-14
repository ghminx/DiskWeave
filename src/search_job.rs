use crate::scanner::ScanResult;
use crate::search::{search, SearchInput, SearchResults};
use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc};

#[derive(Default)]
pub enum SearchStatus {
    #[default]
    Idle,
    Running,
    Completed,
    Cancelled,
    Failed(String),
}
struct Request {
    id: u64,
    scan: Arc<ScanResult>,
    input: SearchInput,
    scope: usize,
}

#[derive(Default)]
pub struct SearchController {
    latest: Arc<AtomicU64>,
    active: Option<mpsc::Receiver<(u64, io::Result<SearchResults>)>>,
    pending: Option<Request>,
    results: Option<SearchResults>,
    status: SearchStatus,
}

impl SearchController {
    pub fn submit(&mut self, scan: Arc<ScanResult>, input: SearchInput, scope: usize) {
        let id = self.latest.fetch_add(1, Ordering::Relaxed) + 1;
        self.pending = Some(Request {
            id,
            scan,
            input,
            scope,
        });
        self.results = None;
        self.status = SearchStatus::Running;
        self.launch_pending();
    }

    fn launch_pending(&mut self) {
        if self.active.is_some() {
            return;
        }
        let Some(request) = self.pending.take() else {
            return;
        };
        let latest = Arc::clone(&self.latest);
        let (sender, receiver) = mpsc::channel();
        std::thread::spawn(move || {
            let result = request.input.compile(request.scope).and_then(|query| {
                search(&request.scan, &query, || {
                    latest.load(Ordering::Relaxed) != request.id
                })
            });
            let _ = sender.send((request.id, result));
        });
        self.active = Some(receiver);
    }

    pub fn cancel(&mut self) {
        self.latest.fetch_add(1, Ordering::Relaxed);
        self.pending = None;
        self.results = None;
        self.status = SearchStatus::Cancelled;
    }

    pub fn poll(&mut self) -> bool {
        let Some(receiver) = &self.active else {
            return false;
        };
        let response = match receiver.try_recv() {
            Ok(response) => Some(response),
            Err(mpsc::TryRecvError::Empty) => return false,
            Err(mpsc::TryRecvError::Disconnected) => None,
        };
        self.active = None;
        let mut changed = false;
        match response {
            Some((id, result)) if id == self.latest.load(Ordering::Relaxed) => {
                match result {
                    Ok(results) => {
                        self.results = Some(results);
                        self.status = SearchStatus::Completed;
                    }
                    Err(error) => {
                        self.status = SearchStatus::Failed(error.to_string());
                    }
                }
                changed = true;
            }
            None if self.pending.is_none() && matches!(self.status, SearchStatus::Running) => {
                self.status = SearchStatus::Failed("검색 작업이 예기치 않게 종료됐습니다".into());
                changed = true;
            }
            _ => {}
        }
        self.launch_pending();
        changed
    }

    pub fn status(&self) -> &SearchStatus {
        &self.status
    }
    pub fn results(&self) -> Option<&SearchResults> {
        self.results.as_ref()
    }
}

impl Drop for SearchController {
    fn drop(&mut self) {
        self.latest.fetch_add(1, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn wait(controller: &mut SearchController) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while matches!(controller.status(), SearchStatus::Running) && Instant::now() < deadline {
            controller.poll();
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(!matches!(controller.status(), SearchStatus::Running));
    }

    #[test]
    fn replacing_scan_does_not_publish_previous_scan_item_ids() {
        let root =
            std::env::temp_dir().join(format!("diskweave-search-replace-{}", std::process::id()));
        std::fs::create_dir_all(root.join("second")).unwrap();
        std::fs::write(root.join("model.bin"), [0; 3]).unwrap();
        let first = Arc::new(crate::scanner::scan(&root, || false).unwrap());
        std::fs::write(root.join("second").join("aaa.bin"), [0]).unwrap();
        std::fs::write(root.join("second").join("replacement.bin"), [0; 9]).unwrap();
        let second = Arc::new(crate::scanner::scan(&root.join("second"), || false).unwrap());
        let mut controller = SearchController::default();
        controller.submit(
            first,
            SearchInput {
                text: "model".into(),
                ..Default::default()
            },
            0,
        );
        controller.submit(
            Arc::clone(&second),
            SearchInput {
                text: "replacement".into(),
                ..Default::default()
            },
            0,
        );
        wait(&mut controller);
        let hits = controller.results().unwrap();
        assert_eq!(hits.items.len(), 1);
        assert_eq!(
            crate::search::item_name(&second, hits.items[0]),
            "replacement.bin"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn only_latest_query_can_publish_and_cancel_discards_results() {
        let root =
            std::env::temp_dir().join(format!("diskweave-search-job-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("model.npy"), [1, 2, 3]).unwrap();
        let scan = Arc::new(crate::scanner::scan(&root, || false).unwrap());
        let mut controller = SearchController::default();
        controller.submit(
            Arc::clone(&scan),
            SearchInput {
                text: "model".into(),
                ..Default::default()
            },
            0,
        );
        controller.submit(
            Arc::clone(&scan),
            SearchInput {
                text: "absent".into(),
                ..Default::default()
            },
            0,
        );
        wait(&mut controller);
        assert!(controller.results().unwrap().items.is_empty());
        controller.submit(Arc::clone(&scan), SearchInput::default(), 0);
        controller.cancel();
        controller.poll();
        assert!(matches!(controller.status(), SearchStatus::Cancelled));
        assert!(controller.results().is_none());
        controller.submit(
            scan,
            SearchInput {
                min_size: "bad".into(),
                ..Default::default()
            },
            0,
        );
        wait(&mut controller);
        assert!(matches!(controller.status(), SearchStatus::Failed(_)));
        assert!(controller.results().is_none());
        std::fs::remove_dir_all(root).unwrap();
    }
}
