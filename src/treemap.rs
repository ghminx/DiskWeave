use crate::explorer::Item;
use crate::global_search::Hit;
use crate::locations::Snapshot;
use std::cmp::Reverse;
use std::collections::{BTreeMap, BinaryHeap, HashSet};
use std::io;

pub enum MapScope {
    Folder(usize),
    Locations,
    Matches(Vec<Hit>),
}
pub struct Tile {
    pub target: Option<Hit>,
    pub bytes: u64,
    pub count: usize,
}
type Rank = (u64, Reverse<(usize, u8, usize)>);

fn rank(hit: Hit, bytes: u64) -> Rank {
    let (kind, row) = match hit.item {
        Item::Folder(row) => (0, row),
        Item::File(row) => (1, row),
    };
    (bytes, Reverse((hit.source, kind, row)))
}

fn unrank((bytes, Reverse((source, kind, row))): Rank) -> (u64, Hit) {
    (
        bytes,
        Hit {
            source,
            item: if kind == 0 {
                Item::Folder(row)
            } else {
                Item::File(row)
            },
        },
    )
}

type LocalRank = (u64, Reverse<(bool, std::path::PathBuf)>);

fn local_rank(scan: &crate::scanner::ScanResult, item: Item, bytes: u64) -> LocalRank {
    (
        bytes,
        Reverse((
            matches!(item, Item::File(_)),
            crate::search::item_path(scan, item),
        )),
    )
}

fn display_tiles(entries: impl Iterator<Item = (u64, Hit)>, count: usize, total: u64) -> Vec<Tile> {
    let mut tiles: Vec<_> = entries
        .take(if count > 2000 { 1999 } else { 2000 })
        .map(|(bytes, hit)| Tile {
            target: Some(hit),
            bytes,
            count: 1,
        })
        .collect();
    if count > 2000 {
        tiles.push(Tile {
            target: None,
            bytes: total - tiles.iter().map(|tile| tile.bytes).sum::<u64>(),
            count: count - tiles.len(),
        });
    }
    tiles
}
pub struct MapData {
    pub total: u64,
    pub zero_count: usize,
    pub tiles: Vec<Tile>,
    ranking: Option<BTreeMap<LocalRank, Item>>,
}
impl MapData {
    pub(crate) fn patch_local(
        &mut self,
        previous: &Snapshot,
        current: &Snapshot,
        folder: usize,
    ) -> bool {
        let Some(delta) = current.scan.changes_from(&previous.scan) else {
            return false;
        };
        let Some(Item::Folder(new_folder)) = delta.remap(Item::Folder(folder)) else {
            return false;
        };
        if (delta.file_rows.is_some() || delta.folder_rows.is_some()) && current.entries.is_none() {
            return false;
        }
        let Some(ranking) = &mut self.ranking else {
            return false;
        };
        let in_folder = |scan: &crate::scanner::ScanResult, item, folder| {
            (match item {
                Item::Folder(row) => scan.folders[row].parent,
                Item::File(row) => Some(scan.files[row].parent),
            }) == Some(folder)
        };
        for &item in &delta.before {
            if !in_folder(&previous.scan, item, folder) {
                continue;
            }
            let old = crate::search::item_bytes(&previous.scan, item);
            if old == 0 {
                self.zero_count -= 1;
            } else {
                ranking.remove(&local_rank(&previous.scan, item, old));
            }
        }
        for &item in &delta.after {
            if !in_folder(&current.scan, item, new_folder) {
                continue;
            }
            let new = crate::search::item_bytes(&current.scan, item);
            if new == 0 {
                self.zero_count += 1;
            } else {
                ranking.insert(local_rank(&current.scan, item, new), item);
            }
        }
        self.total = current.scan.folders[new_folder].logical_bytes;
        let entries = ranking
            .iter()
            .rev()
            .map(|((bytes, Reverse((_, path))), &fallback)| {
                let item = if current.entries.is_some() {
                    current.item_at_path(path).expect("현재 지도 항목 경로")
                } else {
                    fallback
                };
                (*bytes, Hit { source: 0, item })
            });
        self.tiles = display_tiles(entries, ranking.len(), self.total);
        true
    }
}
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

pub fn collect(
    sources: &[Snapshot],
    scope: &MapScope,
    cancelled: impl Fn() -> bool,
) -> io::Result<MapData> {
    let mut heap = BinaryHeap::new();
    let mut ranking = matches!(scope, MapScope::Folder(_)).then(BTreeMap::new);
    let mut zero_count = 0;
    let mut total = 0u64;
    let mut count = 0usize;
    let mut add = |hit: Hit| -> io::Result<()> {
        if cancelled() {
            return Err(io::Error::new(io::ErrorKind::Interrupted, "지도 구성 취소"));
        }
        let bytes = crate::search::item_bytes(&sources[hit.source].scan, hit.item);
        if bytes == 0 {
            zero_count += 1;
            return Ok(());
        }
        total = total
            .checked_add(bytes)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "지도 크기 합계 초과"))?;
        count += 1;
        let value = rank(hit, bytes);
        if let Some(ranking) = &mut ranking {
            ranking.insert(
                local_rank(&sources[hit.source].scan, hit.item, bytes),
                hit.item,
            );
        } else {
            heap.push(Reverse(value));
            if heap.len() > 2000 {
                heap.pop();
            }
        }
        Ok(())
    };
    match scope {
        MapScope::Folder(folder) => {
            if sources.len() != 1 || *folder >= sources[0].scan.folders.len() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "지도 폴더 범위 오류",
                ));
            }
            for &id in sources[0].scan.children[*folder].iter() {
                add(Hit {
                    source: 0,
                    item: Item::Folder(id),
                })?;
            }
            for &id in sources[0].scan.file_children[*folder].iter() {
                add(Hit {
                    source: 0,
                    item: Item::File(id),
                })?;
            }
        }
        MapScope::Locations => {
            for source in 0..sources.len() {
                add(Hit {
                    source,
                    item: Item::Folder(0),
                })?;
            }
        }
        MapScope::Matches(hits) => {
            let mut seen = HashSet::new();
            for &hit in hits {
                if cancelled() {
                    return Err(io::Error::new(io::ErrorKind::Interrupted, "지도 구성 취소"));
                }
                if let Item::File(id) = hit.item {
                    if hit.source >= sources.len() || id >= sources[hit.source].scan.files.len() {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidInput,
                            "지도 파일 출처 오류",
                        ));
                    }
                    if seen.insert((hit.source, id)) {
                        add(hit)?;
                    }
                }
            }
        }
    }
    if cancelled() {
        return Err(io::Error::new(io::ErrorKind::Interrupted, "지도 구성 취소"));
    }
    let tiles = if let Some(ranking) = &ranking {
        display_tiles(
            ranking
                .iter()
                .rev()
                .map(|((bytes, _), &item)| (*bytes, Hit { source: 0, item })),
            count,
            total,
        )
    } else {
        let mut entries: Vec<_> = heap.into_iter().map(|Reverse(entry)| entry).collect();
        entries.sort_unstable_by(|a, b| b.cmp(a));
        display_tiles(entries.into_iter().map(unrank), count, total)
    };
    Ok(MapData {
        total,
        zero_count,
        tiles,
        ranking,
    })
}

pub fn layout(tiles: &[Tile], width: f64, height: f64) -> io::Result<Vec<Rect>> {
    if !width.is_finite() || !height.is_finite() || width < 0.0 || height < 0.0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "지도 영역 크기 오류",
        ));
    }
    let mut rects = Vec::with_capacity(tiles.len());
    fn divide(tiles: &[Tile], rect: Rect, out: &mut Vec<Rect>) {
        if tiles.is_empty() {
            return;
        }
        if tiles.len() == 1 {
            out.push(rect);
            return;
        }
        let total: f64 = tiles.iter().map(|t| t.bytes as f64).sum();
        let mut split = 1;
        let mut left = tiles[0].bytes as f64;
        while split < tiles.len() - 1 && left < total / 2.0 {
            left += tiles[split].bytes as f64;
            split += 1;
        }
        let ratio = if total == 0.0 {
            split as f64 / tiles.len() as f64
        } else {
            left / total
        };
        if rect.width >= rect.height {
            let part = rect.width * ratio;
            divide(
                &tiles[..split],
                Rect {
                    x: rect.x,
                    y: rect.y,
                    width: part,
                    height: rect.height,
                },
                out,
            );
            divide(
                &tiles[split..],
                Rect {
                    x: rect.x + part,
                    y: rect.y,
                    width: rect.width - part,
                    height: rect.height,
                },
                out,
            );
        } else {
            let part = rect.height * ratio;
            divide(
                &tiles[..split],
                Rect {
                    x: rect.x,
                    y: rect.y,
                    width: rect.width,
                    height: part,
                },
                out,
            );
            divide(
                &tiles[split..],
                Rect {
                    x: rect.x,
                    y: rect.y + part,
                    width: rect.width,
                    height: rect.height - part,
                },
                out,
            );
        }
    }
    divide(
        tiles,
        Rect {
            x: 0.0,
            y: 0.0,
            width,
            height,
        },
        &mut rects,
    );
    Ok(rects)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    #[test]
    fn empty_and_zero_only_folders_have_no_tiles_but_keep_zero_count() {
        let root = std::env::temp_dir().join(format!("diskweave-map-empty-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let mut source = Snapshot {
            entries: None,
            id: 1,
            generation: 1,
            scan: Arc::new(crate::scanner::scan(&root, || false).unwrap()),
        };
        let empty = collect(&[source.clone()], &MapScope::Folder(0), || false).unwrap();
        assert_eq!(
            (empty.total, empty.zero_count, empty.tiles.len()),
            (0, 0, 0)
        );
        std::fs::write(root.join("empty"), []).unwrap();
        source.scan = Arc::new(crate::scanner::scan(&root, || false).unwrap());
        let zero = collect(&[source], &MapScope::Folder(0), || false).unwrap();
        assert_eq!((zero.total, zero.zero_count, zero.tiles.len()), (0, 1, 0));
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn empty_zero_and_extreme_geometry_stays_finite_and_proportional() {
        assert!(layout(&[], 800.0, 200.0).unwrap().is_empty());
        assert!(layout(&[], f64::NAN, 20.0).is_err());
        let tiles = vec![
            Tile {
                target: None,
                bytes: 3,
                count: 1,
            },
            Tile {
                target: None,
                bytes: 1,
                count: 1,
            },
        ];
        for (width, height) in [(400.0, 100.0), (90.0, 300.0), (0.0, 20.0)] {
            let rects = layout(&tiles, width, height).unwrap();
            assert!((rects[0].width * rects[0].height - width * height * 0.75).abs() < 0.001);
            assert!((rects[1].width * rects[1].height - width * height * 0.25).abs() < 0.001);
        }
        let extreme = vec![
            Tile {
                target: None,
                bytes: u64::MAX - 1,
                count: 1,
            },
            Tile {
                target: None,
                bytes: 1,
                count: 1,
            },
        ];
        for rect in layout(&extreme, 800.0, 200.0).unwrap() {
            assert!(
                rect.x.is_finite() && rect.y.is_finite() && rect.width >= 0.0 && rect.height >= 0.0
            );
        }
    }
    #[test]
    fn matched_map_counts_only_unique_files_and_handles_missing_or_cancelled_inputs() {
        let root =
            std::env::temp_dir().join(format!("diskweave-map-matches-{}", std::process::id()));
        std::fs::create_dir_all(root.join("a")).unwrap();
        std::fs::create_dir_all(root.join("b")).unwrap();
        std::fs::write(root.join("a/shared.bin"), [0; 3]).unwrap();
        std::fs::write(root.join("b/shared.bin"), [0; 9]).unwrap();
        let sources: Vec<_> = ["a", "b"]
            .iter()
            .enumerate()
            .map(|(id, p)| Snapshot {
                entries: None,
                id: id as u64,
                generation: 1,
                scan: Arc::new(crate::scanner::scan(&root.join(p), || false).unwrap()),
            })
            .collect();
        let hit = Hit {
            source: 0,
            item: Item::File(0),
        };
        let map = collect(
            &sources,
            &MapScope::Matches(vec![
                hit,
                hit,
                Hit {
                    source: 1,
                    item: Item::File(0),
                },
                Hit {
                    source: 0,
                    item: Item::Folder(0),
                },
            ]),
            || false,
        )
        .unwrap();
        assert_eq!((map.total, map.tiles.len()), (12, 2));
        assert_eq!(map.tiles[0].target.unwrap().source, 1);
        assert_eq!(
            collect(&sources, &MapScope::Locations, || false)
                .unwrap()
                .total,
            12
        );
        assert!(collect(
            &sources,
            &MapScope::Matches(vec![Hit {
                source: 9,
                item: Item::File(0)
            }]),
            || false
        )
        .is_err());
        assert!(collect(&[], &MapScope::Locations, || true).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn tile_budget_keeps_the_entire_size_and_reports_grouped_entry_count() {
        let root =
            std::env::temp_dir().join(format!("diskweave-map-budget-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        for id in 0..2005 {
            std::fs::write(root.join(format!("{id:04}.bin")), [0]).unwrap();
        }
        let sources = vec![Snapshot {
            entries: None,
            id: 1,
            generation: 1,
            scan: Arc::new(crate::scanner::scan(&root, || false).unwrap()),
        }];
        let mut map = collect(&sources, &MapScope::Folder(0), || false).unwrap();
        assert_eq!((map.total, map.tiles.len()), (2005, 2000));
        let group = map.tiles.iter().find(|t| t.target.is_none()).unwrap();
        assert_eq!((group.bytes, group.count), (6, 6));
        assert_eq!(map.tiles.iter().map(|t| t.bytes).sum::<u64>(), 2005);
        let old = &sources[0];
        let mut scan = (*old.scan).clone();
        scan.files[0].logical_bytes = 0;
        scan.folders[0].logical_bytes -= 1;
        scan.partial = Some(Arc::new(crate::scanner::PartialUpdate {
            previous: Arc::downgrade(&old.scan),
            affected: [0].into_iter().collect(),
            search: Some(crate::scanner::SearchDelta {
                before: vec![Item::File(0)],
                after: vec![Item::File(0)],
                file_rows: None,
                folder_rows: None,
            }),
        }));
        let new = Snapshot {
            scan: Arc::new(scan),
            generation: 2,
            ..old.clone()
        };
        assert!(map.patch_local(old, &new, 0));
        let rebuilt = collect(&[new], &MapScope::Folder(0), || false).unwrap();
        let tiles = |map: &MapData| {
            map.tiles
                .iter()
                .map(|t| (t.target.map(|h| h.item), t.bytes, t.count))
                .collect::<Vec<_>>()
        };
        assert_eq!(tiles(&map), tiles(&rebuilt));
        assert_eq!(
            (map.total, map.zero_count),
            (rebuilt.total, rebuilt.zero_count)
        );
        let mut job = crate::map_job::MapController::default();
        job.submit(
            crate::analysis_panel::PanelKey {
                versions: vec![(1, 1)],
                folder: Some(0),
                query: None,
            },
            sources,
            MapScope::Folder(0),
        );
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while job.running && std::time::Instant::now() < deadline {
            job.poll();
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert!(!job.running);
        assert!(job.error.is_none());
        assert!(job.route(1999).is_none());
        assert!(job
            .open(1999, crate::file_open::OpenAction::OpenFile)
            .is_none());
        assert!(job.select(1999, std::iter::empty()).is_none());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn folder_map_preserves_direct_sizes_without_readding_descendant_files() {
        let root = std::env::temp_dir().join(format!("diskweave-map-area-{}", std::process::id()));
        std::fs::create_dir_all(root.join("child")).unwrap();
        std::fs::write(root.join("child/a.npy"), [0; 3]).unwrap();
        std::fs::write(root.join("child/b.npy"), [0; 9]).unwrap();
        std::fs::write(root.join("large.bin"), [0; 12]).unwrap();
        std::fs::write(root.join("empty"), []).unwrap();
        let sources = vec![crate::locations::Snapshot {
            entries: None,
            id: 4,
            generation: 1,
            scan: Arc::new(crate::scanner::scan(&root, || false).unwrap()),
        }];
        let map = collect(&sources, &MapScope::Folder(0), || false).unwrap();
        assert_eq!((map.total, map.zero_count, map.tiles.len()), (24, 1, 2));
        assert!(map
            .tiles
            .iter()
            .any(|t| matches!(t.target.unwrap().item, crate::explorer::Item::Folder(_))));
        let rects = layout(&map.tiles, 200.0, 120.0).unwrap();
        assert_eq!(rects.len(), 2);
        for rect in &rects {
            assert!((rect.width * rect.height - 12000.0).abs() < 0.001);
            assert!(
                rect.x >= 0.0
                    && rect.y >= 0.0
                    && rect.x + rect.width <= 200.0001
                    && rect.y + rect.height <= 120.0001
            );
        }
        let (a, b) = (&rects[0], &rects[1]);
        assert!(
            a.x + a.width <= b.x + 0.0001
                || b.x + b.width <= a.x + 0.0001
                || a.y + a.height <= b.y + 0.0001
                || b.y + b.height <= a.y + 0.0001
        );
        std::fs::remove_dir_all(root).unwrap();
    }
}
