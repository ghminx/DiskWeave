use crate::explorer::Item;
use crate::global_search::Hit;
use crate::locations::Snapshot;
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
    pub bytes: u64,
    pub files: u64,
    pub extensions: Vec<ExtensionTotal>,
    top: Vec<Hit>,
}
impl Statistics {
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
    let mut add = |source: usize, id: usize| -> io::Result<()> {
        if cancelled() {
            return Err(io::Error::new(io::ErrorKind::Interrupted, "통계 취소"));
        }
        let file = &sources[source].scan.files[id];
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
                for &id in &scan.file_children[folder] {
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
