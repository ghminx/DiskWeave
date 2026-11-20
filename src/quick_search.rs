use crate::{
    explorer::Item,
    global_search::GlobalController,
    locations::Locations,
    search::{SearchInput, SearchMode},
};
use std::{
    io,
    time::{Duration, Instant},
};
#[derive(Default)]
pub struct QuickSearch {
    pub visible: bool,
    pub focus: bool,
    pub text: String,
    pub mode: SearchMode,
    pub selection: Option<usize>,
    pub job: GlobalController,
    request: Option<(String, SearchMode, u64)>,
    due: Option<Instant>,
}
impl QuickSearch {
    pub fn pending(&self) -> bool {
        self.visible && (self.due.is_some() || self.job.running)
    }
    pub fn open(&mut self) {
        self.visible = true;
        self.focus = true;
        self.request = None;
    }
    pub fn hide(&mut self) {
        self.visible = false;
        self.job.cancel();
        self.request = None;
        self.selection = None;
    }
    pub fn toggle(&mut self, focused: bool) {
        if self.visible && focused {
            self.hide()
        } else {
            self.open()
        }
    }
    pub fn tick(&mut self, locations: &Locations, now: Instant) {
        if !self.visible {
            return;
        }
        let request = (self.text.clone(), self.mode, locations.epoch);
        if self.request.as_ref() != Some(&request) {
            if self
                .request
                .as_ref()
                .is_none_or(|previous| previous.0 != request.0 || previous.1 != request.1)
            {
                self.job.cancel();
                self.due = Some(now + Duration::from_millis(150));
            } else if self.due.is_none() {
                self.due = Some(now);
            }
            self.selection = None;
            self.request = Some(request);
        }
        if self.text.trim().is_empty() {
            self.due = None;
            return;
        }
        if self.due.is_some_and(|due| now >= due) {
            self.due = None;
            self.job.submit(
                locations.snapshots(),
                SearchInput {
                    text: self.text.clone(),
                    mode: self.mode,
                    ..Default::default()
                },
            );
        }
        self.job.poll();
    }
    pub fn step(&mut self, direction: i32) {
        let count = self.job.results.as_ref().map_or(0, |r| r.hits.len());
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
