use crate::explorer::Item;
use crate::global_search::Hit;
use crate::locations::Snapshot;
use crate::metadata::{Allocation, Field, Identity, Missing};
use std::cmp::Reverse;
use std::collections::{BTreeMap, BinaryHeap, HashMap, HashSet};
use std::io;

pub enum StatsScope {
    Subtree(usize),
    All,
    Matches(Vec<Hit>),
}
pub struct ExtensionTotal {
    pub name: String,
    pub bytes: u64,
    pub files: u64,
}
pub struct Statistics {
    pub allocation: Allocation,
    pub unique_allocation: Allocation,
    pub identity_conflicts: usize,
    pub bytes: u64,
    pub files: u64,
    pub extensions: Vec<ExtensionTotal>,
    top: Vec<Hit>,
    local: Option<LocalRanking>,
}
type RankKey = (u64, Reverse<std::path::PathBuf>);

#[derive(Default)]
struct LocalRanking {
    ranked: BTreeMap<RankKey, usize>,
    links: HashMap<Identity, LinkAllocations>,
}

#[derive(Default)]
struct LinkAllocations {
    first: Option<(Field<u64>, usize)>,
    additional: Vec<(Field<u64>, usize)>,
}
impl LinkAllocations {
    fn add(&mut self, field: Field<u64>) {
        if let Some((_, count)) = self
            .first
            .iter_mut()
            .chain(&mut self.additional)
            .find(|(value, _)| *value == field)
        {
            *count += 1;
        } else if self.first.is_none() {
            self.first = Some((field, 1));
        } else {
            self.additional.push((field, 1));
        }
    }
    fn remove(&mut self, field: Field<u64>) {
        if let Some((_, count)) = self.first.as_mut().filter(|(value, _)| *value == field) {
            *count -= 1;
            if *count == 0 {
                self.first = self.additional.pop();
            }
        } else if let Some(at) = self
            .additional
            .iter()
            .position(|(value, _)| *value == field)
        {
            self.additional[at].1 -= 1;
            if self.additional[at].1 == 0 {
                self.additional.swap_remove(at);
            }
        }
    }
    fn total(&self) -> Option<(Field<u64>, bool)> {
        let mut known = None;
        let mut missing = Missing::NotCollected;
        for (field, _) in self.first.iter().chain(&self.additional) {
            match field {
                Field::Value(value) => {
                    if known.replace(*value).is_some_and(|old| old != *value) {
                        return Some((Field::Missing(Missing::Failed), true));
                    }
                }
                Field::Missing(reason) => missing = stronger_missing(missing, *reason),
            }
        }
        self.first
            .map(|_| (known.map_or(Field::Missing(missing), Field::Value), false))
    }
}

fn stronger_missing(a: Missing, b: Missing) -> Missing {
    match (a, b) {
        (Missing::Failed, _) | (_, Missing::Failed) => Missing::Failed,
        (Missing::Unsupported, _) | (_, Missing::Unsupported) => Missing::Unsupported,
        _ => Missing::NotCollected,
    }
}

fn remove_allocation(total: &mut Allocation, value: Field<u64>) {
    match value {
        Field::Value(bytes) => {
            total.bytes -= bytes;
            total.known -= 1;
        }
        Field::Missing(Missing::Failed) => total.failed -= 1,
        Field::Missing(Missing::Unsupported) => total.unsupported -= 1,
        Field::Missing(Missing::NotCollected) => total.not_collected -= 1,
    }
}

impl Statistics {
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
        let Some(local) = &mut self.local else {
            return false;
        };
        let scope = |scan: &crate::scanner::ScanResult, item, folder: usize| matches!(item, Item::File(row) if scan.folders[scan.files[row].parent].path.starts_with(&scan.folders[folder].path));
        let old = delta
            .before
            .iter()
            .copied()
            .filter(|&item| scope(&previous.scan, item, folder));
        let new = delta
            .after
            .iter()
            .copied()
            .filter(|&item| scope(&current.scan, item, new_folder));
        let mut identities = HashSet::new();
        for (scan, items) in [
            (&previous.scan, old.clone().collect::<Vec<_>>()),
            (&current.scan, new.clone().collect()),
        ] {
            for item in items {
                let Item::File(row) = item else {
                    unreachable!()
                };
                if let Field::Value(id) = scan.files[row].extra.identity {
                    identities.insert(id);
                }
            }
        }
        for id in &identities {
            if let Some((allocation, conflict)) =
                local.links.get(id).and_then(LinkAllocations::total)
            {
                remove_allocation(&mut self.unique_allocation, allocation);
                self.identity_conflicts -= usize::from(conflict);
            }
        }
        let mut groups: HashMap<_, _> = self
            .extensions
            .iter()
            .map(|g| (g.name.clone(), (g.bytes, g.files)))
            .collect();
        for (add, scan, items) in [
            (false, &previous.scan, old.collect::<Vec<_>>()),
            (true, &current.scan, new.collect()),
        ] {
            for item in items {
                let Item::File(row) = item else {
                    unreachable!()
                };
                let file = &scan.files[row];
                let key = (
                    file.logical_bytes,
                    Reverse(crate::search::item_path(scan, item)),
                );
                let group = groups.entry(extension(&file.name)).or_default();
                if add {
                    local.ranked.insert(key, row);
                    group.0 += file.logical_bytes;
                    group.1 += 1;
                    if let Field::Missing(reason) = file.extra.identity {
                        self.unique_allocation
                            .add(Field::Missing(reason))
                            .expect("파일 개수 범위");
                    } else if let Field::Value(id) = file.extra.identity {
                        local
                            .links
                            .entry(id)
                            .or_default()
                            .add(file.extra.allocation);
                    }
                } else {
                    local.ranked.remove(&key);
                    group.0 -= file.logical_bytes;
                    group.1 -= 1;
                    match file.extra.identity {
                        Field::Missing(reason) => {
                            remove_allocation(&mut self.unique_allocation, Field::Missing(reason))
                        }
                        Field::Value(id) => local
                            .links
                            .get_mut(&id)
                            .unwrap()
                            .remove(file.extra.allocation),
                    }
                }
            }
        }
        for id in identities {
            if let Some((allocation, conflict)) =
                local.links.get(&id).and_then(LinkAllocations::total)
            {
                self.unique_allocation
                    .add(allocation)
                    .expect("폴더 할당량 범위");
                self.identity_conflicts += usize::from(conflict);
            } else {
                local.links.remove(&id);
            }
        }
        let root = &current.scan.folders[new_folder];
        self.bytes = root.logical_bytes;
        self.files = root.file_count;
        self.allocation = root.allocation;
        self.extensions = sorted_extensions(groups);
        self.top = local
            .ranked
            .iter()
            .rev()
            .take(100)
            .map(|((_, Reverse(path)), &row)| {
                let item = if current.entries.is_some() {
                    current.item_at_path(path).expect("현재 통계 항목 경로")
                } else {
                    Item::File(row)
                };
                Hit { source: 0, item }
            })
            .collect();
        true
    }

    pub fn top(&self, count: usize) -> &[Hit] {
        &self.top[..count.min(self.top.len())]
    }
}

pub fn extension(name: &std::ffi::OsStr) -> String {
    let name = name.to_string_lossy().to_lowercase();
    for compound in [".nii.gz", ".tar.gz"] {
        if name.ends_with(compound) {
            return compound.into();
        }
    }
    match name.rsplit_once('.') {
        Some((base, suffix)) if !base.is_empty() && !suffix.is_empty() => format!(".{suffix}"),
        _ => "확장자 없음".into(),
    }
}

fn sorted_extensions(groups: HashMap<String, (u64, u64)>) -> Vec<ExtensionTotal> {
    let mut extensions: Vec<_> = groups
        .into_iter()
        .filter(|(_, (_, files))| *files > 0)
        .map(|(name, (bytes, files))| ExtensionTotal { name, bytes, files })
        .collect();
    extensions.sort_unstable_by(|a, b| b.bytes.cmp(&a.bytes).then_with(|| a.name.cmp(&b.name)));
    extensions
}

pub fn compute(
    sources: &[Snapshot],
    scope: &StatsScope,
    cancelled: impl Fn() -> bool,
) -> io::Result<Statistics> {
    if cancelled() {
        return Err(io::Error::new(io::ErrorKind::Interrupted, "통계 취소"));
    }
    let mut bytes = 0u64;
    let mut files = 0u64;
    let mut groups: HashMap<String, (u64, u64)> = HashMap::new();
    let mut heap = BinaryHeap::new();
    let mut allocation = Allocation::default();
    let mut unique_allocation = Allocation::default();
    let mut unique: HashMap<Identity, LinkAllocations> = HashMap::new();
    let mut local = matches!(scope, StatsScope::Subtree(_)).then(LocalRanking::default);
    let mut add = |source: usize, id: usize| -> io::Result<()> {
        if cancelled() {
            return Err(io::Error::new(io::ErrorKind::Interrupted, "통계 취소"));
        }
        let file = &sources[source].scan.files[id];
        allocation.add(file.extra.allocation)?;
        match file.extra.identity {
            Field::Value(identity) => {
                unique
                    .entry(identity)
                    .or_default()
                    .add(file.extra.allocation);
            }
            Field::Missing(reason) => unique_allocation.add(Field::Missing(reason))?,
        }
        bytes = bytes.checked_add(file.logical_bytes).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "파일 용량 합계가 표시 범위를 초과했습니다",
            )
        })?;
        files += 1;
        let group = groups.entry(extension(&file.name)).or_default();
        group.0 += file.logical_bytes;
        group.1 += 1;
        let path = sources[source].scan.folders[file.parent]
            .path
            .join(&file.name);
        if let Some(local) = &mut local {
            local.ranked.insert((file.logical_bytes, Reverse(path)), id);
        } else {
            heap.push(Reverse((file.logical_bytes, Reverse(path), source, id)));
            if heap.len() > 100 {
                heap.pop();
            }
        }
        Ok(())
    };
    match scope {
        StatsScope::Subtree(root) => {
            if sources.len() != 1 || *root >= sources[0].scan.folders.len() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "통계 폴더 범위가 없습니다",
                ));
            }
            let scan = &sources[0].scan;
            let mut pending = vec![*root];
            while let Some(folder) = pending.pop() {
                if cancelled() {
                    return Err(io::Error::new(io::ErrorKind::Interrupted, "통계 취소"));
                }
                for &id in scan.file_children[folder].iter() {
                    add(0, id)?;
                }
                pending.extend(scan.children[folder].iter().copied());
            }
        }
        StatsScope::All => {
            for (source, snapshot) in sources.iter().enumerate() {
                for id in 0..snapshot.scan.files.len() {
                    add(source, id)?;
                }
            }
        }
        StatsScope::Matches(hits) => {
            let mut seen = HashSet::new();
            for hit in hits {
                if cancelled() {
                    return Err(io::Error::new(io::ErrorKind::Interrupted, "통계 취소"));
                }
                if let Item::File(id) = hit.item {
                    if hit.source >= sources.len() || id >= sources[hit.source].scan.files.len() {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidInput,
                            "통계 항목의 출처가 유효하지 않습니다",
                        ));
                    }
                    if seen.insert((hit.source, id)) {
                        add(hit.source, id)?;
                    }
                }
            }
        }
    }
    let mut identity_conflicts = 0;
    for (value, conflict) in unique.values().filter_map(LinkAllocations::total) {
        if cancelled() {
            return Err(io::Error::new(io::ErrorKind::Interrupted, "통계 취소"));
        }
        if conflict {
            identity_conflicts += 1;
        }
        unique_allocation.add(value)?;
    }
    let extensions = sorted_extensions(groups);
    let mut top: Vec<_> = heap
        .into_iter()
        .map(|Reverse((_, _, source, id))| Hit {
            source,
            item: Item::File(id),
        })
        .collect();
    top.sort_unstable_by(|a, b| {
        crate::search::item_bytes(&sources[b.source].scan, b.item)
            .cmp(&crate::search::item_bytes(&sources[a.source].scan, a.item))
            .then_with(|| {
                crate::search::item_path(&sources[a.source].scan, a.item)
                    .cmp(&crate::search::item_path(&sources[b.source].scan, b.item))
            })
    });
    if let Some(local) = &mut local {
        top = local
            .ranked
            .values()
            .rev()
            .take(100)
            .map(|&row| Hit {
                source: 0,
                item: Item::File(row),
            })
            .collect();
        local.links = unique;
    }
    Ok(Statistics {
        allocation,
        unique_allocation,
        identity_conflicts,
        bytes,
        files,
        extensions,
        top,
        local,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    #[test]
    fn moved_subtree_reconnects_search_statistics_and_map_in_shifted_scope() {
        use crate::search::{refresh_matches, search, SearchInput};
        use crate::treemap::{collect, MapScope};
        let (old, new) = crate::usn::stream::tests::folder_move_snapshots();
        let old_folder = old.scan.folder_index(std::path::Path::new("z")).unwrap();
        let new_folder = new.scan.folder_index(std::path::Path::new("z")).unwrap();
        assert!(new.scan.folders.shares_row_with(&old.scan.folders, 256));
        assert_ne!(old_folder, new_folder);
        let input = SearchInput {
            text: ".bin".into(),
            ..Default::default()
        };
        let old_hits = search(&old.scan, &input.compile(old_folder).unwrap(), || false).unwrap();
        let query = input.compile(new_folder).unwrap();
        let matches = refresh_matches(&old.scan, &new.scan, &query, old_hits, || false)
            .unwrap()
            .unwrap();
        assert_eq!(
            matches.items,
            search(&new.scan, &query, || false).unwrap().items
        );
        let mut old_stats = compute(
            std::slice::from_ref(&old),
            &StatsScope::Subtree(old_folder),
            || false,
        )
        .unwrap();
        assert!(old_stats.patch_local(&old, &new, old_folder));
        let new_stats = compute(
            std::slice::from_ref(&new),
            &StatsScope::Subtree(new_folder),
            || false,
        )
        .unwrap();
        assert_eq!(
            (
                old_stats.bytes,
                old_stats.files,
                old_stats.unique_allocation
            ),
            (
                new_stats.bytes,
                new_stats.files,
                new_stats.unique_allocation
            )
        );
        assert_eq!(
            old_stats
                .top(100)
                .iter()
                .map(|h| h.item)
                .collect::<Vec<_>>(),
            new_stats
                .top(100)
                .iter()
                .map(|h| h.item)
                .collect::<Vec<_>>()
        );
        let mut old_map = collect(
            std::slice::from_ref(&old),
            &MapScope::Folder(old_folder),
            || false,
        )
        .unwrap();
        assert!(old_map.patch_local(&old, &new, old_folder));
        let new_map = collect(
            std::slice::from_ref(&new),
            &MapScope::Folder(new_folder),
            || false,
        )
        .unwrap();
        let tiles = |map: &crate::treemap::MapData| {
            map.tiles
                .iter()
                .map(|t| (t.target.map(|h| h.item), t.bytes, t.count))
                .collect::<Vec<_>>()
        };
        assert_eq!(tiles(&old_map), tiles(&new_map));
    }
    #[test]
    fn local_delta_replaces_a_ranked_file_and_updates_hard_link_allocation() {
        let fixture = crate::duplicates::tests::Fixture::new();
        for n in 0..101 {
            std::fs::write(fixture.0.join(format!("{n:03}")), vec![0; n + 1]).unwrap();
        }
        std::fs::hard_link(fixture.0.join("100"), fixture.0.join("alias")).unwrap();
        let old = Snapshot {
            id: 1,
            generation: 1,
            entries: None,
            scan: Arc::new(
                crate::scanner::scan_unified(
                    &fixture.0,
                    Default::default(),
                    || false,
                    |_| {},
                    None,
                )
                .unwrap(),
            ),
        };
        let mut stats = compute(std::slice::from_ref(&old), &StatsScope::Subtree(0), || {
            false
        })
        .unwrap();
        let mut result = (*old.scan).clone();
        let row = result
            .files
            .iter()
            .position(|file| file.name == "100")
            .unwrap();
        let file = &mut result.files[row];
        result.folders[0].logical_bytes -= file.logical_bytes;
        file.logical_bytes = 0;
        let allocation = file.extra.allocation;
        file.extra.allocation = Field::Value(0);
        remove_allocation(&mut result.folders[0].allocation, allocation);
        result.folders[0].allocation.add(Field::Value(0)).unwrap();
        let item = Item::File(row);
        result.partial = Some(Arc::new(crate::scanner::PartialUpdate {
            previous: Arc::downgrade(&old.scan),
            affected: [0].into_iter().collect(),
            search: Some(crate::scanner::SearchDelta {
                before: vec![item],
                after: vec![item],
                file_rows: None,
                folder_rows: None,
            }),
        }));
        let new = Snapshot {
            scan: Arc::new(result),
            generation: 2,
            ..old.clone()
        };
        assert!(stats.patch_local(&old, &new, 0));
        let rebuilt = compute(std::slice::from_ref(&new), &StatsScope::Subtree(0), || {
            false
        })
        .unwrap();
        assert_eq!((stats.bytes, stats.files), (rebuilt.bytes, rebuilt.files));
        assert_eq!(stats.allocation, rebuilt.allocation);
        assert_eq!(stats.unique_allocation, rebuilt.unique_allocation);
        assert_eq!(stats.identity_conflicts, rebuilt.identity_conflicts);
        assert_eq!(
            stats.top(100).iter().map(|h| h.item).collect::<Vec<_>>(),
            rebuilt.top(100).iter().map(|h| h.item).collect::<Vec<_>>()
        );
    }
    #[test]
    fn allocation_conflicts_and_missing_ids_do_not_become_complete_unique_totals() {
        let root = std::env::temp_dir().join(format!(
            "diskweave-allocation-conflict-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("data"), [0; 3]).unwrap();
        std::fs::hard_link(root.join("data"), root.join("alias")).unwrap();
        let scan = crate::scanner::scan_with_probe(
            &root,
            true,
            || false,
            |_| {},
            |path| {
                let mut report = crate::metadata::read(path);
                if path.file_name().unwrap() == "alias" {
                    report.values.allocation =
                        Field::Value(report.values.allocation.value().unwrap() + 4096);
                }
                report
            },
        )
        .unwrap();
        let sources = vec![crate::locations::Snapshot {
            entries: None,
            id: 1,
            generation: 1,
            scan: Arc::new(scan),
        }];
        let stats = compute(&sources, &StatsScope::All, || false).unwrap();
        assert_eq!(
            (
                stats.identity_conflicts,
                stats.unique_allocation.bytes,
                stats.unique_allocation.failed
            ),
            (1, 0, 1)
        );
        assert_eq!(stats.bytes, 6);
        let basic = vec![crate::locations::Snapshot {
            entries: None,
            id: 1,
            generation: 1,
            scan: Arc::new(crate::scanner::scan(&root, || false).unwrap()),
        }];
        let stats = compute(&basic, &StatsScope::All, || false).unwrap();
        assert_eq!(
            (
                stats.allocation.not_collected,
                stats.unique_allocation.not_collected
            ),
            (2, 2)
        );
        let stats = compute(
            &sources,
            &StatsScope::Matches(vec![
                Hit {
                    source: 0,
                    item: Item::File(0),
                },
                Hit {
                    source: 0,
                    item: Item::File(0),
                },
            ]),
            || false,
        )
        .unwrap();
        assert_eq!((stats.files, stats.unique_allocation.known), (1, 1));
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn allocation_totals_distinguish_paths_unique_objects_and_nonadditive_child_scopes() {
        let root =
            std::env::temp_dir().join(format!("diskweave-allocation-stats-{}", std::process::id()));
        std::fs::create_dir_all(root.join("a")).unwrap();
        std::fs::create_dir_all(root.join("b")).unwrap();
        std::fs::write(root.join("a/data.bin"), vec![3; 8193]).unwrap();
        std::fs::hard_link(root.join("a/data.bin"), root.join("b/alias.bin")).unwrap();
        let scan =
            Arc::new(crate::scanner::scan_with_options(&root, true, || false, |_| {}).unwrap());
        let allocation = scan.files[0].extra.allocation.value().unwrap();
        let source = crate::locations::Snapshot {
            entries: None,
            id: 1,
            generation: 1,
            scan,
        };
        let all = compute(std::slice::from_ref(&source), &StatsScope::All, || false).unwrap();
        assert_eq!(all.bytes, 16386);
        assert_eq!(all.allocation.bytes, allocation * 2);
        assert_eq!(
            (
                all.unique_allocation.bytes,
                all.unique_allocation.known,
                all.unique_allocation.missing()
            ),
            (allocation, 1, 0)
        );
        let a = source.scan.folder_index(std::path::Path::new("a")).unwrap();
        let b = source.scan.folder_index(std::path::Path::new("b")).unwrap();
        let first = compute(
            std::slice::from_ref(&source),
            &StatsScope::Subtree(a),
            || false,
        )
        .unwrap();
        let second = compute(
            std::slice::from_ref(&source),
            &StatsScope::Subtree(b),
            || false,
        )
        .unwrap();
        assert_eq!(
            first.unique_allocation.bytes + second.unique_allocation.bytes,
            allocation * 2
        );
        let sources: Vec<_> = ["a", "b"]
            .iter()
            .enumerate()
            .map(|(i, p)| crate::locations::Snapshot {
                entries: None,
                id: i as u64 + 10,
                generation: 1,
                scan: Arc::new(
                    crate::scanner::scan_with_options(&root.join(p), true, || false, |_| {})
                        .unwrap(),
                ),
            })
            .collect();
        let cross = compute(&sources, &StatsScope::All, || false).unwrap();
        assert_eq!(
            (cross.allocation.bytes, cross.unique_allocation.bytes),
            (allocation * 2, allocation)
        );
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn cancelling_folder_only_matches_stops_without_a_file_to_aggregate() {
        let calls = std::cell::Cell::new(0);
        let hits = vec![
            Hit {
                source: 0,
                item: Item::Folder(0)
            };
            100
        ];
        let result = compute(&[], &StatsScope::Matches(hits), || {
            calls.set(calls.get() + 1);
            calls.get() > 1
        });
        assert_eq!(
            result.err().expect("집계 중 취소").kind(),
            io::ErrorKind::Interrupted
        );
    }
    #[test]
    fn top_limits_are_sorted_and_empty_or_invalid_scopes_are_explicit() {
        let root = std::env::temp_dir().join(format!("diskweave-top-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        for i in 0..125 {
            std::fs::write(root.join(format!("{i:03}.bin")), vec![0; i]).unwrap();
        }
        let sources = vec![Snapshot {
            entries: None,
            id: 1,
            generation: 1,
            scan: Arc::new(crate::scanner::scan(&root, || false).unwrap()),
        }];
        let stats = compute(&sources, &StatsScope::All, || false).unwrap();
        assert_eq!((stats.files, stats.bytes), (125, 7750));
        assert_eq!(
            (
                stats.top(10).len(),
                stats.top(50).len(),
                stats.top(100).len(),
                stats.top(101).len()
            ),
            (10, 50, 100, 100)
        );
        assert_eq!(
            crate::search::item_name(&sources[0].scan, stats.top(100)[99].item),
            "025.bin"
        );
        assert_eq!(
            compute(&sources, &StatsScope::Matches(vec![]), || false)
                .unwrap()
                .files,
            0
        );
        assert!(compute(&sources, &StatsScope::Subtree(99), || false).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn matched_statistics_exclude_folders_deduplicate_hits_and_keep_sources() {
        let root =
            std::env::temp_dir().join(format!("diskweave-stats-matches-{}", std::process::id()));
        std::fs::create_dir_all(root.join("a")).unwrap();
        std::fs::create_dir(root.join("b")).unwrap();
        std::fs::write(root.join("a/same.bin"), [0; 3]).unwrap();
        std::fs::write(root.join("a/big.bin"), [0; 9]).unwrap();
        std::fs::hard_link(root.join("a/same.bin"), root.join("a/alias.bin")).unwrap();
        std::fs::write(root.join("b/same.bin"), [0; 3]).unwrap();
        let sources: Vec<_> = ["a", "b"]
            .iter()
            .enumerate()
            .map(|(index, path)| Snapshot {
                entries: None,
                id: index as u64,
                generation: 1,
                scan: Arc::new(crate::scanner::scan(&root.join(path), || false).unwrap()),
            })
            .collect();
        let id = sources[0]
            .scan
            .files
            .iter()
            .position(|f| f.name == "same.bin")
            .unwrap();
        let matched = StatsScope::Matches(vec![
            Hit {
                source: 0,
                item: Item::Folder(0),
            },
            Hit {
                source: 0,
                item: Item::File(id),
            },
            Hit {
                source: 0,
                item: Item::File(id),
            },
            Hit {
                source: 1,
                item: Item::File(0),
            },
        ]);
        let stats = compute(&sources, &matched, || false).unwrap();
        assert_eq!((stats.bytes, stats.files), (6, 2));
        let all = compute(&sources, &StatsScope::All, || false).unwrap();
        assert_eq!((all.bytes, all.files), (18, 4));
        assert_eq!(all.top(1)[0].source, 0);
        assert_eq!(
            crate::search::item_name(&sources[0].scan, all.top(1)[0].item),
            "big.bin"
        );
        assert!(compute(&[], &StatsScope::All, || true).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn subtree_statistics_group_extensions_and_top_files_without_counting_folders() {
        let root =
            std::env::temp_dir().join(format!("diskweave-statistics-{}", std::process::id()));
        std::fs::create_dir_all(root.join("data").join("deep")).unwrap();
        for (name, size) in [
            ("data/a.NPY", 3),
            ("data/b.npy", 5),
            ("data/deep/c.nii.gz", 7),
            ("data/deep/d.tar.gz", 11),
            ("data/README", 0),
            ("data/.gitignore", 2),
            ("outside.bin", 100),
        ] {
            std::fs::write(root.join(name), vec![0; size]).unwrap();
        }
        let scan = Arc::new(crate::scanner::scan(&root, || false).unwrap());
        let folder = scan.folder_index(std::path::Path::new("data")).unwrap();
        let sources = vec![crate::locations::Snapshot {
            entries: None,
            id: 1,
            generation: 1,
            scan,
        }];
        let stats = compute(&sources, &StatsScope::Subtree(folder), || false).unwrap();
        assert_eq!((stats.bytes, stats.files), (28, 6));
        let npy = stats.extensions.iter().find(|e| e.name == ".npy").unwrap();
        assert_eq!((npy.bytes, npy.files), (8, 2));
        assert_eq!(
            stats
                .extensions
                .iter()
                .find(|e| e.name == "확장자 없음")
                .unwrap()
                .files,
            2
        );
        let top = stats.top(10);
        assert_eq!(top.len(), 6);
        assert_eq!(
            crate::search::item_name(&sources[top[0].source].scan, top[0].item),
            "d.tar.gz"
        );
        std::fs::remove_dir_all(root).unwrap();
    }
}
