use crate::explorer::Item;
use crate::locations::Snapshot;
use crate::search::{PathKey, Query, SearchInput, SortField, SortKey};
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
    let (hits, unknown_dates) = if input.sort == SortField::Path {
        order_hits(
            &sources,
            &query,
            PathKey::new,
            |a, b| a.compare(b, input.descending, false),
            &cancelled,
        )?
    } else {
        order_hits(
            &sources,
            &query,
            |scan, item| SortKey::new(scan, item, input.sort),
            |a, b| a.compare(b, input.descending, false),
            &cancelled,
        )?
    };
    Ok(GlobalResults {
        sources,
        hits,
        unknown_dates,
    })
}

fn order_hits<'a, K>(
    sources: &'a [Snapshot],
    query: &Query,
    make_key: impl Fn(&'a crate::scanner::ScanResult, Item) -> K,
    compare: impl Fn(&K, &K) -> std::cmp::Ordering,
    cancelled: &impl Fn() -> bool,
) -> io::Result<(Vec<Hit>, usize)> {
    let mut keyed = Vec::new();
    let mut unknown_dates = 0;
    for (source, snapshot) in sources.iter().enumerate() {
        let result = crate::search::matching(&snapshot.scan, query, cancelled)?;
        unknown_dates += result.unknown_dates;
        for item in result.items {
            crate::output::check(cancelled)?;
            keyed.push((Hit { source, item }, make_key(&snapshot.scan, item)));
        }
    }
    keyed.sort_unstable_by(|a, b| compare(&a.1, &b.1));
    if cancelled() {
        return Err(io::Error::new(io::ErrorKind::Interrupted, "검색 취소"));
    }
    Ok((
        keyed.into_iter().map(|(hit, _)| hit).collect(),
        unknown_dates,
    ))
}

// 같은 검색 조건의 직전 결과를 확인된 변경분으로 갱신하고 순서가 불명확하면 전체 검색.
fn refresh_all(
    previous: &GlobalResults,
    sources: Vec<Snapshot>,
    input: &SearchInput,
    cancelled: impl Fn() -> bool,
) -> io::Result<GlobalResults> {
    crate::output::check(&cancelled)?;
    if sources.len() != previous.sources.len()
        || sources
            .iter()
            .zip(&previous.sources)
            .any(|(now, old)| now.id != old.id)
    {
        return search_all(sources, input, cancelled);
    }
    let query = input.compile(0)?;
    let mut changes = Vec::with_capacity(sources.len());
    let mut unknown_dates = previous.unknown_dates;
    for (now, old) in sources.iter().zip(&previous.sources) {
        if Arc::ptr_eq(&now.scan, &old.scan) {
            changes.push(None);
            continue;
        }
        let Some(change) =
            crate::search::matching_changes(&old.scan, &now.scan, &query, 0, &cancelled)?
        else {
            return search_all(sources, input, cancelled);
        };
        unknown_dates = unknown_dates - change.unknown_before + change.unknown_after;
        changes.push(Some(change));
    }
    let mut hits = Vec::with_capacity(previous.hits.len());
    for chunk in previous.hits.chunks(4096) {
        crate::output::check(&cancelled)?;
        hits.extend(chunk.iter().filter_map(|&hit| match &changes[hit.source] {
            Some(change) => change.retained(hit.item).map(|item| Hit { item, ..hit }),
            None => Some(hit),
        }));
    }
    for (source, change) in changes.into_iter().enumerate() {
        let Some(change) = change else { continue };
        for item in change.added {
            crate::output::check(&cancelled)?;
            let key = SortKey::new(&sources[source].scan, item, input.sort);
            let at = hits.partition_point(|hit| {
                SortKey::new(&sources[hit.source].scan, hit.item, input.sort)
                    .compare(&key, input.descending, false)
                    .is_lt()
            });
            hits.insert(at, Hit { source, item });
        }
    }
    crate::output::check(&cancelled)?;
    Ok(GlobalResults {
        sources,
        hits,
        unknown_dates,
    })
}

type SearchResponse = (u64, SearchInput, io::Result<GlobalResults>);

#[derive(Default)]
pub struct GlobalController {
    latest: Arc<std::sync::atomic::AtomicU64>,
    active: Option<std::sync::mpsc::Receiver<SearchResponse>>,
    pending: Option<(u64, Vec<Snapshot>, SearchInput)>,
    cached: Option<(SearchInput, Arc<GlobalResults>)>,
    pub results: Option<Arc<GlobalResults>>,
    pub error: Option<String>,
    pub running: bool,
}

impl GlobalController {
    pub fn submit(&mut self, sources: Vec<Snapshot>, input: SearchInput) {
        let id = self
            .latest
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            + 1;
        self.error = None;
        if let Some((key, cached)) = &self.cached {
            if key != &input {
                self.cached = None;
            } else if sources.len() == cached.sources.len()
                && sources.iter().zip(&cached.sources).all(|(now, old)| {
                    now.id == old.id
                        && now.generation == old.generation
                        && Arc::ptr_eq(&now.scan, &old.scan)
                        && now.entries.as_ref().map(Arc::as_ptr)
                            == old.entries.as_ref().map(Arc::as_ptr)
                })
            {
                self.pending = None;
                self.results = Some(Arc::clone(cached));
                self.running = false;
                return;
            }
        }
        self.pending = Some((id, sources, input));
        self.results = None;
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
        let previous = self.cached.as_ref().map(|(_, results)| Arc::clone(results));
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let cancelled = || latest.load(std::sync::atomic::Ordering::Relaxed) != id;
            let result = match previous {
                Some(previous) => refresh_all(&previous, sources, &input, cancelled),
                None => search_all(sources, &input, cancelled),
            };
            let _ = sender.send((id, input, result));
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
            Some((id, input, result))
                if id == self.latest.load(std::sync::atomic::Ordering::Relaxed) =>
            {
                self.running = false;
                match result {
                    Ok(results) => {
                        let results = Arc::new(results);
                        self.cached = Some((input, Arc::clone(&results)));
                        self.results = Some(results);
                    }
                    Err(error) => {
                        self.cached = None;
                        self.error = Some(error.to_string());
                    }
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
        self.cached = None;
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

    #[test]
    fn live_folder_move_updates_path_hits_without_traversing_unrelated_items() {
        let (previous, current) = crate::usn::stream::tests::folder_move_snapshots();
        let input = SearchInput {
            text: "z\\a".into(),
            mode: crate::search::SearchMode::Path,
            kind: crate::search::EntryKind::Files,
            ..Default::default()
        };
        let old = search_all(vec![previous], &input, || false).unwrap();
        assert!(old.hits.is_empty());

        let checks = std::cell::Cell::new(0);
        let refreshed = refresh_all(&old, vec![current], &input, || {
            checks.set(checks.get() + 1);
            checks.get() > 100
        })
        .unwrap();
        assert_eq!(refreshed.hits.len(), 1);
        assert!(
            crate::search::item_path(&refreshed.sources[0].scan, refreshed.hits[0].item)
                .ends_with(std::path::Path::new("z/a/moved.bin"))
        );
    }
    #[test]
    fn path_sort_preserves_full_windows_order_and_cancellation_across_scans() {
        use crate::search::{item_path, search, EntryKind};
        use std::cell::Cell;

        let fixture = crate::duplicates::tests::Fixture::new();
        let root = std::fs::canonicalize(&fixture.0).unwrap();
        let long = root.join("한글".repeat(55)).join("긴이름".repeat(45));
        std::fs::create_dir_all(&long).unwrap();
        let paths = [
            root.join("a.BIN"),
            root.join("a-뒤.BIN"),
            long.join("한글-한.BIN"),
            long.join("한글-가.BIN"),
            root.join("other").join("한글-가.BIN"),
        ];
        std::fs::create_dir(root.join("other")).unwrap();
        for path in &paths {
            std::fs::write(path, []).unwrap();
        }
        let whole = Arc::new(crate::scanner::scan(&root, || false).unwrap());
        let nested = Arc::new(crate::scanner::scan(&long, || false).unwrap());
        let sources = || {
            [&nested, &whole, &whole]
                .into_iter()
                .enumerate()
                .map(|(i, scan)| Snapshot {
                    id: i as u64 + 1,
                    generation: 0,
                    entries: None,
                    scan: Arc::clone(scan),
                })
                .collect()
        };

        for descending in [false, true] {
            let input = SearchInput {
                text: ".bin".into(),
                kind: EntryKind::Files,
                sort: SortField::Path,
                descending,
                ..Default::default()
            };
            let by_path = |a: &std::path::PathBuf, b: &std::path::PathBuf| {
                if descending {
                    b.cmp(a)
                } else {
                    a.cmp(b)
                }
            };
            let query = input.compile(0).unwrap();
            let local = search(&whole, &query, || false).unwrap();
            let mut local_expected = paths.to_vec();
            local_expected.sort_by(by_path);
            assert_eq!(
                local
                    .items
                    .into_iter()
                    .map(|item| item_path(&whole, item))
                    .collect::<Vec<_>>(),
                local_expected
            );

            let calls = Cell::new(0);
            let global = search_all(sources(), &input, || {
                calls.set(calls.get() + 1);
                false
            })
            .unwrap();
            let mut global_expected = local_expected.clone();
            global_expected.extend(local_expected);
            global_expected.extend(paths.iter().filter(|path| path.starts_with(&long)).cloned());
            global_expected.sort_by(by_path);
            assert_eq!(
                global
                    .hits
                    .iter()
                    .map(|hit| item_path(&global.sources[hit.source].scan, hit.item))
                    .collect::<Vec<_>>(),
                global_expected
            );
            for stop in [0, calls.get() / 2, calls.get() - 1] {
                let count = Cell::new(0);
                let outcome = search_all(sources(), &input, || {
                    count.set(count.get() + 1);
                    count.get() > stop
                });
                assert_eq!(outcome.err().unwrap().kind(), io::ErrorKind::Interrupted);
            }
        }
    }

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
        let mut edited = sources[0].scan.copy_for_update(true);
        let row = edited
            .files
            .iter()
            .position(|file| file.name == "Beta.bin")
            .unwrap();
        edited.files[row].logical_bytes = 11;
        edited.folders[0].logical_bytes += 4;
        edited.files[row].modified = Some(std::time::UNIX_EPOCH);
        edited.partial = Some(Arc::new(crate::scanner::PartialUpdate {
            previous: Arc::downgrade(&sources[0].scan),
            affected: [0].into_iter().collect(),
            search: Some(crate::scanner::SearchDelta {
                before: vec![Item::File(row)],
                after: vec![Item::File(row)],
                file_rows: None,
                folder_rows: None,
            }),
        }));
        let edited = Arc::new(edited);
        let mut updated = sources.clone();
        updated[0].scan = edited;
        updated[0].generation += 1;
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
                let input = SearchInput {
                    kind: EntryKind::Files,
                    sort,
                    descending,
                    ..Default::default()
                };
                let result = search_all(sources.clone(), &input, || false).unwrap();
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
                let patched = refresh_all(&result, updated.clone(), &input, || false).unwrap();
                let full = search_all(updated.clone(), &input, || false).unwrap();
                assert_eq!(patched.hits, full.hits, "{sort:?} descending={descending}");
                assert_eq!(patched.sources[0].generation, 2);
            }
        }
        let input = SearchInput {
            from_date: "1969-01-01".into(),
            to_date: "1971-01-01".into(),
            kind: EntryKind::Files,
            ..Default::default()
        };
        let old = search_all(sources.clone(), &input, || false).unwrap();
        let patched = refresh_all(&old, updated.clone(), &input, || false).unwrap();
        assert_eq!((old.unknown_dates, patched.unknown_dates), (2, 1));
        assert_eq!(
            patched.hits,
            search_all(updated, &input, || false).unwrap().hits
        );
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
    fn unchanged_sources_keep_results_and_discard_an_obsolete_refresh() {
        use std::time::{Duration, Instant};
        let (source, _) = crate::usn::stream::tests::folder_move_snapshots();
        let mut controller = GlobalController::default();
        let input = SearchInput::default();
        controller.submit(vec![source.clone()], input.clone());
        let deadline = Instant::now() + Duration::from_secs(5);
        while controller.running {
            controller.poll();
            assert!(Instant::now() < deadline);
            std::thread::yield_now();
        }
        let cached = controller.results.clone().unwrap();
        let mut renewed = source.clone();
        renewed.generation += 1;
        controller.submit(vec![renewed], input.clone());
        assert!(controller.running);
        controller.submit(vec![source], input);
        assert!(!controller.running);
        assert!(Arc::ptr_eq(controller.results.as_ref().unwrap(), &cached));

        controller.submit(vec![], SearchInput::default());
        while controller.running {
            controller.poll();
            assert!(Instant::now() < deadline);
            std::thread::yield_now();
        }
        assert!(controller.results.as_ref().unwrap().sources.is_empty());
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
