use crate::{
    explorer::Item,
    global_search::GlobalController,
    locations::{Locations, Snapshot},
    navigation::{Resolved, ViewState},
    search_job::{SearchController, SearchStatus},
};
use std::{
    collections::HashSet,
    path::{Path, PathBuf},
};

/// 화면에 보이는 비활성 목록 한 곳의 행. 나머지 탭에는 경로와 탐색 상태만 보관.
#[derive(Default)]
pub struct PaneRows {
    key: Option<(u64, u64, Option<u64>)>,
    resolved: Option<Resolved>,
    selected: HashSet<Item>,
    selected_paths: HashSet<PathBuf>,
    local: SearchController,
    global: GlobalController,
}

impl PaneRows {
    pub fn clear(&mut self) {
        if self.key.is_some() {
            *self = Self::default();
        }
    }

    pub fn refresh(&mut self, view: &ViewState, locations: &Locations) {
        let Some(source) = locations.completed_snapshot(view.location) else {
            self.clear();
            return;
        };
        let key = (
            source.id,
            source.generation,
            view.global.then_some(locations.epoch),
        );
        if self.key != Some(key) {
            let resolved = view.resolve_on(&source).ok();
            let Some(resolved) = resolved else {
                self.clear();
                return;
            };
            self.selected = resolved.selected.iter().copied().collect();
            if view.global {
                self.selected_paths = view.selected.iter().cloned().collect();
                self.global
                    .submit(locations.snapshots(), view.query.clone());
            } else {
                self.local.submit(
                    source.scan,
                    view.query.clone(),
                    if view.whole_scan { 0 } else { resolved.folder },
                );
            }
            self.resolved = Some(resolved);
            self.key = Some(key);
        }
        self.local.poll();
        self.global.poll();
    }

    pub fn folder(&self, whole_scan: bool) -> Option<&Path> {
        let view = self.resolved.as_ref()?;
        Some(&view.source.scan.folders[if whole_scan { 0 } else { view.folder }].path)
    }

    pub fn running(&self) -> bool {
        self.global.running || matches!(self.local.status(), SearchStatus::Running)
    }

    pub fn error(&self) -> Option<&str> {
        self.global
            .error
            .as_deref()
            .or_else(|| match self.local.status() {
                SearchStatus::Failed(message) => Some(message.as_str()),
                _ => None,
            })
    }

    pub fn len(&self) -> usize {
        if let Some(results) = &self.global.results {
            results.hits.len()
        } else {
            self.local
                .results()
                .map_or(0, |results| results.items.len())
        }
    }

    pub fn row(&self, index: usize) -> Option<(&Snapshot, Item, bool)> {
        if let Some(results) = &self.global.results {
            let hit = results.hits.get(index)?;
            let source = &results.sources[hit.source];
            let selected = !self.selected_paths.is_empty()
                && self
                    .selected_paths
                    .contains(&crate::search::item_path(&source.scan, hit.item));
            Some((source, hit.item, selected))
        } else {
            let source = &self.resolved.as_ref()?.source;
            let &item = self.local.results()?.items.get(index)?;
            Some((source, item, self.selected.contains(&item)))
        }
    }
}
