use crate::analysis_panel::PanelKey;
use crate::explorer::Item;
use crate::file_open::{OpenAction, OpenRequest};
use crate::locations::Snapshot;
use crate::treemap::{collect, MapData, MapScope};
use std::io;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    mpsc, Arc,
};
pub type Target = (u64, u64, Item);
#[derive(Clone, Copy)]
pub struct Selection {
    pub target: Target,
    pub row: Option<usize>,
}
impl Selection {
    pub fn apply_local(
        &self,
        source: &Snapshot,
        explorer: &mut crate::explorer::Explorer,
        search: &mut crate::search_view::SearchView,
    ) -> bool {
        if (source.id, source.generation) != (self.target.0, self.target.1) {
            return false;
        }
        explorer.reveal(&source.scan, self.target.2);
        search.selection.clear();
        search.reveal_row = self.row;
        if let Some(row) = self.row {
            search.selection.click(row, false, false);
        }
        true
    }
}
pub struct Output {
    pub sources: Vec<Snapshot>,
    pub data: MapData,
}
struct Request {
    id: u64,
    sources: Vec<Snapshot>,
    scope: MapScope,
}
#[derive(Default)]
pub struct MapController {
    pub revision: u64,
    pub running: bool,
    pub error: Option<String>,
    pub output: Option<Output>,
    pub selected: Option<Selection>,
    key: Option<PanelKey>,
    latest: Arc<AtomicU64>,
    active: Option<mpsc::Receiver<(u64, io::Result<Output>)>>,
    pending: Option<Request>,
}
impl MapController {
    pub fn needs(&self, key: &PanelKey) -> bool {
        self.key.as_ref() != Some(key)
    }
    pub fn clear(&mut self) {
        self.latest.fetch_add(1, Ordering::Relaxed);
        self.revision += 1;
        self.key = None;
        self.pending = None;
        self.output = None;
        self.selected = None;
        self.error = None;
        self.running = false;
    }
    pub fn submit(&mut self, key: PanelKey, sources: Vec<Snapshot>, scope: MapScope) {
        let id = self.latest.fetch_add(1, Ordering::Relaxed) + 1;
        self.revision += 1;
        self.key = Some(key);
        self.output = None;
        self.selected = None;
        self.error = None;
        self.running = true;
        self.pending = Some(Request { id, sources, scope });
        self.launch();
    }
    fn launch(&mut self) {
        if self.active.is_some() {
            return;
        }
        let Some(request) = self.pending.take() else {
            return;
        };
        let latest = Arc::clone(&self.latest);
        let (sender, receiver) = mpsc::channel();
        std::thread::spawn(move || {
            let result = collect(&request.sources, &request.scope, || {
                latest.load(Ordering::Relaxed) != request.id
            });
            let _ = sender.send((
                request.id,
                result.map(|data| Output {
                    sources: request.sources,
                    data,
                }),
            ));
        });
        self.active = Some(receiver);
    }
    pub fn poll(&mut self) {
        let Some(receiver) = &self.active else {
            return;
        };
        let response = match receiver.try_recv() {
            Ok(r) => Some(r),
            Err(mpsc::TryRecvError::Empty) => return,
            Err(mpsc::TryRecvError::Disconnected) => None,
        };
        self.active = None;
        match response {
            Some((id, result)) if id == self.latest.load(Ordering::Relaxed) => {
                self.running = false;
                match result {
                    Ok(output) => self.output = Some(output),
                    Err(error) => self.error = Some(error.to_string()),
                }
            }
            None if self.pending.is_none() && self.running => {
                self.running = false;
                self.error = Some("지도 작업이 예기치 않게 종료됐습니다".into());
            }
            _ => {}
        }
        self.launch();
    }
    pub fn route(&self, row: usize) -> Option<Target> {
        let output = self.output.as_ref()?;
        let hit = output.data.tiles.get(row)?.target?;
        let source = &output.sources[hit.source];
        Some((source.id, source.generation, hit.item))
    }
    pub fn select(
        &mut self,
        row: usize,
        mut visible: impl Iterator<Item = Target>,
    ) -> Option<Selection> {
        let target = self.route(row)?;
        let selection = Selection {
            target,
            row: visible.position(|t| t == target),
        };
        self.selected = Some(selection);
        Some(selection)
    }
    pub fn open(&self, row: usize, action: OpenAction) -> Option<OpenRequest> {
        let output = self.output.as_ref()?;
        let hit = output.data.tiles.get(row)?.target?;
        Some(OpenRequest::for_item(
            &output.sources[hit.source].scan,
            hit.item,
            action,
        ))
    }
}
impl Drop for MapController {
    fn drop(&mut self) {
        self.latest.fetch_add(1, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::global_search::Hit;
    use std::time::{Duration, Instant};
    #[test]
    fn map_selection_reveals_tree_and_list_while_preserving_query_and_folder() {
        let root =
            std::env::temp_dir().join(format!("diskweave-map-selection-{}", std::process::id()));
        std::fs::create_dir_all(root.join("child")).unwrap();
        std::fs::write(root.join("child/deep.bin"), [0; 5]).unwrap();
        let source = Snapshot {
            id: 7,
            generation: 2,
            scan: Arc::new(crate::scanner::scan(&root, || false).unwrap()),
        };
        let mut explorer = crate::explorer::Explorer::new(&source.scan);
        let mut search = crate::search_view::SearchView::default();
        search.input.text = "deep".into();
        let selection = Selection {
            target: (7, 2, Item::File(0)),
            row: Some(0),
        };
        assert!(selection.apply_local(&source, &mut explorer, &mut search));
        assert_eq!(explorer.folder(), 0);
        assert_eq!(search.input.text, "deep");
        assert_eq!(explorer.selected(), Item::File(0));
        assert_eq!(
            search.selection.single(&[Item::File(0)]),
            Some(Item::File(0))
        );
        let outside = Selection {
            target: (8, 2, Item::File(0)),
            row: None,
        };
        assert!(!outside.apply_local(&source, &mut explorer, &mut search));
        let hidden = Selection {
            target: (7, 2, Item::File(0)),
            row: None,
        };
        assert!(hidden.apply_local(&source, &mut explorer, &mut search));
        assert_eq!(search.selection.count(), 0);
        assert_eq!(search.input.text, "deep");
        std::fs::remove_dir_all(root).unwrap();
    }
    fn wait(map: &mut MapController) {
        let end = Instant::now() + Duration::from_secs(5);
        while map.running && Instant::now() < end {
            map.poll();
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(!map.running);
        assert!(map.error.is_none(), "{:?}", map.error);
    }
    #[test]
    fn latest_map_keeps_source_identity_and_selection_without_changing_search_scope() {
        let root = std::env::temp_dir().join(format!("diskweave-map-job-{}", std::process::id()));
        std::fs::create_dir_all(root.join("a")).unwrap();
        std::fs::create_dir_all(root.join("b")).unwrap();
        std::fs::write(root.join("a/same.bin"), [0; 3]).unwrap();
        std::fs::write(root.join("b/same.bin"), [0; 9]).unwrap();
        let sources: Vec<_> = ["a", "b"]
            .iter()
            .enumerate()
            .map(|(i, p)| Snapshot {
                id: i as u64 + 10,
                generation: 1,
                scan: Arc::new(crate::scanner::scan(&root.join(p), || false).unwrap()),
            })
            .collect();
        let key = PanelKey {
            versions: vec![(10, 1), (11, 1)],
            folder: None,
            query: Some(crate::search::SearchInput::default()),
        };
        let hits = vec![
            Hit {
                source: 0,
                item: Item::File(0),
            },
            Hit {
                source: 1,
                item: Item::File(0),
            },
        ];
        let mut map = MapController::default();
        map.submit(key.clone(), sources.clone(), MapScope::Matches(hits));
        wait(&mut map);
        assert!(!map.needs(&key));
        let visible = vec![(10, 1, Item::File(0)), (11, 1, Item::File(0))];
        let selection = map.select(0, visible.into_iter()).unwrap();
        assert_eq!(
            (selection.target, selection.row),
            ((11, 1, Item::File(0)), Some(1))
        );
        assert_eq!(
            map.open(0, crate::file_open::OpenAction::RevealFile)
                .unwrap()
                .path(),
            root.join("b/same.bin")
        );
        let selection = map.select(0, std::iter::empty()).unwrap();
        assert_eq!(selection.row, None);
        let old = key.clone();
        map.submit(old, sources.clone(), MapScope::Locations);
        let next = PanelKey {
            versions: vec![(10, 2)],
            folder: Some(0),
            query: None,
        };
        let mut fresh = sources[0].clone();
        fresh.generation = 2;
        std::fs::write(root.join("a/same.bin"), [0; 7]).unwrap();
        fresh.scan = Arc::new(crate::scanner::scan(&root.join("a"), || false).unwrap());
        map.submit(next, vec![fresh], MapScope::Folder(0));
        assert!(map.output.is_none());
        assert!(map.selected.is_none());
        wait(&mut map);
        assert_eq!(map.route(0), Some((10, 2, Item::File(0))));
        assert_eq!(map.output.as_ref().unwrap().data.total, 7);
        map.clear();
        assert!(map.output.is_none());
        assert!(map.route(0).is_none());
        std::fs::remove_dir_all(root).unwrap();
    }
}
