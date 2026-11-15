use crate::{
    metadata::Extra,
    scanner::{FileEntry, Folder, ScanResult},
    snapshot::SavedScan,
};
use serde::{Deserialize, Serialize};
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    io,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime},
};

#[derive(Clone)]
pub struct Journal {
    pub volume: String,
    pub id: u64,
    pub first: i64,
    pub lowest: i64,
    pub next: i64,
}
#[derive(Clone, Debug)]
pub struct Event {
    pub usn: i64,
    pub id: u64,
    pub parent: u64,
    pub reason: u32,
}
pub trait Source {
    fn journal(&mut self) -> io::Result<Journal>;
    fn read(&mut self, start: i64, end: i64) -> io::Result<Vec<Event>>;
    fn inspect(&mut self, path: &Path, detailed: bool) -> io::Result<crate::ntfs::Info>;
    fn list(&mut self, path: &Path) -> io::Result<Vec<PathBuf>>;
}
/// USN 페이지의 길이·형식·이름·단조 증가를 확인한 뒤 변경 ID만 반환.
pub fn decode(page: &[u8], start: i64) -> io::Result<(i64, Vec<Event>)> {
    if page.len() < 8 {
        return Err(invalid("잘린 변경 기록 페이지"));
    }
    let next = i64::from_le_bytes(page[..8].try_into().unwrap());
    if next < start {
        return Err(invalid("역행한 변경 기록 커서"));
    }
    let mut events = Vec::new();
    let mut at = 8;
    let mut previous = start;
    while at < page.len() {
        let b = &page[at..];
        if b.len() < 60 {
            return Err(invalid("잘린 변경 기록"));
        }
        let len = u32::from_le_bytes(b[..4].try_into().unwrap()) as usize;
        if len < 60
            || len > b.len()
            || !len.is_multiple_of(8)
            || u16::from_le_bytes(b[4..6].try_into().unwrap()) != 2
        {
            return Err(invalid("지원하지 않거나 손상된 변경 기록 형식"));
        }
        let size = u16::from_le_bytes(b[56..58].try_into().unwrap()) as usize;
        let offset = u16::from_le_bytes(b[58..60].try_into().unwrap()) as usize;
        if size == 0 || !size.is_multiple_of(2) || offset < 60 || offset + size > len {
            return Err(invalid("변경 기록 이름 범위 오류"));
        }
        for pair in b[offset..offset + size].as_chunks::<2>().0 {
            if [0, 47, 92, 58].contains(&u16::from_le_bytes(*pair)) {
                return Err(invalid("변경 기록 이름 오류"));
            }
        }
        let usn = i64::from_le_bytes(b[24..32].try_into().unwrap());
        let id = u64::from_le_bytes(b[8..16].try_into().unwrap());
        let parent = u64::from_le_bytes(b[16..24].try_into().unwrap());
        if usn < previous || usn >= next || id == 0 || parent == 0 {
            return Err(invalid("변경 기록 순서 또는 ID 오류"));
        }
        previous = usn + 1;
        events.push(Event {
            usn,
            id,
            parent,
            reason: u32::from_le_bytes(b[40..44].try_into().unwrap()),
        });
        at += len;
    }
    Ok((next, events))
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Excluded {
    pub path: Vec<u16>,
    pub id: u64,
    pub attributes: u32,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Checkpoint {
    pub version: u32,
    pub volume: String,
    pub root: u64,
    pub journal: u64,
    pub next: i64,
    pub folders: Vec<u64>,
    pub files: Vec<u64>,
    pub excluded: Vec<Excluded>,
    pub pending: Vec<u64>,
}
fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}
#[derive(Clone, PartialEq, Eq)]
struct Node {
    id: u64,
    attributes: u32,
    bytes: u64,
    created: Option<SystemTime>,
    modified: Option<SystemTime>,
    extra: Extra,
    issue: Option<String>,
}
impl Node {
    fn from_info(info: crate::ntfs::Info) -> io::Result<Self> {
        let Some(crate::ntfs::Id::V2(id)) = info.reference else {
            return Err(invalid("항목 ID를 확인할 수 없습니다"));
        };
        Ok(Self {
            id,
            attributes: info.attributes,
            bytes: if info.attributes & 0x10 != 0 {
                0
            } else {
                info.bytes
            },
            created: info.created,
            modified: info.modified,
            extra: info.extra.values,
            issue: info.extra.issue.map(|e| e.to_string()),
        })
    }
    fn dir(&self) -> bool {
        self.attributes & 0x10 != 0
    }
    fn excluded(&self) -> bool {
        self.attributes & 0x400 != 0
    }
}
pub struct Index {
    root: PathBuf,
    detailed: bool,
    nodes: BTreeMap<PathBuf, Node>,
    pub checkpoint: Checkpoint,
    scan: Option<Arc<ScanResult>>,
    started: SystemTime,
}
impl Checkpoint {
    pub fn validate(&self, scan: &ScanResult) -> io::Result<()> {
        if self.version != 1
            || self.volume.is_empty()
            || self.root == 0
            || self.next < 0
            || self.folders.len() != scan.folders.len()
            || self.files.len() != scan.files.len()
            || self.folders.first() != Some(&self.root)
            || self.folders.iter().chain(&self.files).any(|id| *id == 0)
        {
            return Err(invalid("변경 추적 기준점과 분석 결과가 일치하지 않습니다"));
        }
        let mut dirs = HashSet::new();
        if self.folders.iter().any(|id| !dirs.insert(*id)) {
            return Err(invalid("중복된 폴더 ID"));
        }
        let files: HashSet<_> = self.files.iter().copied().collect();
        let mut pending = HashSet::new();
        if self.pending.len() > files.len()
            || self
                .pending
                .iter()
                .any(|id| !files.contains(id) || !pending.insert(*id))
            || files.iter().any(|id| dirs.contains(id))
        {
            return Err(invalid("확인 대기 파일의 추적 ID 오류"));
        }
        let mut paths = HashSet::new();
        for e in &self.excluded {
            let p = PathBuf::from(std::ffi::OsString::from_wide(&e.path));
            if e.id == 0
                || e.attributes & 0x400 == 0
                || p.as_os_str().is_empty()
                || e.path.len() > 32767
                || e.path.contains(&0)
                || !paths.insert(p.clone())
                || p.components()
                    .any(|c| !matches!(c, std::path::Component::Normal(_)))
            {
                return Err(invalid("제외 경로의 추적 정보가 올바르지 않습니다"));
            }
        }
        Ok(())
    }
}
impl Index {
    pub fn begin(
        root: &Path,
        detailed: bool,
        source: &mut impl Source,
        cancelled: &impl Fn() -> bool,
    ) -> io::Result<Self> {
        let started = SystemTime::now();
        let journal = source.journal()?;
        let first = Node::from_info(source.inspect(root, false)?)?;
        if !first.dir() || first.excluded() {
            return Err(invalid("일반 폴더만 변경 추적할 수 있습니다"));
        }
        let checkpoint = Checkpoint {
            version: 1,
            volume: journal.volume,
            root: first.id,
            journal: journal.id,
            next: journal.next,
            folders: vec![],
            files: vec![],
            excluded: vec![],
            pending: vec![],
        };
        let mut nodes = BTreeMap::new();
        nodes.insert(PathBuf::new(), first);
        Self::collect(root, Path::new(""), detailed, source, cancelled, &mut nodes)?;
        let mut index = Self {
            root: root.to_owned(),
            detailed,
            nodes,
            checkpoint,
            scan: None,
            started,
        };
        index.refresh(source, cancelled)?;
        Ok(index)
    }
    fn collect(
        root: &Path,
        relative: &Path,
        detailed: bool,
        source: &mut impl Source,
        cancelled: &impl Fn() -> bool,
        nodes: &mut BTreeMap<PathBuf, Node>,
    ) -> io::Result<()> {
        let mut pending = vec![relative.to_owned()];
        while let Some(parent) = pending.pop() {
            crate::output::check(cancelled)?;
            let absolute = root.join(&parent);
            for path in source.list(&absolute)? {
                crate::output::check(cancelled)?;
                if path.parent() != Some(absolute.as_path()) {
                    return Err(invalid("열거한 항목의 부모 경로가 다릅니다"));
                }
                let rel = path
                    .strip_prefix(root)
                    .map_err(|_| invalid("추적 범위 밖 항목"))?
                    .to_owned();
                let node = Node::from_info(source.inspect(&path, detailed)?)?;
                if node.dir() && !node.excluded() {
                    pending.push(rel.clone());
                }
                nodes.insert(rel, node);
            }
        }
        Ok(())
    }
    pub fn restore(record: &SavedScan) -> io::Result<Self> {
        let state = record
            .tracking
            .as_ref()
            .ok_or_else(|| invalid("저장된 변경 추적 기준점이 없습니다"))?;
        state.validate(&record.scan)?;
        if record.scan.error_count != 0 {
            return Err(invalid("누락된 결과에서 변경 추적을 이어갈 수 없습니다"));
        }
        let root = record.scan.folders[0].path.clone();
        let issues: HashMap<_, _> = record
            .scan
            .metadata_issues
            .iter()
            .map(|issue| (issue.path.as_path(), issue.error.to_string()))
            .collect();
        let mut nodes = BTreeMap::new();
        for (i, f) in record.scan.folders.iter().enumerate() {
            nodes.insert(
                f.path
                    .strip_prefix(&root)
                    .map_err(|_| invalid("폴더 범위 불일치"))?
                    .to_owned(),
                Node {
                    id: state.folders[i],
                    attributes: f.attributes,
                    bytes: 0,
                    created: f.created,
                    modified: f.modified,
                    extra: Extra::default(),
                    issue: None,
                },
            );
        }
        for (i, f) in record.scan.files.iter().enumerate() {
            let p = record.scan.folders[f.parent].path.join(&f.name);
            use crate::metadata::{Field, Missing};
            let failed = matches!(
                f.extra.allocation,
                Field::Missing(Missing::Failed | Missing::Unsupported)
            ) || matches!(
                f.extra.identity,
                Field::Missing(Missing::Failed | Missing::Unsupported)
            ) || matches!(
                f.extra.links,
                Field::Missing(Missing::Failed | Missing::Unsupported)
            );
            let issue = issues
                .get(p.as_path())
                .cloned()
                .or_else(|| failed.then(|| "저장된 메타데이터에 미지원·조회 실패 값 포함".into()));
            nodes.insert(
                p.strip_prefix(&root)
                    .map_err(|_| invalid("파일 범위 불일치"))?
                    .to_owned(),
                Node {
                    id: state.files[i],
                    attributes: f.attributes,
                    bytes: f.logical_bytes,
                    created: f.created,
                    modified: f.modified,
                    extra: f.extra,
                    issue,
                },
            );
        }
        for e in &state.excluded {
            let p = PathBuf::from(std::ffi::OsString::from_wide(&e.path));
            if nodes.contains_key(&p) {
                return Err(invalid("제외 항목과 분석 항목이 겹칩니다"));
            }
            nodes.insert(
                p,
                Node {
                    id: e.id,
                    attributes: e.attributes,
                    bytes: 0,
                    created: None,
                    modified: None,
                    extra: Extra::default(),
                    issue: None,
                },
            );
        }
        Ok(Self {
            root,
            detailed: record.scan.detailed,
            nodes,
            checkpoint: state.clone(),
            scan: Some(Arc::clone(&record.scan)),
            started: record.started,
        })
    }
    fn verify(&self, j: &Journal) -> io::Result<()> {
        if j.volume != self.checkpoint.volume
            || j.id != self.checkpoint.journal
            || self.checkpoint.next < j.first.max(j.lowest)
            || self.checkpoint.next > j.next
        {
            return Err(invalid("드라이브 또는 변경 기록이 바뀌었거나 필요한 기록이 유실되었습니다. 다시 스캔해 주세요"));
        }
        Ok(())
    }
    pub fn refresh(
        &mut self,
        source: &mut impl Source,
        cancelled: &impl Fn() -> bool,
    ) -> io::Result<bool> {
        crate::output::check(cancelled)?;
        let journal = source.journal()?;
        self.verify(&journal)?;
        let root = Node::from_info(source.inspect(&self.root, false)?)?;
        if root.id != self.checkpoint.root || !root.dir() || root.excluded() {
            return Err(invalid(
                "추적 폴더가 이동·교체되었습니다. 폴더를 다시 지정해 주세요",
            ));
        }
        let events = source.read(self.checkpoint.next, journal.next)?;
        if events.is_empty()
            && self.checkpoint.pending.is_empty()
            && self.nodes[Path::new("")] == root
        {
            // 유휴 확인마다 전체 파일 목록을 복사하지 않도록 변경 없는 배치 종료.
            let final_journal = source.journal()?;
            self.verify(&final_journal)?;
            if journal.next < final_journal.first.max(final_journal.lowest) {
                return Err(invalid("반영 중 변경 기록이 유실되었습니다"));
            }
            crate::output::check(cancelled)?;
            self.checkpoint.next = journal.next;
            return Ok(false);
        }
        let mut by_id: HashMap<u64, Vec<PathBuf>> = HashMap::new();
        for (path, n) in &self.nodes {
            by_id.entry(n.id).or_default().push(path.clone());
        }
        let mut directories = HashSet::new();
        let mut files = HashSet::new();
        let mut pending: HashSet<_> = self.checkpoint.pending.iter().copied().collect();
        for e in &events {
            if e.id == self.checkpoint.root && e.reason & 0x3200 != 0 {
                return Err(invalid(
                    "추적 루트의 이름·위치가 변경되었습니다. 폴더를 다시 지정해 주세요",
                ));
            }
            if e.usn < self.checkpoint.next || e.usn >= journal.next {
                return Err(invalid("변경 기록의 USN 범위가 올바르지 않습니다"));
            }
            if e.reason & 0x80000000 != 0 {
                pending.remove(&e.id);
            } else {
                pending.insert(e.id);
            }
            let structural = e.reason & 0x00373300 != 0;
            if let Some(paths) = by_id.get(&e.id) {
                for p in paths {
                    let n = &self.nodes[p];
                    if p.as_os_str().is_empty() {
                        continue;
                    }
                    if structural || n.dir() || n.excluded() {
                        directories.insert(p.parent().unwrap_or(Path::new("")).to_owned());
                    } else {
                        files.insert(p.clone());
                    }
                }
            }
            if let Some(paths) = by_id.get(&e.parent) {
                for p in paths {
                    if self.nodes[p].dir()
                        && !self.nodes[p].excluded()
                        && (structural || !by_id.contains_key(&e.id))
                    {
                        directories.insert(p.clone());
                    }
                }
            }
        }
        for id in &pending {
            if let Some(paths) = by_id.get(id) {
                for p in paths {
                    if !self.nodes[p].dir() {
                        files.insert(p.clone());
                    }
                }
            }
        }
        let mut next = self.nodes.clone();
        next.insert(PathBuf::new(), root);
        let mut directories: Vec<_> = directories.into_iter().collect();
        directories.sort_by_key(|p| p.components().count());
        for directory in directories {
            crate::output::check(cancelled)?;
            if !next
                .get(&directory)
                .is_some_and(|n| n.dir() && !n.excluded())
            {
                continue;
            }
            let absolute = self.root.join(&directory);
            let current = Node::from_info(source.inspect(&absolute, false)?)?;
            if current.id != next[&directory].id || !current.dir() || current.excluded() {
                return Err(invalid("반영 중 상위 폴더가 교체되었습니다"));
            }
            next.insert(directory.clone(), current);
            let children = source.list(&absolute)?;
            let mut seen = HashSet::new();
            for path in children {
                if path.parent() != Some(absolute.as_path()) {
                    return Err(invalid("열거한 경로가 추적 부모와 다릅니다"));
                }
                let rel = path
                    .strip_prefix(&self.root)
                    .map_err(|_| invalid("추적 범위 밖 경로"))?
                    .to_owned();
                let node = Node::from_info(source.inspect(&path, self.detailed)?)?;
                seen.insert(rel.clone());
                let replaced = next.get(&rel).is_none_or(|old| {
                    old.id != node.id || old.attributes & 0x410 != node.attributes & 0x410
                });
                if replaced {
                    next.retain(|p, _| !p.starts_with(&rel));
                }
                let descend = replaced && node.dir() && !node.excluded();
                next.insert(rel.clone(), node);
                if descend {
                    Self::collect(
                        &self.root,
                        &rel,
                        self.detailed,
                        source,
                        cancelled,
                        &mut next,
                    )?;
                }
            }
            let removed: Vec<_> = next
                .keys()
                .filter(|p| p.parent() == Some(directory.as_path()) && !seen.contains(*p))
                .cloned()
                .collect();
            for p in removed {
                next.retain(|key, _| !key.starts_with(&p));
            }
        }
        for path in files {
            crate::output::check(cancelled)?;
            if !next.contains_key(&path) || next[&path].id != self.nodes[&path].id {
                continue;
            }
            let node = Node::from_info(source.inspect(&self.root.join(&path), self.detailed)?)?;
            if node.id != next[&path].id {
                return Err(invalid("변경 확인 중 파일이 교체되었습니다"));
            }
            next.insert(path, node);
        }
        let final_root = Node::from_info(source.inspect(&self.root, false)?)?;
        if final_root.id != self.checkpoint.root || final_root.excluded() {
            return Err(invalid("반영 중 추적 폴더가 바뀌었습니다"));
        }
        let final_journal = source.journal()?;
        self.verify(&final_journal)?;
        if journal.next < final_journal.first.max(final_journal.lowest) {
            return Err(invalid("반영 중 변경 기록이 유실되었습니다"));
        }
        crate::output::check(cancelled)?;
        let changed = self.nodes != next;
        self.nodes = next;
        self.checkpoint.next = journal.next;
        let file_ids: HashSet<_> = self
            .nodes
            .values()
            .filter(|n| !n.dir() && !n.excluded())
            .map(|n| n.id)
            .collect();
        self.checkpoint.pending = pending
            .into_iter()
            .filter(|id| file_ids.contains(id))
            .collect();
        self.checkpoint.pending.sort_unstable();
        if changed {
            self.scan = None;
        }
        Ok(changed)
    }
    pub fn snapshot(&mut self, cancelled: &impl Fn() -> bool) -> io::Result<SavedScan> {
        if self.scan.is_none() {
            let mut scan = ScanResult {
                detailed: self.detailed,
                metadata_error_count: 0,
                metadata_issues: vec![],
                folders: vec![],
                files: vec![],
                children: vec![],
                file_children: vec![],
                error_count: 0,
                issues: vec![],
                skipped_reparse_points: 0,
            };
            let mut folders = HashMap::new();
            let mut refs = vec![];
            let mut files = vec![];
            let mut excluded = vec![];
            let mut directories: Vec<_> = self
                .nodes
                .iter()
                .filter(|(_, n)| n.dir() && !n.excluded())
                .collect();
            directories.sort_by_key(|(p, _)| p.components().count());
            for (p, n) in directories {
                crate::output::check(cancelled)?;
                let parent = if p.as_os_str().is_empty() {
                    None
                } else {
                    Some(
                        *folders
                            .get(p.parent().unwrap())
                            .ok_or_else(|| invalid("부모 없는 추적 폴더"))?,
                    )
                };
                folders.insert(p.clone(), scan.folders.len());
                refs.push(n.id);
                scan.folders.push(Folder {
                    path: self.root.join(p),
                    parent,
                    logical_bytes: 0,
                    file_count: 0,
                    folder_count: 0,
                    allocation: Default::default(),
                    created: n.created,
                    modified: n.modified,
                    attributes: n.attributes,
                });
            }
            for (p, n) in &self.nodes {
                crate::output::check(cancelled)?;
                if n.excluded() {
                    scan.skipped_reparse_points += 1;
                    excluded.push(Excluded {
                        path: p.as_os_str().encode_wide().collect(),
                        id: n.id,
                        attributes: n.attributes,
                    });
                    continue;
                }
                if n.dir() {
                    continue;
                }
                let parent = *folders
                    .get(p.parent().unwrap())
                    .ok_or_else(|| invalid("부모 없는 추적 파일"))?;
                let folder = &mut scan.folders[parent];
                folder.logical_bytes = folder
                    .logical_bytes
                    .checked_add(n.bytes)
                    .ok_or_else(|| invalid("용량 합계 초과"))?;
                folder.file_count += 1;
                folder.allocation.add(n.extra.allocation)?;
                if let Some(issue) = &n.issue {
                    scan.metadata_error_count += 1;
                    if scan.metadata_issues.len() < 20 {
                        scan.metadata_issues.push(crate::scanner::ScanIssue {
                            path: self.root.join(p),
                            error: io::Error::other(issue.clone()),
                        });
                    }
                }
                scan.files.push(FileEntry {
                    name: p.file_name().unwrap().to_owned(),
                    parent,
                    logical_bytes: n.bytes,
                    created: n.created,
                    modified: n.modified,
                    attributes: n.attributes,
                    extra: n.extra,
                });
                files.push(n.id);
            }
            crate::scanner::finish(&mut scan)?;
            self.checkpoint.folders = refs;
            self.checkpoint.files = files;
            self.checkpoint.excluded = excluded;
            self.scan = Some(Arc::new(scan));
        }
        let mut record = SavedScan::new(
            Arc::clone(self.scan.as_ref().unwrap()),
            self.started,
            SystemTime::now(),
            Duration::ZERO,
        );
        record.tracking = Some(self.checkpoint.clone());
        Ok(record)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        time::{SystemTime, UNIX_EPOCH},
    };
    struct Fixture {
        root: PathBuf,
        journal: Journal,
        events: Vec<Event>,
    }
    impl Source for Fixture {
        fn journal(&mut self) -> io::Result<Journal> {
            Ok(self.journal.clone())
        }
        fn read(&mut self, start: i64, end: i64) -> io::Result<Vec<Event>> {
            Ok(self
                .events
                .iter()
                .filter(|e| e.usn >= start && e.usn < end)
                .cloned()
                .collect())
        }
        fn inspect(&mut self, path: &Path, detailed: bool) -> io::Result<crate::ntfs::Info> {
            crate::ntfs_native::inspect_file(path, detailed, &self.journal.volume)
        }
        fn list(&mut self, path: &Path) -> io::Result<Vec<PathBuf>> {
            fs::read_dir(path)?.map(|e| e.map(|e| e.path())).collect()
        }
    }
    impl Fixture {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!(
                "diskweave-usn-{}-{}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            fs::create_dir_all(root.join("scope")).unwrap();
            let drive = root.components().take(2).collect::<PathBuf>();
            let volume = crate::ntfs_native::target(&drive).unwrap().volume;
            Self {
                root,
                journal: Journal {
                    volume,
                    id: 7,
                    first: 0,
                    lowest: 0,
                    next: 100,
                },
                events: vec![],
            }
        }
        fn id(&mut self, path: &Path) -> u64 {
            let crate::ntfs::Id::V2(id) = self.inspect(path, false).unwrap().reference.unwrap();
            id
        }
        fn event(&mut self, id: u64, parent: u64, reason: u32) {
            let usn = self.journal.next;
            self.journal.next += 8;
            self.events.push(Event {
                usn,
                id,
                parent,
                reason,
            });
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }
    #[test]
    fn scoped_updates_include_incoming_subtrees_and_external_hard_link_writes() {
        let mut f = Fixture::new();
        let scope = f.root.join("scope");
        let a = scope.join("a.bin");
        fs::write(&a, b"12345").unwrap();
        fs::hard_link(&a, f.root.join("outside.bin")).unwrap();
        let mut index = Index::begin(&scope, true, &mut f, &|| false).unwrap();
        let initial = index.snapshot(&|| false).unwrap();
        assert_eq!(initial.scan.folders[0].logical_bytes, 5);
        let file = f.id(&a);
        let outside = f.root.clone();
        let parent = f.id(&outside);
        fs::write(f.root.join("outside.bin"), b"123456789").unwrap();
        f.event(file, parent, 0x80000001);
        assert!(index.refresh(&mut f, &|| false).unwrap());
        assert_eq!(
            index.snapshot(&|| false).unwrap().scan.folders[0].logical_bytes,
            9
        );
        fs::create_dir_all(f.root.join("incoming/deep")).unwrap();
        fs::write(f.root.join("incoming/deep/b"), b"1234567").unwrap();
        let incoming = f.id(&f.root.join("incoming"));
        fs::rename(f.root.join("incoming"), scope.join("incoming")).unwrap();
        let root_id = f.id(&scope);
        f.event(incoming, root_id, 0x80002000);
        index.refresh(&mut f, &|| false).unwrap();
        let result = index.snapshot(&|| false).unwrap();
        assert_eq!(result.scan.folders[0].logical_bytes, 16);
        assert_eq!(result.scan.folders[0].folder_count, 2);
    }
    #[test]
    fn moved_out_files_and_case_only_root_rename_never_reuse_old_scope_data() {
        let mut f = Fixture::new();
        let scope = f.root.join("scope");
        let file = scope.join("old");
        fs::write(&file, b"12345").unwrap();
        let mut index = Index::begin(&scope, false, &mut f, &|| false).unwrap();
        let id = f.id(&file);
        let parent = f.id(&scope);
        fs::rename(&file, f.root.join("outside")).unwrap();
        f.event(id, parent, 0x80001000);
        index.refresh(&mut f, &|| false).unwrap();
        assert_eq!(
            index.snapshot(&|| false).unwrap().scan.folders[0].logical_bytes,
            0
        );
        let checkpoint = index.checkpoint.clone();
        f.event(parent, 999, 0x80002000);
        assert!(index.refresh(&mut f, &|| false).is_err());
        assert_eq!(index.checkpoint, checkpoint);
    }
    #[test]
    fn changes_decode_rejects_bad_versions_and_out_of_order_records() {
        let mut page = vec![0u8; 72];
        page[..8].copy_from_slice(&116i64.to_le_bytes());
        page[8..12].copy_from_slice(&64u32.to_le_bytes());
        page[12..14].copy_from_slice(&2u16.to_le_bytes());
        page[16..24].copy_from_slice(&99u64.to_le_bytes());
        page[24..32].copy_from_slice(&5u64.to_le_bytes());
        page[32..40].copy_from_slice(&100i64.to_le_bytes());
        page[48..52].copy_from_slice(&0x80000100u32.to_le_bytes());
        page[64..66].copy_from_slice(&2u16.to_le_bytes());
        page[66..68].copy_from_slice(&60u16.to_le_bytes());
        page[68..70].copy_from_slice(&65u16.to_le_bytes());
        let (next, events) = decode(&page, 100).unwrap();
        assert_eq!(next, 116);
        assert_eq!(events[0].id, 99);
        assert_eq!(events[0].reason, 0x80000100);
        page[12..14].copy_from_slice(&3u16.to_le_bytes());
        assert!(decode(&page, 100).is_err());
        page[12..14].copy_from_slice(&2u16.to_le_bytes());
        assert!(decode(&page[..71], 100).is_err());
        assert!(decode(&page, 108).is_err());
    }
    #[test]
    fn checkpoints_roundtrip_and_corrupt_identity_lists_are_rejected() {
        let mut f = Fixture::new();
        let scope = f.root.join("scope");
        fs::write(scope.join("a"), b"12345").unwrap();
        let mut index = Index::begin(&scope, true, &mut f, &|| false).unwrap();
        let record = index.snapshot(&|| false).unwrap();
        let save = f.root.join("state.dwsnap");
        crate::snapshot::save(&record, &save, false, || false).unwrap();
        let reopened = crate::snapshot::load(&save, || false).unwrap();
        assert_eq!(reopened.tracking, record.tracking);
        let mut resumed = Index::restore(&reopened).unwrap();
        let id = f.id(&scope.join("a"));
        let parent = f.id(&scope);
        fs::write(scope.join("a"), b"1234567").unwrap();
        f.event(id, parent, 0x80000001);
        resumed.refresh(&mut f, &|| false).unwrap();
        assert_eq!(
            resumed.snapshot(&|| false).unwrap().scan.folders[0].logical_bytes,
            7
        );
        let mut value: serde_json::Value =
            serde_json::from_slice(&fs::read(&save).unwrap()).unwrap();
        value["tracking"]["files"][0] = 0.into();
        fs::write(&save, serde_json::to_vec(&value).unwrap()).unwrap();
        assert!(crate::snapshot::load(&save, || false).is_err());
    }
    #[test]
    fn restored_metadata_failures_remain_visible_after_unrelated_updates() {
        let mut f = Fixture::new();
        let scope = f.root.join("scope");
        fs::write(scope.join("failed"), b"12345").unwrap();
        fs::create_dir(scope.join("other")).unwrap();
        let mut index = Index::begin(&scope, true, &mut f, &|| false).unwrap();
        let mut record = index.snapshot(&|| false).unwrap();
        drop(index);
        let scan = Arc::get_mut(&mut record.scan).unwrap_or_else(|| panic!("독립 결과 필요"));
        scan.files[0].extra.identity =
            crate::metadata::Field::Missing(crate::metadata::Missing::Failed);
        scan.metadata_error_count = 1;
        let mut resumed = Index::restore(&record).unwrap();
        fs::write(scope.join("other/child"), b"123").unwrap();
        let parent = f.id(&scope.join("other"));
        let id = f.id(&scope.join("other/child"));
        f.event(id, parent, 0x80000100);
        resumed.refresh(&mut f, &|| false).unwrap();
        let updated = resumed.snapshot(&|| false).unwrap();
        assert_eq!(updated.scan.metadata_error_count, 1);
        assert!(updated
            .scan
            .metadata_issues
            .iter()
            .any(|e| e.path.ends_with("failed")));
    }
    #[test]
    fn checkpoint_rejects_duplicate_pending_ids_and_nul_excluded_paths() {
        let mut f = Fixture::new();
        let scope = f.root.join("scope");
        fs::write(scope.join("a"), b"1").unwrap();
        fs::write(scope.join("b"), b"2").unwrap();
        let mut index = Index::begin(&scope, false, &mut f, &|| false).unwrap();
        let record = index.snapshot(&|| false).unwrap();
        let mut checkpoint = record.tracking.unwrap();
        checkpoint.pending = vec![checkpoint.files[0]; 2];
        assert!(checkpoint.validate(&record.scan).is_err());
        checkpoint.pending.clear();
        checkpoint.excluded.push(Excluded {
            path: vec![65, 0, 66],
            id: 999,
            attributes: 0x400,
        });
        assert!(checkpoint.validate(&record.scan).is_err());
    }
    #[test]
    fn root_replacement_journal_loss_cancellation_and_open_writes_keep_the_last_checkpoint() {
        let mut f = Fixture::new();
        let scope = f.root.join("scope");
        let file = scope.join("a");
        fs::write(&file, b"12345").unwrap();
        let mut index = Index::begin(&scope, false, &mut f, &|| false).unwrap();
        let id = f.id(&file);
        let parent = f.id(&scope);
        f.event(id, parent, 1);
        index.refresh(&mut f, &|| false).unwrap();
        fs::write(&file, b"123456789").unwrap();
        assert!(index.refresh(&mut f, &|| false).unwrap());
        assert_eq!(
            index.snapshot(&|| false).unwrap().scan.folders[0].logical_bytes,
            9
        );
        let checkpoint = index.checkpoint.clone();
        f.journal.first = f.journal.next + 8;
        assert!(index.refresh(&mut f, &|| false).is_err());
        assert_eq!(index.checkpoint, checkpoint);
        f.journal.first = 0;
        assert!(index.refresh(&mut f, &|| true).is_err());
        assert_eq!(index.checkpoint, checkpoint);
        fs::rename(&scope, f.root.join("old")).unwrap();
        fs::create_dir(&scope).unwrap();
        assert!(index.refresh(&mut f, &|| false).is_err());
        assert_eq!(index.checkpoint, checkpoint);
    }
}
