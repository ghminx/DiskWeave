use crate::explorer::Item;
use crate::metadata::{Allocation, Extra};
use crate::scanner::ScanResult;
use crate::snapshot::SavedScan;
use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::io;
use std::path::PathBuf;
use std::time::SystemTime;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChangeKind {
    Added,
    Removed,
    Modified,
    Review,
}
impl ChangeKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Added => "추가",
            Self::Removed => "삭제",
            Self::Modified => "정보 변경",
            Self::Review => "확인 필요",
        }
    }
}
pub struct Change {
    pub path: PathBuf,
    pub folder: bool,
    pub kind: ChangeKind,
    pub before: Option<u64>,
    pub after: Option<u64>,
    pub delta: Option<i128>,
    pub allocation_delta: Option<i128>,
}
pub struct Comparison {
    pub root: PathBuf,
    pub before_bytes: u64,
    pub after_bytes: u64,
    pub delta: i128,
    pub incomplete: bool,
    pub volume_unverified: bool,
    pub changes: Vec<Change>,
    pub before_time: SystemTime,
    pub after_time: SystemTime,
}
#[derive(PartialEq, Eq)]
struct Info {
    folder: bool,
    bytes: u64,
    allocation: Allocation,
    created: Option<SystemTime>,
    modified: Option<SystemTime>,
    attributes: u32,
    extra: Option<Extra>,
    files: Option<u64>,
    folders: Option<u64>,
}
fn info(scan: &ScanResult, item: Item) -> Info {
    match item {
        Item::File(id) => {
            let f = &scan.files[id];
            let mut a = Allocation::default();
            a.add(f.extra.allocation).expect("단일 파일 할당 크기");
            Info {
                folder: false,
                bytes: f.logical_bytes,
                allocation: a,
                created: f.created,
                modified: f.modified,
                attributes: f.attributes,
                extra: Some(f.extra),
                files: None,
                folders: None,
            }
        }
        Item::Folder(id) => {
            let f = &scan.folders[id];
            Info {
                folder: true,
                bytes: f.logical_bytes,
                allocation: f.allocation,
                created: f.created,
                modified: f.modified,
                attributes: f.attributes,
                extra: None,
                files: Some(f.file_count),
                folders: Some(f.folder_count),
            }
        }
    }
}
fn entries(
    scan: &ScanResult,
    cancelled: &impl Fn() -> bool,
) -> io::Result<BTreeMap<PathBuf, Item>> {
    let root = &scan.folders[0].path;
    let mut entries = BTreeMap::new();
    for item in (0..scan.folders.len())
        .map(Item::Folder)
        .chain((0..scan.files.len()).map(Item::File))
    {
        crate::output::check(cancelled)?;
        entries.insert(
            crate::search::item_path(scan, item)
                .strip_prefix(root)
                .map_err(io::Error::other)?
                .to_path_buf(),
            item,
        );
    }
    Ok(entries)
}
fn volume(scan: &ScanResult) -> Option<u64> {
    scan.files
        .iter()
        .find_map(|f| f.extra.identity.value().map(|id| id.volume))
}
pub fn compare(
    before: &SavedScan,
    after: &SavedScan,
    cancelled: impl Fn() -> bool,
) -> io::Result<Comparison> {
    crate::output::check(&cancelled)?;
    let a = &before.scan;
    let b = &after.scan;
    if a.folders[0].path != b.folders[0].path {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "같은 분석 경로의 전체 결과끼리 비교하세요",
        ));
    }
    let (old_volume, new_volume) = (volume(a), volume(b));
    if let (Some(a), Some(b)) = (old_volume, new_volume) {
        if a != b {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "드라이브 식별 정보가 달라 비교할 수 없습니다",
            ));
        }
    }
    let mut old = entries(a, &cancelled)?.into_iter().peekable();
    let mut new = entries(b, &cancelled)?.into_iter().peekable();
    let incomplete = a.error_count > 0
        || b.error_count > 0
        || a.skipped_reparse_points > 0
        || b.skipped_reparse_points > 0;
    let mut changes = Vec::new();
    loop {
        let order = match (old.peek(), new.peek()) {
            (Some((a, _)), Some((b, _))) => a.cmp(b),
            (Some(_), None) => Ordering::Less,
            (None, Some(_)) => Ordering::Greater,
            (None, None) => break,
        };
        crate::output::check(&cancelled)?;
        let old_row = (order != Ordering::Greater).then(|| old.next().unwrap());
        let new_row = (order != Ordering::Less).then(|| new.next().unwrap());
        let left = old_row.as_ref().map(|&(_, item)| info(a, item));
        let right = new_row.as_ref().map(|&(_, item)| info(b, item));
        if left == right {
            continue;
        }
        let folder = right.as_ref().or(left.as_ref()).unwrap().folder;
        let kind = match (&left, &right) {
            (_, None) if incomplete => ChangeKind::Review,
            (None, _) if incomplete => ChangeKind::Review,
            (Some(_), None) => ChangeKind::Removed,
            (None, Some(_)) => ChangeKind::Added,
            _ if folder && incomplete => ChangeKind::Review,
            _ => ChangeKind::Modified,
        };
        let allocation_delta = if left.as_ref().is_none_or(|v| v.allocation.missing() == 0)
            && right.as_ref().is_none_or(|v| v.allocation.missing() == 0)
            && kind != ChangeKind::Review
        {
            Some(
                i128::from(right.as_ref().map_or(0, |v| v.allocation.bytes))
                    - i128::from(left.as_ref().map_or(0, |v| v.allocation.bytes)),
            )
        } else {
            None
        };
        let before = left.map(|v| v.bytes);
        let after = right.map(|v| v.bytes);
        let delta = if kind == ChangeKind::Review && (before.is_none() || after.is_none()) {
            None
        } else {
            Some(i128::from(after.unwrap_or(0)) - i128::from(before.unwrap_or(0)))
        };
        changes.push(Change {
            path: old_row.or(new_row).unwrap().0,
            folder,
            kind,
            before,
            after,
            delta,
            allocation_delta,
        });
    }
    Ok(Comparison {
        root: a.folders[0].path.clone(),
        before_bytes: a.folders[0].logical_bytes,
        after_bytes: b.folders[0].logical_bytes,
        delta: i128::from(b.folders[0].logical_bytes) - i128::from(a.folders[0].logical_bytes),
        incomplete,
        volume_unverified: old_volume.is_none() || new_volume.is_none(),
        changes,
        before_time: before.finished,
        after_time: after.finished,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::time::{Duration, SystemTime};
    #[test]
    fn comparison_uses_paths_keeps_signed_sizes_and_does_not_treat_missing_coverage_as_deletion() {
        let root = std::env::temp_dir().join(format!("diskweave-compare-{}", std::process::id()));
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::write(root.join("sub/change.bin"), [0; 3]).unwrap();
        std::fs::write(root.join("remove.bin"), [0; 7]).unwrap();
        let now = SystemTime::now();
        let before = crate::snapshot::SavedScan::new(
            Arc::new(crate::scanner::scan(&root, || false).unwrap()),
            now,
            now,
            Duration::ZERO,
        );
        let archive = root.with_extension("dwsnap");
        crate::snapshot::save(&before, &archive, false, || false).unwrap();
        let restored = crate::snapshot::load(&archive, || false).unwrap();
        assert!(compare(&before, &restored, || false)
            .unwrap()
            .changes
            .is_empty());
        std::fs::remove_file(archive).unwrap();
        std::fs::write(root.join("sub/change.bin"), [1; 8]).unwrap();
        std::fs::remove_file(root.join("remove.bin")).unwrap();
        std::fs::write(root.join("add.bin"), [1; 5]).unwrap();
        let mut after_scan = crate::scanner::scan(&root, || false).unwrap();
        after_scan.metadata_error_count = 1;
        let mut after =
            crate::snapshot::SavedScan::new(Arc::new(after_scan), now, now, Duration::ZERO);
        let report = compare(&before, &after, || false).unwrap();
        assert_eq!(
            (report.before_bytes, report.after_bytes, report.delta),
            (10, 13, 3)
        );
        assert!(report
            .changes
            .windows(2)
            .all(|pair| pair[0].path < pair[1].path));
        let removed = report
            .changes
            .iter()
            .find(|c| c.path == std::path::Path::new("remove.bin"))
            .unwrap();
        assert_eq!(removed.kind, ChangeKind::Removed);
        assert_eq!(removed.delta, Some(-7));
        assert_eq!(removed.allocation_delta, None);
        let added = report
            .changes
            .iter()
            .find(|c| c.path == std::path::Path::new("add.bin"))
            .unwrap();
        assert_eq!(added.kind, ChangeKind::Added);
        let changed = report
            .changes
            .iter()
            .find(|c| c.path == std::path::Path::new("sub/change.bin"))
            .unwrap();
        assert_eq!(changed.kind, ChangeKind::Modified);
        assert_eq!(changed.delta, Some(5));
        Arc::get_mut(&mut after.scan).unwrap().error_count = 1;
        let partial = compare(&before, &after, || false).unwrap();
        assert_eq!(
            partial
                .changes
                .iter()
                .find(|c| c.path == std::path::Path::new("remove.bin"))
                .unwrap()
                .kind,
            ChangeKind::Review
        );
        assert!(partial.incomplete);
        assert_eq!(
            partial
                .changes
                .iter()
                .find(|c| c.path == std::path::Path::new("remove.bin"))
                .unwrap()
                .delta,
            None
        );
        assert_eq!(
            compare(&before, &after, || true).err().unwrap().kind(),
            std::io::ErrorKind::Interrupted
        );
        let other = root.join("sub");
        let wrong = crate::snapshot::SavedScan::new(
            Arc::new(crate::scanner::scan(&other, || false).unwrap()),
            now,
            now,
            Duration::ZERO,
        );
        assert!(compare(&before, &wrong, || false).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }
}
