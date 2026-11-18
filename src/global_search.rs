use crate::explorer::Item;
use crate::locations::Snapshot;
use crate::search::{SearchInput, SortKey};
use std::io;
use std::sync::Arc;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Hit {
    pub source: usize,
    pub item: Item,
}
pub struct GlobalResults {
    pub sources: Vec<Snapshot>,
    pub hits: Vec<Hit>,
    pub unknown_dates: usize,
}

pub fn search_all(
    sources: Vec<Snapshot>,
    input: &SearchInput,
    cancelled: impl Fn() -> bool,
) -> io::Result<GlobalResults> {
    let query = input.compile(0)?;
    let mut keyed = Vec::new();
    let mut unknown_dates = 0;
    for (source, snapshot) in sources.iter().enumerate() {
        let result = crate::search::matching(&snapshot.scan, &query, &cancelled)?;
        unknown_dates += result.unknown_dates;
        for item in result.items {
            crate::output::check(&cancelled)?;
            keyed.push((
                Hit { source, item },
                SortKey::new(&snapshot.scan, item, input.sort),
            ));
        }
    }
    keyed.sort_unstable_by(|a, b| a.1.compare(&b.1, input.descending, false));
    if cancelled() {
        return Err(io::Error::new(io::ErrorKind::Interrupted, "검색 취소"));
    }
    Ok(GlobalResults {
        sources,
        hits: keyed.into_iter().map(|(hit, _)| hit).collect(),
        unknown_dates,
    })
}

#[derive(Default)]
pub struct GlobalController {
    latest: Arc<std::sync::atomic::AtomicU64>,
    active: Option<std::sync::mpsc::Receiver<(u64, io::Result<GlobalResults>)>>,
    pending: Option<(u64, Vec<Snapshot>, SearchInput)>,
    pub results: Option<GlobalResults>,
    pub error: Option<String>,
    pub running: bool,
}

impl GlobalController {
    pub fn submit(&mut self, sources: Vec<Snapshot>, input: SearchInput) {
        let id = self
            .latest
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            + 1;
        self.pending = Some((id, sources, input));
        self.results = None;
        self.error = None;
        self.running = true;
        self.launch();
    }
    fn launch(&mut self) {
        if self.active.is_some() {
            return;
        }
        let Some((id, sources, input)) = self.pending.take() else {
            return;
        };
        let latest = Arc::clone(&self.latest);
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let result = search_all(sources, &input, || {
                latest.load(std::sync::atomic::Ordering::Relaxed) != id
            });
            let _ = sender.send((id, result));
        });
        self.active = Some(receiver);
    }
    pub fn poll(&mut self) {
        let Some(receiver) = &self.active else {
            return;
        };
        let response = match receiver.try_recv() {
            Ok(value) => Some(value),
            Err(std::sync::mpsc::TryRecvError::Empty) => return,
            Err(_) => None,
        };
        self.active = None;
        match response {
            Some((id, result)) if id == self.latest.load(std::sync::atomic::Ordering::Relaxed) => {
                self.running = false;
                match result {
                    Ok(results) => self.results = Some(results),
                    Err(error) => self.error = Some(error.to_string()),
                }
            }
            None if self.pending.is_none() && self.running => {
                self.error = Some("전체 검색 작업이 예기치 않게 종료됐습니다".into());
                self.running = false;
            }
            _ => {}
        }
        self.launch();
    }
    pub fn cancel(&mut self) {
        self.latest
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.pending = None;
        self.results = None;
        self.error = None;
        self.running = false;
    }
}
impl Drop for GlobalController {
    fn drop(&mut self) {
        self.cancel();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::SortField;
    #[test]
    fn sorting_across_locations_preserves_case_folding_path_ties_and_unknown_dates() {
        use crate::search::{item_path, EntryKind};
        let root =
            std::env::temp_dir().join(format!("diskweave-global-sort-{}", std::process::id()));
        for name in ["a", "b"] {
            std::fs::create_dir_all(root.join(name)).unwrap();
        }
        for (path, bytes) in [
            ("a/alpha.bin", 3),
            ("a/Beta.bin", 7),
            ("b/ALPHA.bin", 5),
            ("b/gamma.bin", 7),
        ] {
            std::fs::write(root.join(path), vec![0; bytes]).unwrap();
        }
        let sources: Vec<_> = ["a", "b"]
            .into_iter()
            .enumerate()
            .map(|(id, name)| {
                let mut scan = crate::scanner::scan(&root.join(name), || false).unwrap();
                for row in 0..scan.files.len() {
                    let file = &mut scan.files[row];
                    file.modified = match file.name.to_str().unwrap() {
                        "alpha.bin" => {
                            Some(std::time::UNIX_EPOCH + std::time::Duration::from_secs(1))
                        }
                        "ALPHA.bin" => {
                            Some(std::time::UNIX_EPOCH + std::time::Duration::from_secs(2))
                        }
                        _ => None,
                    };
                }
                Snapshot {
                    entries: None,
                    id: id as u64,
                    generation: 1,
                    scan: Arc::new(scan),
                }
            })
            .collect();
        for (sort, ascending, descending) in [
            (
                SortField::Name,
                ["a/alpha.bin", "b/ALPHA.bin", "a/Beta.bin", "b/gamma.bin"],
                ["b/gamma.bin", "a/Beta.bin", "a/alpha.bin", "b/ALPHA.bin"],
            ),
            (
                SortField::Path,
                ["a/Beta.bin", "a/alpha.bin", "b/ALPHA.bin", "b/gamma.bin"],
                ["b/gamma.bin", "b/ALPHA.bin", "a/alpha.bin", "a/Beta.bin"],
            ),
            (
                SortField::Size,
                ["a/alpha.bin", "b/ALPHA.bin", "a/Beta.bin", "b/gamma.bin"],
                ["a/Beta.bin", "b/gamma.bin", "b/ALPHA.bin", "a/alpha.bin"],
            ),
            (
                SortField::Modified,
                ["a/alpha.bin", "b/ALPHA.bin", "a/Beta.bin", "b/gamma.bin"],
                ["b/ALPHA.bin", "a/alpha.bin", "a/Beta.bin", "b/gamma.bin"],
            ),
        ] {
            for (descending, expected) in [(false, ascending), (true, descending)] {
                let result = search_all(
                    sources.clone(),
                    &SearchInput {
                        kind: EntryKind::Files,
                        sort,
                        descending,
                        ..Default::default()
                    },
                    || false,
                )
                .unwrap();
                let paths: Vec<_> = result
                    .hits
                    .iter()
                    .map(|hit| item_path(&result.sources[hit.source].scan, hit.item))
                    .collect();
                assert_eq!(
                    paths,
                    expected.map(|p| root.join(p)),
                    "{sort:?} descending={descending}"
                );
            }
        }
        assert_eq!(
            search_all(sources, &SearchInput::default(), || true)
                .err()
                .unwrap()
                .kind(),
            io::ErrorKind::Interrupted
        );
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn global_controller_only_publishes_latest_request_and_reports_errors() {
        use std::time::{Duration, Instant};
        let root = std::env::temp_dir().join(format!(
            "diskweave-global-controller-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("match.bin"), [0]).unwrap();
        let source = Snapshot {
            entries: None,
            id: 1,
            generation: 1,
            scan: Arc::new(crate::scanner::scan(&root, || false).unwrap()),
        };
        let mut controller = GlobalController::default();
        controller.submit(
            vec![source.clone()],
            SearchInput {
                text: "match".into(),
                ..Default::default()
            },
        );
        controller.submit(
            vec![source.clone()],
            SearchInput {
                text: "absent".into(),
                ..Default::default()
            },
        );
        let deadline = Instant::now() + Duration::from_secs(5);
        while controller.running && Instant::now() < deadline {
            controller.poll();
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(controller.results.as_ref().unwrap().hits.is_empty());
        controller.submit(
            vec![source.clone()],
            SearchInput {
                min_size: "bad".into(),
                ..Default::default()
            },
        );
        let deadline = Instant::now() + Duration::from_secs(5);
        while controller.running && Instant::now() < deadline {
            controller.poll();
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(controller.error.is_some());
        controller.submit(vec![source], SearchInput::default());
        controller.cancel();
        controller.poll();
        assert!(!controller.running);
        assert!(controller.results.is_none());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn global_order_and_open_requests_keep_the_correct_source() {
        let root = std::env::temp_dir().join(format!("diskweave-global-{}", std::process::id()));
        std::fs::create_dir_all(root.join("a")).unwrap();
        std::fs::create_dir(root.join("b")).unwrap();
        std::fs::write(root.join("a").join("same.bin"), [0; 3]).unwrap();
        std::fs::write(root.join("b").join("same.bin"), [0; 9]).unwrap();
        let sources = vec![
            Snapshot {
                entries: None,
                id: 10,
                generation: 1,
                scan: Arc::new(crate::scanner::scan(&root.join("a"), || false).unwrap()),
            },
            Snapshot {
                entries: None,
                id: 20,
                generation: 4,
                scan: Arc::new(crate::scanner::scan(&root.join("b"), || false).unwrap()),
            },
        ];
        let input = SearchInput {
            text: "same".into(),
            ..Default::default()
        };
        let hits = search_all(sources, &input, || false).unwrap();
        assert_eq!(hits.hits.len(), 2);
        assert_eq!(hits.sources[hits.hits[0].source].id, 20);
        assert_eq!(hits.sources[hits.hits[0].source].generation, 4);
        for hit in &hits.hits {
            let source = &hits.sources[hit.source];
            let expected = crate::search::item_path(&source.scan, hit.item);
            let request = crate::file_open::OpenRequest::for_item(
                &source.scan,
                hit.item,
                crate::file_open::OpenAction::OpenFile,
            );
            let explorer = source.analyze(hit.item);
            assert_eq!(explorer.selected(), hit.item);
            assert_eq!(
                source.scan.folders[explorer.folder()].path,
                expected.parent().unwrap()
            );
            crate::file_open::open_with(request, |validated| {
                assert_eq!(validated.path(), expected);
                Ok(())
            })
            .unwrap();
        }
        std::fs::remove_dir_all(root).unwrap();
    }
}
