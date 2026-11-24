use crate::{
    explorer::Item,
    file_open::TargetKind,
    global_search::GlobalController,
    locations::Locations,
    search::{EntryKind, SearchInput, SearchMode},
};
use std::{
    io,
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime},
};

mod live;
pub use live::Hit;
use live::LiveSearch;

impl Hit {
    pub fn analysis_folder(&self) -> Option<&Path> {
        match self.kind {
            TargetKind::Folder => Some(&self.path),
            TargetKind::File => self.path.parent(),
        }
    }

    pub fn verify_for_analysis(&self) -> io::Result<std::fs::Metadata> {
        use std::os::windows::fs::MetadataExt;
        let folder = self
            .analysis_folder()
            .ok_or_else(|| io::Error::other("분석할 폴더를 확인할 수 없습니다"))?;
        live::check_root(folder)?;
        let current = std::fs::symlink_metadata(&self.path)?;
        let changed = || io::Error::other("검색 결과의 항목이 바뀌었습니다. 다시 검색해 주세요");
        if current.is_dir() != (self.kind == TargetKind::Folder)
            || current.file_attributes() & 0x400 != 0
        {
            return Err(changed());
        }
        if let Some(expected) = self.identity {
            let observed = crate::file_ops::Target::capture(&self.path)?;
            if observed.directory != current.is_dir() || observed.identity() != expected {
                return Err(changed());
            }
        }
        Ok(current)
    }

    pub fn analysis_item(&self, scan: &crate::scanner::ScanResult) -> Option<Item> {
        let relative =
            crate::locations::relative_folder(&scan.folders[0].path, self.analysis_folder()?)
                .ok()??;
        self.analysis_item_at(scan, relative, None)
    }

    pub fn analysis_item_at(
        &self,
        scan: &crate::scanner::ScanResult,
        relative: &Path,
        created: Option<SystemTime>,
    ) -> Option<Item> {
        let folder = scan.folder_index(relative).ok()?;
        if scan.folders[folder].access.unreadable() {
            return None;
        }
        match self.kind {
            TargetKind::Folder => (!scan.folders[folder]
                .created
                .zip(created)
                .is_some_and(|(scanned, current)| scanned != current))
            .then_some(Item::Folder(folder)),
            TargetKind::File => {
                let file = scan.file_named(folder, self.path.file_name()?)?;
                let observed = scan.files[file].extra.identity.value();
                if self.identity.is_some() && observed.is_some() && observed != self.identity {
                    return None;
                }
                Some(Item::File(file))
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Scope {
    #[default]
    Scanned,
    Folder,
    Computer,
}

impl Scope {
    pub fn label(self) -> &'static str {
        match self {
            Self::Scanned => "스캔한 위치",
            Self::Folder => "폴더 지정",
            Self::Computer => "내 PC 전체",
        }
    }
}

#[derive(Default)]
pub struct QuickSearch {
    pub visible: bool,
    pub focus: bool,
    pub text: String,
    pub mode: SearchMode,
    pub selection: Option<usize>,
    pub job: GlobalController,
    kind: EntryKind,
    scope: Scope,
    folder: PathBuf,
    pub live: LiveSearch,
    request: Option<(String, SearchMode, EntryKind, u64)>,
    due: Option<Instant>,
    return_hit: Option<Hit>,
    timing: Option<SearchTiming>,
}

struct SearchTiming {
    started: Instant,
    finished: Option<Instant>,
}

impl QuickSearch {
    pub fn elapsed(&self, now: Instant) -> Option<Duration> {
        let timing = self.timing.as_ref()?;
        Some(
            timing
                .finished
                .unwrap_or(now)
                .saturating_duration_since(timing.started),
        )
    }

    fn finish_timing(&mut self, now: Instant) {
        if let Some(timing) = &mut self.timing {
            timing.finished.get_or_insert(now);
        }
    }

    pub fn pending(&self) -> bool {
        self.visible && (self.due.is_some() || self.job.running || self.live.running())
    }
    pub fn scope(&self) -> Scope {
        self.scope
    }
    fn scanned_query_matches(&self) -> bool {
        self.request.as_ref().is_some_and(|(text, mode, kind, _)| {
            text == &self.text && *mode == self.mode && *kind == self.kind
        })
    }
    pub fn scanned_request_current(&self, epoch: u64) -> bool {
        self.due.is_none()
            && self.scanned_query_matches()
            && self
                .request
                .as_ref()
                .is_some_and(|(_, _, _, current)| *current == epoch)
    }
    pub fn set_scope(&mut self, scope: Scope) {
        if scope != self.scope {
            self.scope = scope;
            self.job.cancel();
            self.request = None;
            self.due = None;
            self.selection = None;
            self.live.clear();
            self.timing = None;
        }
    }
    pub fn folder(&self) -> &std::path::Path {
        &self.folder
    }
    pub fn kind(&self) -> EntryKind {
        self.kind
    }
    pub fn set_kind(&mut self, kind: EntryKind) {
        if self.kind != kind {
            self.kind = kind;
            self.selection = None;
            self.invalidate_live();
        }
    }
    pub fn set_folder(&mut self, path: PathBuf) {
        self.set_scope(Scope::Folder);
        self.folder = path;
        self.invalidate_live();
    }
    pub fn drop_folder(&mut self, paths: &[PathBuf]) -> io::Result<()> {
        let [path] = paths else {
            return Err(io::Error::other("검색할 폴더를 하나만 놓아 주세요"));
        };
        live::check_root(path)?;
        self.set_folder(path.clone());
        Ok(())
    }
    fn live_input_matches(&self, input: &live::Input) -> bool {
        input.folder.as_deref() == (self.scope == Scope::Folder).then_some(self.folder())
            && input.text == self.text.trim()
            && input.mode == self.mode
            && input.kind == self.kind
    }
    pub fn invalidate_live(&mut self) {
        if self
            .live
            .input
            .as_ref()
            .is_some_and(|input| !self.live_input_matches(input))
        {
            self.live.clear();
            self.selection = None;
            self.timing = None;
        }
    }
    pub fn start_live(&mut self) -> io::Result<()> {
        self.start_live_using(crate::drives::fixed_roots)
    }
    fn start_live_using(
        &mut self,
        fixed_roots: impl FnOnce() -> io::Result<Vec<PathBuf>> + Send + 'static,
    ) -> io::Result<()> {
        let text = self.text.trim();
        if self.scope == Scope::Scanned || text.is_empty() {
            return Err(io::Error::other("검색 범위와 검색어를 확인하세요"));
        }
        let folder = (self.scope == Scope::Folder).then_some(self.folder());
        if folder.is_some_and(|path| !path.is_absolute()) {
            return Err(io::Error::other("검색할 폴더의 전체 경로를 입력하세요"));
        }
        let input = live::Input {
            folder: folder.map(Path::to_path_buf),
            text: text.to_owned(),
            mode: self.mode,
            kind: self.kind,
        };
        let started = Instant::now();
        self.live.start(input, fixed_roots)?;
        self.timing = Some(SearchTiming {
            started,
            finished: None,
        });
        self.selection = None;
        Ok(())
    }
    #[cfg(test)]
    fn live_hits(&self) -> &[Hit] {
        &self.live.hits
    }
    pub fn remove_recycled(&mut self, path: &std::path::Path) {
        if self.live.remove_recycled(path) && self.scope != Scope::Scanned {
            self.selection = None;
        }
    }
    pub fn count(&self) -> usize {
        if self.scope == Scope::Scanned {
            self.job.results.as_ref().map_or(0, |r| r.hits.len())
        } else {
            self.live.hits.len()
        }
    }
    pub fn row(&self, row: usize) -> Option<Hit> {
        if self.scope != Scope::Scanned {
            return self.live.hits.get(row).cloned();
        }
        let results = self.job.results.as_ref()?;
        let hit = results.hits.get(row)?;
        let scan = &results.sources[hit.source].scan;
        let (kind, identity) = match hit.item {
            Item::Folder(_) => (TargetKind::Folder, None),
            Item::File(id) => (TargetKind::File, scan.files[id].extra.identity.value()),
        };
        Some(Hit {
            path: crate::search::item_path(scan, hit.item),
            kind,
            bytes: Some(crate::search::item_bytes(scan, hit.item)),
            identity: identity.filter(|id| id.file != [0; 16]),
        })
    }
    pub fn selected_hit(&self, locations: &Locations) -> io::Result<Hit> {
        if self.scope == Scope::Scanned {
            self.selected(locations)?;
        } else if self
            .live
            .input
            .as_ref()
            .is_none_or(|input| !self.live_input_matches(input))
        {
            return Err(io::Error::other("현재 범위에서 다시 검색해 주세요"));
        }
        self.selection
            .and_then(|row| self.row(row))
            .ok_or_else(|| io::Error::other("항목을 선택하세요"))
    }
    pub fn open(&mut self) {
        self.visible = true;
        self.focus = true;
        if self.return_hit.is_none() {
            self.request = None;
        }
    }
    pub fn leave_for_analysis(&mut self) {
        self.return_hit = self.selection.and_then(|row| self.row(row));
        self.live.stop();
        self.visible = false;
    }
    pub fn can_return_from_analysis(&self) -> bool {
        !self.visible && self.return_hit.is_some()
    }
    pub fn hide(&mut self) {
        self.visible = false;
        self.job.cancel();
        self.due = None;
        self.live.stop();
        self.request = None;
        self.selection = None;
        self.return_hit = None;
    }
    pub fn toggle(&mut self, focused: bool) {
        if self.visible && focused {
            self.hide()
        } else {
            self.open()
        }
    }
    pub fn tick(&mut self, locations: &Locations, now: Instant) {
        self.invalidate_live();
        self.live.poll();
        if self.scope != Scope::Scanned && !self.live.running() {
            self.finish_timing(now);
        }
        if !self.visible {
            return;
        }
        if self.scope != Scope::Scanned {
            self.return_hit = None;
            return;
        }
        let edited = !self.scanned_query_matches();
        if edited
            || self
                .request
                .as_ref()
                .is_some_and(|(_, _, _, epoch)| *epoch != locations.epoch)
        {
            if edited {
                self.job.cancel();
                self.due = Some(now + Duration::from_millis(150));
                self.return_hit = None;
            } else if self.due.is_none() {
                self.due = Some(now);
            }
            self.selection = None;
            self.timing = None;
            self.request = Some((self.text.clone(), self.mode, self.kind, locations.epoch));
        }
        if self.text.trim().is_empty() {
            self.due = None;
            return;
        }
        if self.due.is_some_and(|due| now >= due) {
            self.due = None;
            self.timing = Some(SearchTiming {
                started: now,
                finished: None,
            });
            self.job.submit(
                locations.snapshots(),
                SearchInput {
                    text: self.text.clone(),
                    mode: self.mode,
                    kind: self.kind,
                    ..Default::default()
                },
            );
        }
        self.job.poll();
        if !self.job.running && self.due.is_none() {
            self.finish_timing(now);
            if let (Some(hit), Some(results)) = (self.return_hit.take(), &self.job.results) {
                let candidates: Vec<_> = results
                    .sources
                    .iter()
                    .map(|s| hit.analysis_item(&s.scan))
                    .collect();
                self.selection = results
                    .hits
                    .iter()
                    .position(|row| candidates[row.source] == Some(row.item));
            }
        }
    }
    pub fn step(&mut self, direction: i32) {
        let count = self.count();
        if count == 0 {
            self.selection = None;
            return;
        }
        self.selection = Some(match self.selection {
            None => {
                if direction < 0 {
                    count - 1
                } else {
                    0
                }
            }
            Some(row) => (row as i64 + i64::from(direction)).clamp(0, count as i64 - 1) as usize,
        });
    }
    pub fn selected(&self, locations: &Locations) -> io::Result<(u64, u64, Item)> {
        if self.scope != Scope::Scanned || !self.scanned_query_matches() {
            return Err(io::Error::other("현재 범위에서 다시 검색해 주세요"));
        }
        let r = self
            .job
            .results
            .as_ref()
            .ok_or_else(|| io::Error::other("검색 결과를 기다려 주세요"))?;
        let hit = self
            .selection
            .and_then(|i| r.hits.get(i))
            .ok_or_else(|| io::Error::other("항목을 선택하세요"))?;
        let source = &r.sources[hit.source];
        if locations.get(source.id).is_none_or(|e| {
            e.stale
                || e.generation != source.generation
                || e.scan
                    .completed()
                    .is_none_or(|c| !std::sync::Arc::ptr_eq(&c.result, &source.scan))
        }) {
            return Err(io::Error::other(
                "결과가 변경되었습니다. 다시 검색해 주세요",
            ));
        }
        Ok((source.id, source.generation, hit.item))
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
    fn dropped_folder_searches_unscanned_files_and_folders_only_after_start() {
        let root =
            std::env::temp_dir().join(format!("diskweave-quick-drop-{}", std::process::id()));
        fs::create_dir_all(root.join("보고서")).unwrap();
        fs::write(root.join("분기 보고서.xlsx"), [1, 2]).unwrap();
        fs::write(root.join("notes.txt"), []).unwrap();
        let locations = Locations::default();
        let mut quick = QuickSearch::default();
        quick.open();
        quick.text = "보고서".into();
        assert!(quick.drop_folder(&[root.join("notes.txt")]).is_err());
        quick.drop_folder(std::slice::from_ref(&root)).unwrap();
        assert_eq!(quick.scope(), Scope::Folder);
        assert_eq!(quick.text, "보고서");
        quick.tick(&locations, Instant::now());
        assert!(!quick.pending());
        quick.start_live().unwrap();
        let until = Instant::now() + Duration::from_secs(5);
        while quick.pending() {
            quick.tick(&locations, Instant::now());
            assert!(Instant::now() < until);
            std::thread::yield_now();
        }
        let mut paths: Vec<_> = quick
            .live_hits()
            .iter()
            .map(|hit| hit.path.clone())
            .collect();
        paths.sort();
        let mut expected = vec![root.join("보고서"), root.join("분기 보고서.xlsx")];
        expected.sort();
        assert_eq!(paths, expected);
        quick.step(1);
        let selected = quick.selected_hit(&locations).unwrap().path;
        quick.text = "  보고서  ".into();
        quick.tick(&locations, Instant::now());
        assert_eq!(quick.selected_hit(&locations).unwrap().path, selected);
        for hit in quick.live_hits() {
            let actual = crate::file_ops::Target::capture(&hit.path).unwrap();
            assert_eq!(hit.identity, Some(actual.identity()));
        }
        assert!(quick
            .live_hits()
            .iter()
            .any(|hit| hit.kind == TargetKind::File));
        assert!(quick
            .live_hits()
            .iter()
            .any(|hit| hit.kind == TargetKind::Folder));
        quick.remove_recycled(&root.join("보고서"));
        assert_eq!(quick.live_hits().len(), 1);
        quick.mode = SearchMode::Path;
        quick.tick(&locations, Instant::now());
        assert_eq!(quick.count(), 0);
        assert!(locations.entries.is_empty());
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn computer_search_uses_each_fixed_root_and_discards_a_cancelled_request() {
        let root = std::env::temp_dir().join(format!("diskweave-quick-pc-{}", std::process::id()));
        for name in ["a", "b", "not-local"] {
            fs::create_dir_all(root.join(name).join("result folder")).unwrap();
            fs::write(root.join(name).join("result.txt"), []).unwrap();
        }
        let locations = Locations::default();
        let mut quick = QuickSearch::default();
        quick.open();
        quick.set_scope(Scope::Computer);
        quick.text = "result".into();
        quick.set_kind(EntryKind::Files);
        let roots = vec![root.join("a"), root.join("b")];
        let use_roots = roots.clone();
        let (release, wait) = std::sync::mpsc::channel();
        quick
            .start_live_using(move || {
                wait.recv_timeout(Duration::from_secs(5)).unwrap();
                Ok(use_roots)
            })
            .unwrap();
        quick.live.stop();
        assert!(quick.pending());
        assert!(quick.start_live_using(|| Ok(vec![])).is_err());
        release.send(()).unwrap();
        let until = Instant::now() + Duration::from_secs(5);
        while quick.pending() {
            quick.tick(&locations, Instant::now());
            assert!(Instant::now() < until);
            std::thread::yield_now();
        }
        assert!(quick.live_hits().is_empty());
        assert!(quick.live.stopped);
        assert!(quick.live.error.is_none());

        let file_roots = roots.clone();
        quick.start_live_using(move || Ok(file_roots)).unwrap();
        while quick.pending() {
            quick.tick(&locations, Instant::now());
            assert!(Instant::now() < until);
            std::thread::yield_now();
        }
        let mut paths: Vec<_> = (0..quick.count())
            .map(|row| quick.row(row).unwrap().path)
            .collect();
        paths.sort();
        assert_eq!(
            paths,
            [root.join("a/result.txt"), root.join("b/result.txt")]
        );
        quick.set_kind(EntryKind::Folders);
        assert!(!quick.pending());
        assert_eq!(quick.count(), 0);
        quick.start_live_using(move || Ok(roots)).unwrap();
        while quick.pending() {
            quick.tick(&locations, Instant::now());
            assert!(Instant::now() < until);
            std::thread::yield_now();
        }
        assert_eq!(quick.count(), 2);
        assert_eq!(quick.row(0).unwrap().kind, TargetKind::Folder);
        quick.step(1);
        assert!(quick.selected_hit(&locations).is_ok());
        quick.text = "other".into();
        assert!(quick.selected_hit(&locations).is_err());
        quick.tick(&locations, Instant::now());
        assert!(quick.live_hits().is_empty());
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn computer_search_applies_the_limit_only_to_the_requested_kind() {
        let owned = crate::duplicates::tests::Fixture::new();
        let earlier = owned.0.join("earlier");
        let later = owned.0.join("later");
        fs::create_dir(&earlier).unwrap();
        fs::create_dir_all(later.join("match-folder")).unwrap();
        let parts: Vec<_> = (0..4)
            .map(|part| {
                let path = earlier.join(format!("part-{part}"));
                fs::create_dir(&path).unwrap();
                path
            })
            .collect();
        for row in 0..10_001 {
            fs::write(
                parts[row % parts.len()].join(format!("match-{row:05}.txt")),
                [],
            )
            .unwrap();
        }
        let locations = Locations::default();
        let mut quick = QuickSearch::default();
        quick.set_scope(Scope::Computer);
        quick.set_kind(EntryKind::Folders);
        quick.open();
        quick.text = "match".into();
        let roots = vec![later, earlier];
        let folder_roots = roots.clone();
        quick.start_live_using(move || Ok(folder_roots)).unwrap();
        let until = Instant::now() + Duration::from_secs(30);
        while quick.pending() {
            quick.tick(&locations, Instant::now());
            assert!(Instant::now() < until);
            std::thread::sleep(Duration::from_millis(2));
        }
        assert!(!quick.live.coverage.limited);
        assert_eq!(quick.count(), 1);
        assert_eq!(
            quick.row(0).unwrap().path.file_name().unwrap(),
            "match-folder"
        );

        quick.set_kind(EntryKind::Files);
        let stop_roots = roots.clone();
        quick.start_live_using(move || Ok(stop_roots)).unwrap();
        let until = Instant::now() + Duration::from_secs(30);
        while quick.pending() && quick.count() == 0 {
            quick.tick(&locations, Instant::now());
            assert!(Instant::now() < until);
            std::thread::sleep(Duration::from_millis(2));
        }
        assert!(quick.pending());
        let before_stop = quick.count();
        assert!(before_stop > 0);
        quick.live.stop();
        while quick.pending() {
            quick.tick(&locations, Instant::now());
            assert!(Instant::now() < until);
            std::thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(quick.count(), before_stop);
        assert!(quick.live.error.is_none());

        quick.start_live_using(move || Ok(roots)).unwrap();
        let until = Instant::now() + Duration::from_secs(30);
        while quick.pending() {
            quick.tick(&locations, Instant::now());
            assert!(Instant::now() < until);
            std::thread::sleep(Duration::from_millis(2));
        }
        assert!(quick.live.error.is_none());
        assert!(quick.live.coverage.limited);
        assert_eq!(quick.count(), 10_000);
        let paths: std::collections::HashSet<_> =
            quick.live_hits().iter().map(|hit| &hit.path).collect();
        assert_eq!(paths.len(), 10_000);
    }
    #[test]
    fn quick_search_uses_live_locations_and_rejects_changed_results() {
        let root =
            std::env::temp_dir().join(format!("diskweave-quick-search-{}", std::process::id()));
        fs::create_dir_all(root.join("a")).unwrap();
        fs::create_dir_all(root.join("b")).unwrap();
        fs::write(root.join("a/model.bin"), [1; 3]).unwrap();
        fs::write(root.join("b/MODEL.bin"), [2; 9]).unwrap();
        let mut locations = crate::locations::Locations::default();
        let a = locations.add(&root.join("a")).unwrap();
        let b = locations.add(&root.join("b")).unwrap();
        locations.start_selected(&[a, b]).unwrap();
        let until = Instant::now() + Duration::from_secs(5);
        while locations.is_running() {
            locations.poll();
            assert!(Instant::now() < until);
            std::thread::yield_now();
        }
        let mut quick = QuickSearch::default();
        quick.open();
        quick.tick(&locations, Instant::now());
        assert!(quick.job.results.is_none());
        quick.text = "model".into();
        quick.tick(&locations, Instant::now());
        assert!(quick.pending());
        quick.tick(&locations, Instant::now() + Duration::from_secs(1));
        while quick.job.running {
            quick.tick(&locations, Instant::now() + Duration::from_secs(1));
            assert!(Instant::now() < until);
            std::thread::yield_now();
        }
        assert_eq!(quick.job.results.as_ref().unwrap().hits.len(), 2);
        assert!(!quick.pending());
        quick.step(1);
        let target = quick.selected(&locations).unwrap();
        assert_eq!(target.0, b);
        quick.step(1);
        assert_eq!(quick.selected(&locations).unwrap().0, a);
        quick.step(1);
        assert_eq!(quick.selection, Some(1));
        quick.step(-1);
        quick.step(-1);
        assert_eq!(quick.selection, Some(0));
        let previous = quick.job.results.clone().unwrap();
        fs::create_dir(root.join("unscanned")).unwrap();
        locations.add(&root.join("unscanned")).unwrap();
        quick.tick(&locations, Instant::now());
        assert!(quick.selection.is_none());
        assert!(!quick.pending());
        assert!(std::sync::Arc::ptr_eq(
            quick.job.results.as_ref().unwrap(),
            &previous
        ));
        // 이전 요청이 끝나기 전 조건 변경 시 최신 경로 검색만 게시
        quick.text = "no-match".into();
        quick.tick(&locations, Instant::now());
        quick.tick(&locations, Instant::now() + Duration::from_secs(1));
        quick.mode = SearchMode::Path;
        quick.text = "b\\model".into();
        quick.tick(&locations, Instant::now());
        quick.tick(&locations, Instant::now() + Duration::from_secs(1));
        while quick.job.running {
            quick.tick(&locations, Instant::now() + Duration::from_secs(1));
            assert!(Instant::now() < until);
            std::thread::yield_now();
        }
        assert_eq!(quick.job.results.as_ref().unwrap().hits.len(), 1);
        quick.step(1);
        assert_eq!(quick.selected(&locations).unwrap().0, b);
        locations.tracking_failed(b);
        assert!(quick.selected(&locations).is_err());
        let changed_at = Instant::now();
        quick.tick(&locations, changed_at);
        assert!(quick.selection.is_none());
        assert!(quick.job.running || quick.job.results.is_some());
        while quick.job.running {
            quick.tick(&locations, changed_at);
            assert!(Instant::now() < until);
            std::thread::yield_now();
        }
        assert!(quick.job.results.as_ref().unwrap().hits.is_empty());
        quick.hide();
        assert!(!quick.visible);
        assert_eq!(quick.text, "b\\model");
        quick.open();
        assert!(quick.visible);
        quick.text.clear();
        quick.tick(&locations, Instant::now());
        assert!(quick.job.results.is_none());
        fs::remove_dir_all(root).unwrap();
    }
}
