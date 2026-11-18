use crate::explorer::Item;
use crate::global_search::Hit;
use crate::locations::Snapshot;
use crate::metadata::{Allocation, Field, Identity, Missing};
use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap, HashSet};
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
}
impl Statistics {
    pub(crate) fn patch_local(
        &mut self,
        previous: &Snapshot,
        current: &Snapshot,
        folder: usize,
    ) -> bool {
        let Some(items) = current.scan.stable_changes_from(&previous.scan) else {
            return false;
        };
        let root = &current.scan.folders[folder];
        let files: Vec<_> = items
            .iter()
            .filter_map(|&item| {
                let Item::File(row) = item else { return None };
                let (old, new) = (&previous.scan.files[row], &current.scan.files[row]);
                current.scan.folders[new.parent]
                    .path
                    .starts_with(&root.path)
                    .then_some((item, old, new))
            })
            .collect();
        // 할당량과 식별자가 유지되는 경우에만 Hard Link 집계 재사용.
        if files.iter().any(|&(item, old, new)| {
            old.extra.allocation != new.extra.allocation
                || old.extra.identity != new.extra.identity
                || (self.files > self.top.len() as u64
                    && new.logical_bytes < old.logical_bytes
                    && self.top.iter().any(|hit| hit.item == item))
        }) {
            return false;
        }
        let mut groups: HashMap<_, _> = self
            .extensions
            .iter()
            .map(|g| (g.name.clone(), g.bytes))
            .collect();
        for &(_, old, _) in &files {
            let Some(bytes) = groups.get_mut(&extension(&old.name)) else {
                return false;
            };
            let Some(value) = bytes.checked_sub(old.logical_bytes) else {
                return false;
            };
            *bytes = value;
        }
        for &(item, _, new) in &files {
            *groups.get_mut(&extension(&new.name)).unwrap() += new.logical_bytes;
            if !self.top.iter().any(|hit| hit.item == item) {
                self.top.push(Hit { source: 0, item });
            }
        }
        self.bytes = root.logical_bytes;
        for group in &mut self.extensions {
            group.bytes = groups[&group.name];
        }
        self.extensions
            .sort_unstable_by(|a, b| b.bytes.cmp(&a.bytes).then_with(|| a.name.cmp(&b.name)));
        self.top.sort_unstable_by(|a, b| {
            crate::search::item_bytes(&current.scan, b.item)
                .cmp(&crate::search::item_bytes(&current.scan, a.item))
                .then_with(|| {
                    crate::search::item_path(&current.scan, a.item)
                        .cmp(&crate::search::item_path(&current.scan, b.item))
                })
        });
        self.top.truncate(100);
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
    let mut unique: HashMap<Identity, (Field<u64>, bool)> = HashMap::new();
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
                    .and_modify(|old| {
                        if !old.1 {
                            match (old.0, file.extra.allocation) {
                                (Field::Value(a), Field::Value(b)) if a != b => {
                                    *old = (Field::Missing(Missing::Failed), true);
                                }
                                (Field::Missing(_), Field::Value(value)) => {
                                    old.0 = Field::Value(value)
                                }
                                _ => {}
                            }
                        }
                    })
                    .or_insert((file.extra.allocation, false));
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
        heap.push(Reverse((file.logical_bytes, Reverse(path), source, id)));
        if heap.len() > 100 {
            heap.pop();
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
    for (value, conflict) in unique.into_values() {
        if cancelled() {
            return Err(io::Error::new(io::ErrorKind::Interrupted, "통계 취소"));
        }
        if conflict {
            identity_conflicts += 1;
        }
        unique_allocation.add(value)?;
    }
    let mut extensions: Vec<_> = groups
        .into_iter()
        .map(|(name, (bytes, files))| ExtensionTotal { name, bytes, files })
        .collect();
    extensions.sort_unstable_by(|a, b| b.bytes.cmp(&a.bytes).then_with(|| a.name.cmp(&b.name)));
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
    Ok(Statistics {
        allocation,
        unique_allocation,
        identity_conflicts,
        bytes,
        files,
        extensions,
        top,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
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
