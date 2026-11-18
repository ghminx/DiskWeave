use crate::{
    metadata::Extra,
    scanner::{FileEntry, Folder, ScanResult},
    snapshot::SavedScan,
};
use serde::{Deserialize, Serialize};
#[path = "usn/buffered.rs"]
pub mod buffered;
#[path = "usn/edit.rs"]
pub mod edit;
#[path = "usn/entries.rs"]
pub mod entries;
#[path = "usn/stream.rs"]
pub mod stream;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::{
    collections::{HashMap, HashSet},
    io,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime},
};

#[derive(Clone, Debug)]
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
    fn now(&self) -> SystemTime {
        SystemTime::now()
    }
}
fn verify_continuity(volume: &str, generation: u64, cursor: i64, j: &Journal) -> io::Result<()> {
    let reason = if j.volume != volume {
        Some("volume")
    } else if j.id != generation {
        Some("generation")
    } else if cursor < j.first.max(j.lowest) {
        Some("retention")
    } else if cursor > j.next {
        Some("future")
    } else {
        None
    };
    if let Some(reason) = reason {
        return Err(invalid(&format!(
            "변경 기록 연속성 오류 ({reason}): cursor={cursor}, first={}, lowest={}, next={}, saved_journal={generation}, current_journal={}. 다시 스캔해 주세요",
            j.first, j.lowest, j.next, j.id
        )));
    }
    Ok(())
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
pub enum UnavailableReason {
    Os(i32),
    Changing,
    Redirected,
}
#[derive(Debug)]
pub struct RedirectedDirectory;
impl std::fmt::Display for RedirectedDirectory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("표시 경로와 실제 위치가 다른 별칭 폴더")
    }
}
impl std::error::Error for RedirectedDirectory {}
impl UnavailableReason {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Os(5) => "접근 권한 없음",
            Self::Os(32) => "다른 프로그램에서 사용 중",
            Self::Os(33) => "파일 잠금 충돌",
            Self::Os(_) => "조회 실패",
            Self::Changing => "계속 변경 중 · 확인 대기",
            Self::Redirected => "실제 위치가 다른 별칭 폴더 · 제외",
        }
    }
    pub fn error(&self) -> io::Error {
        match self {
            Self::Os(code) => io::Error::from_raw_os_error(*code),
            Self::Changing => io::Error::other(self.label()),
            Self::Redirected => io::Error::other(RedirectedDirectory),
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Unavailable {
    pub path: Vec<u16>,
    pub parent: u64,
    pub previous_id: Option<u64>,
    pub directory: bool,
    pub reason: UnavailableReason,
    pub listing: bool,
    pub checked: SystemTime,
}
impl Unavailable {
    pub fn relative(&self) -> PathBuf {
        std::ffi::OsString::from_wide(&self.path).into()
    }
}
fn unavailable_reason(error: &io::Error) -> Option<UnavailableReason> {
    match error.raw_os_error() {
        Some(code @ (5 | 32 | 33)) => Some(UnavailableReason::Os(code)),
        _ if error
            .get_ref()
            .is_some_and(|e| e.is::<RedirectedDirectory>()) =>
        {
            Some(UnavailableReason::Redirected)
        }
        _ => None,
    }
}
fn confirmed_missing(source: &mut impl Source, path: &Path) -> io::Result<bool> {
    let parent = path
        .parent()
        .ok_or_else(|| invalid("조회 대상의 부모가 없습니다"))?;
    let entries = source.list(parent).map_err(|e| context(parent, e))?;
    if entries.iter().any(|p| p.parent() != Some(parent)) {
        return Err(invalid("삭제 재확인의 부모 경로 오류"));
    }
    Ok(!entries.iter().any(|p| p == path))
}
fn context(path: &Path, error: io::Error) -> io::Error {
    io::Error::new(error.kind(), format!("{}: {error}", path.display()))
}
fn inspect_present(
    source: &mut impl Source,
    path: &Path,
    detailed: bool,
) -> io::Result<Option<crate::ntfs::Info>> {
    match source.inspect(path, detailed) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            if confirmed_missing(source, path)? {
                return Ok(None);
            }
            // 삭제·재생성 경합에서 부모가 이름을 확인한 경우에만 한 번 재조회.
            match source.inspect(path, detailed) {
                Err(e)
                    if e.kind() == io::ErrorKind::NotFound && confirmed_missing(source, path)? =>
                {
                    Ok(None)
                }
                value => value.map(Some),
            }
        }
        value => value.map(Some),
    }
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
    #[serde(default)]
    pub unavailable: Vec<Unavailable>,
    #[serde(default)]
    /// 마지막으로 성공한 갱신의 시작 시각. 진행 알림·수신 시각과 분리.
    pub confirmed_after: Option<SystemTime>,
}
fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}
fn conflicting_directories(nodes: &entries::Entries) -> Vec<(u64, PathBuf, PathBuf)> {
    let mut by_id = HashMap::new();
    let mut conflicts = Vec::new();
    for (path, node) in nodes
        .iter()
        .filter(|(_, node)| node.dir() && !node.excluded())
    {
        if let Some(previous) = by_id.insert(node.id, path) {
            conflicts.push((node.id, previous.clone(), path.clone()));
        }
    }
    conflicts
}
fn recheck_directory_parent(
    path: &Path,
    attempted: &mut HashSet<PathBuf>,
    directories: &mut Vec<PathBuf>,
) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| invalid("반영 중 추적 루트가 교체되었습니다"))?;
    if !attempted.insert(path.to_owned()) || attempted.len() > 64 {
        return Err(invalid(&format!(
            "하위 폴더가 계속 교체되어 위치를 확인할 수 없습니다 [{}]",
            path.display()
        )));
    }
    directories.push(parent.to_owned());
    // 부모 확인 전에 원래 ID가 돌아와도 자식 변경을 다시 확인하도록 재예약.
    directories.push(path.to_owned());
    Ok(())
}
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
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
/// 변경 없는 주기의 성공 확인. 전체 파일 목록과 분리한 전송 값.
#[derive(serde::Serialize, serde::Deserialize)]
pub struct Advance {
    pub next: i64,
    pub pending: Vec<u64>,
    pub confirmed_after: Option<SystemTime>,
    pub finished: SystemTime,
}
pub enum WatchUpdate {
    Record(Box<SavedScan>),
    Cursor(Advance),
}
pub struct Index {
    root: PathBuf,
    detailed: bool,
    nodes: entries::Entries,
    unavailable: edit::Issues,
    pub checkpoint: Checkpoint,
    scan: Option<Arc<ScanResult>>,
    projection: Option<Arc<entries::Projection>>,
    started: SystemTime,
    changes: Vec<stream::Change>,
    last_edit: edit::Stats,
}
struct RefreshPlan {
    root: Node,
    directories: HashSet<PathBuf>,
    files: HashSet<PathBuf>,
    retry_ready: HashSet<PathBuf>,
    now: SystemTime,
}
impl Checkpoint {
    pub fn validate(&self, scan: &ScanResult) -> io::Result<()> {
        if !matches!(self.version, 1 | 3 | 4)
            || self.version == 1 && !self.unavailable.is_empty()
            || self.version != 4
                && self
                    .unavailable
                    .iter()
                    .any(|e| e.reason == UnavailableReason::Redirected)
            || scan.error_count != self.unavailable.len() as u64
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
        let root = &scan.folders[0].path;
        let mut parents = HashMap::new();
        for (i, (f, id)) in scan.folders.iter().zip(&self.folders).enumerate() {
            if if i == 0 {
                f.parent.is_some()
            } else {
                f.parent.is_none_or(|p| p >= i)
            } {
                return Err(invalid("추적 폴더 부모 관계 오류"));
            }
            let path = f
                .path
                .strip_prefix(root)
                .map_err(|_| invalid("추적 폴더 범위 오류"))?;
            if parents.insert(path, (*id, i)).is_some() {
                return Err(invalid("중복된 추적 폴더 경로"));
            }
        }
        let mut expected = vec![crate::scanner::FolderAccess::Complete; scan.folders.len()];
        let mut blocked_names: HashMap<usize, HashSet<std::ffi::OsString>> = HashMap::new();
        for e in &self.unavailable {
            let p = e.relative();
            if p.as_os_str().is_empty()
                || e.path.len() > 32767
                || e.path.contains(&0)
                || !p
                    .components()
                    .all(|c| matches!(c, std::path::Component::Normal(_)))
                || !paths.insert(p.clone())
                || matches!(e.reason, UnavailableReason::Os(code) if ![5, 32, 33].contains(&code))
                || matches!(e.reason, UnavailableReason::Changing) && e.listing
                || e.reason == UnavailableReason::Redirected && (e.listing || !e.directory)
                || e.previous_id == Some(0)
                || parents.get(p.parent().unwrap()).map(|v| v.0) != Some(e.parent)
                || e.listing
                    && (!e.directory || parents.get(p.as_path()).map(|v| v.0) != e.previous_id)
                || !e.listing && parents.contains_key(p.as_path())
            {
                return Err(invalid("미수집 경로와 추적 기준점이 일치하지 않습니다"));
            }
            let parent = parents[p.parent().unwrap()].1;
            blocked_names
                .entry(parent)
                .or_default()
                .insert(p.file_name().unwrap().to_owned());
            let mut ancestor = Some(parent);
            while let Some(i) = ancestor {
                if expected[i] == crate::scanner::FolderAccess::Complete {
                    expected[i] = crate::scanner::FolderAccess::Partial;
                }
                ancestor = scan.folders[i].parent;
            }
            if e.listing {
                expected[parents[p.as_path()].1] = if e.reason == UnavailableReason::Os(5) {
                    crate::scanner::FolderAccess::Denied
                } else {
                    crate::scanner::FolderAccess::Unavailable
                };
            }
        }
        let unreadable = |i: usize| {
            matches!(
                expected[i],
                crate::scanner::FolderAccess::Denied | crate::scanner::FolderAccess::Unavailable
            )
        };
        if scan
            .folders
            .iter()
            .any(|f| f.parent.is_some_and(unreadable))
        {
            return Err(invalid("미확인 폴더 안에 수집된 하위 폴더가 있습니다"));
        }
        for f in &scan.files {
            if f.parent >= scan.folders.len()
                || unreadable(f.parent)
                || blocked_names
                    .get(&f.parent)
                    .is_some_and(|names| names.contains(&f.name))
            {
                return Err(invalid("미수집 경로와 정상 파일 목록이 겹칩니다"));
            }
        }
        if scan
            .folders
            .iter()
            .zip(expected)
            .any(|(f, expected)| f.access != expected)
        {
            return Err(invalid("미수집 목록과 폴더 수집 상태가 일치하지 않습니다"));
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
        let first = Node::from_info(source.inspect(root, false).map_err(|e| {
            io::Error::new(
                e.kind(),
                format!("기준 폴더 확인 [{}]: {e}", root.display()),
            )
        })?)?;
        if !first.dir() || first.excluded() {
            return Err(invalid("일반 폴더만 변경 추적할 수 있습니다"));
        }
        let checkpoint = Checkpoint {
            version: 4,
            volume: journal.volume,
            root: first.id,
            journal: journal.id,
            next: journal.next,
            folders: vec![],
            files: vec![],
            excluded: vec![],
            pending: vec![],
            unavailable: vec![],
            confirmed_after: None,
        };
        let mut nodes = entries::Entries::new();
        nodes.insert(PathBuf::new(), first)?;
        let mut unavailable = edit::Issues::new();
        Self::collect(
            root,
            Path::new(""),
            detailed,
            source,
            cancelled,
            &mut nodes,
            &mut unavailable,
        )?;
        let mut index = Self {
            root: root.to_owned(),
            detailed,
            nodes,
            unavailable,
            checkpoint,
            scan: None,
            projection: None,
            started,
            changes: vec![],
            last_edit: edit::Stats::default(),
        };
        index.refresh(source, cancelled)?;
        Ok(index)
    }
    fn exclude(
        relative: &Path,
        reason: UnavailableReason,
        listing: bool,
        now: SystemTime,
        nodes: &mut entries::Entries,
        unavailable: &mut edit::Issues,
    ) -> io::Result<()> {
        if relative.as_os_str().is_empty() {
            return Err(context(relative, reason.error()));
        }
        // 별칭은 논리 경로의 폴더 ID 자체를 신뢰할 수 없으므로 노드 보존 금지.
        let listing = listing && reason != UnavailableReason::Redirected;
        let parent = nodes
            .get(relative.parent().unwrap())
            .ok_or_else(|| invalid("미수집 항목의 부모가 없습니다"))?
            .id;
        let previous = nodes.get(relative).cloned();
        let value = Unavailable {
            path: relative.as_os_str().encode_wide().collect(),
            parent,
            previous_id: previous
                .as_ref()
                .map(|n| n.id)
                .or_else(|| unavailable.get(relative).and_then(|e| e.previous_id)),
            directory: previous.as_ref().is_some_and(Node::dir)
                || reason == UnavailableReason::Redirected,
            reason,
            listing,
            checked: now,
        };
        match &previous {
            None => {}
            Some(node) if !node.dir() => {
                nodes.remove(relative)?;
            }
            Some(_) => nodes.retain(|p, _| !p.starts_with(relative) || listing && p == relative)?,
        }
        unavailable.retain(|p, _| !p.starts_with(relative))?;
        unavailable.insert(relative.to_owned(), value)?;
        Ok(())
    }
    fn collect(
        root: &Path,
        relative: &Path,
        detailed: bool,
        source: &mut impl Source,
        cancelled: &impl Fn() -> bool,
        nodes: &mut entries::Entries,
        unavailable: &mut edit::Issues,
    ) -> io::Result<()> {
        let mut pending = vec![relative.to_owned()];
        while let Some(parent) = pending.pop() {
            crate::output::check(cancelled)?;
            let absolute = root.join(&parent);
            let children = match source.list(&absolute) {
                Ok(v) => v,
                Err(e) if !parent.as_os_str().is_empty() && unavailable_reason(&e).is_some() => {
                    Self::exclude(
                        &parent,
                        unavailable_reason(&e).unwrap(),
                        true,
                        source.now(),
                        nodes,
                        unavailable,
                    )?;
                    continue;
                }
                Err(e) => return Err(context(&absolute, e)),
            };
            for path in children {
                crate::output::check(cancelled)?;
                if path.parent() != Some(absolute.as_path()) {
                    return Err(invalid("열거한 항목의 부모 경로가 다릅니다"));
                }
                let rel = path
                    .strip_prefix(root)
                    .map_err(|_| invalid("추적 범위 밖 항목"))?
                    .to_owned();
                let info = match inspect_present(source, &path, detailed) {
                    Ok(Some(v)) => v,
                    Ok(None) => continue,
                    Err(e) if unavailable_reason(&e).is_some() => {
                        Self::exclude(
                            &rel,
                            unavailable_reason(&e).unwrap(),
                            false,
                            source.now(),
                            nodes,
                            unavailable,
                        )?;
                        continue;
                    }
                    Err(e) => return Err(context(&path, e)),
                };
                let node = Node::from_info(info)?;
                if node.dir() && !node.excluded() {
                    pending.push(rel.clone());
                }
                nodes.insert(rel, node)?;
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
        if record.scan.error_count != state.unavailable.len() as u64 {
            return Err(invalid("누락된 결과에서 변경 추적을 이어갈 수 없습니다"));
        }
        let root = record.scan.folders[0].path.clone();
        let issues: HashMap<_, _> = record
            .scan
            .metadata_issues
            .iter()
            .map(|issue| (issue.path.as_path(), issue.error.to_string()))
            .collect();
        let mut nodes = entries::Entries::new();
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
            )?;
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
            )?;
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
            )?;
        }
        Ok(Self {
            root,
            detailed: record.scan.detailed,
            nodes,
            unavailable: state
                .unavailable
                .iter()
                .map(|e| (e.relative(), e.clone()))
                .collect(),
            checkpoint: {
                let mut current = state.clone();
                current.version = 4;
                current
            },
            scan: Some(Arc::clone(&record.scan)),
            projection: None,
            started: record.started,
            changes: vec![],
            last_edit: edit::Stats::default(),
        })
    }
    fn verify(&self, j: &Journal) -> io::Result<()> {
        self.verify_at(j, self.checkpoint.next)
    }
    fn verify_at(&self, j: &Journal, cursor: i64) -> io::Result<()> {
        verify_continuity(&self.checkpoint.volume, self.checkpoint.journal, cursor, j)
    }
    pub fn refresh(
        &mut self,
        source: &mut impl Source,
        cancelled: &impl Fn() -> bool,
    ) -> io::Result<bool> {
        self.refresh_bounded(source, cancelled, stream::Limits::default().state_bytes / 2)
    }
    #[allow(dead_code)] // 개발용 비교 도구의 변경량 계측.
    pub fn edit_stats(&self) -> edit::Stats {
        self.last_edit
    }
    pub fn refresh_bounded(
        &mut self,
        source: &mut impl Source,
        cancelled: &impl Fn() -> bool,
        undo_bytes: u64,
    ) -> io::Result<bool> {
        crate::output::check(cancelled)?;
        let confirmed_after = source.now();
        let journal = source.journal()?;
        self.verify(&journal)?;
        let root = Node::from_info(source.inspect(&self.root, false)?)?;
        if root.id != self.checkpoint.root || !root.dir() || root.excluded() {
            return Err(invalid(
                "추적 폴더가 이동·교체되었습니다. 폴더를 다시 지정해 주세요",
            ));
        }
        let events = source.read(self.checkpoint.next, journal.next)?;
        let now = source.now();
        let retry_ready: HashSet<_> = self
            .unavailable
            .iter()
            .filter(|(_, e)| {
                now.duration_since(e.checked)
                    .is_ok_and(|d| d >= Duration::from_secs(30))
            })
            .take(64)
            .map(|(p, _)| p.clone())
            .collect();
        if retry_ready.is_empty()
            && events.is_empty()
            && self.checkpoint.pending.is_empty()
            && self.nodes[Path::new("")] == root
        {
            // 유휴 확인마다 전체 파일 목록을 복사하지 않도록 변경 없는 배치 종료.
            let final_journal = source.journal()?;
            self.verify_at(&final_journal, journal.next)?;
            crate::output::check(cancelled)?;
            self.last_edit = edit::Stats::default();
            self.checkpoint.next = journal.next;
            self.checkpoint.confirmed_after = Some(confirmed_after);
            return Ok(false);
        }
        let mut by_id: HashMap<u64, Vec<PathBuf>> = HashMap::new();
        let mut directories = HashSet::new();
        for (path, n) in &self.nodes {
            let paths = by_id.entry(n.id).or_default();
            if n.dir() && !n.excluded() && !paths.is_empty() {
                for p in paths.iter().chain(std::iter::once(path)) {
                    let parent = p
                        .parent()
                        .ok_or_else(|| invalid("추적 루트와 중복된 폴더 ID"))?;
                    directories.insert(parent.to_owned());
                }
            }
            paths.push(path.clone());
        }
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
        for p in &retry_ready {
            if self.unavailable[p].listing && self.nodes.contains_key(p) {
                directories.insert(p.clone());
            } else {
                directories.insert(p.parent().unwrap().to_owned());
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
        let plan = RefreshPlan {
            root,
            directories,
            files,
            retry_ready,
            now,
        };
        let mut edit = edit::Edit::begin(self, undo_bytes, false)?;
        edit.apply_changes(source, cancelled, plan)?;
        let final_root = Node::from_info(source.inspect(&edit.root, false)?)?;
        if final_root.id != edit.checkpoint.root || final_root.excluded() {
            return Err(invalid("반영 중 추적 폴더가 바뀌었습니다"));
        }
        let final_journal = source.journal()?;
        edit.verify_at(&final_journal, journal.next)?;
        crate::output::check(cancelled)?;
        let file_ids: HashSet<_> = edit
            .nodes
            .values()
            .filter(|n| !n.dir() && !n.excluded())
            .map(|n| n.id)
            .collect();
        let mut pending: Vec<_> = pending
            .into_iter()
            .filter(|id| file_ids.contains(id))
            .collect();
        pending.sort_unstable();
        let changed = edit.commit(cancelled, true)?;
        self.checkpoint.next = journal.next;
        self.checkpoint.confirmed_after = Some(confirmed_after);
        self.checkpoint.pending = pending;
        Ok(changed)
    }
    fn apply_changes(
        &mut self,
        source: &mut impl Source,
        cancelled: &impl Fn() -> bool,
        plan: RefreshPlan,
    ) -> io::Result<()> {
        let RefreshPlan {
            root,
            directories,
            files,
            retry_ready,
            now,
        } = plan;
        let mut attempted = HashSet::new();
        let next = &mut self.nodes;
        let unavailable = &mut self.unavailable;
        next.insert(PathBuf::new(), root)?;
        let mut replacements = HashMap::new();
        let mut directories: Vec<_> = directories.into_iter().collect();
        let mut remaining_files = Some(files);
        let mut repaired = HashSet::new();
        let mut directory_rechecks = HashSet::new();
        let mut check_directories = !directories.is_empty();
        // 구조 변경을 먼저 반영하고 남은 파일의 교체만 부모 재확인으로 연결.
        loop {
            directories.sort_by(|a, b| {
                a.components()
                    .count()
                    .cmp(&b.components().count())
                    .then_with(|| a.cmp(b))
            });
            directories.dedup();
            let mut batch = std::mem::take(&mut directories).into_iter();
            let mut rechecking = false;
            while let Some(directory) = batch.next() {
                crate::output::check(cancelled)?;
                if !next
                    .get(&directory)
                    .is_some_and(|n| n.dir() && !n.excluded())
                {
                    continue;
                }
                if unavailable.contains_key(&directory)
                    && (!retry_ready.contains(&directory) || !attempted.insert(directory.clone()))
                {
                    continue;
                }
                let absolute = self.root.join(&directory);
                let observed = if directory.as_os_str().is_empty() {
                    source.inspect(&absolute, false).map(Some)
                } else {
                    inspect_present(source, &absolute, false)
                };
                let info = match observed {
                    Ok(Some(v)) => v,
                    Err(e)
                        if !directory.as_os_str().is_empty()
                            && unavailable_reason(&e).is_some() =>
                    {
                        Self::exclude(
                            &directory,
                            unavailable_reason(&e).unwrap(),
                            false,
                            now,
                            next,
                            unavailable,
                        )?;
                        continue;
                    }
                    Ok(None) => {
                        next.retain(|p, _| !p.starts_with(&directory))?;
                        unavailable.retain(|p, _| !p.starts_with(&directory))?;
                        continue;
                    }
                    Err(e) => return Err(context(&absolute, e)),
                };
                let current = Node::from_info(info)?;
                if current.id != next[&directory].id || !current.dir() || current.excluded() {
                    attempted.remove(&directory);
                    recheck_directory_parent(
                        &directory,
                        &mut directory_rechecks,
                        &mut directories,
                    )?;
                    directories.extend(batch);
                    rechecking = true;
                    break;
                }
                next.insert(directory.clone(), current)?;
                let children = match source.list(&absolute) {
                    Ok(v) => v,
                    Err(e)
                        if !directory.as_os_str().is_empty()
                            && unavailable_reason(&e).is_some() =>
                    {
                        Self::exclude(
                            &directory,
                            unavailable_reason(&e).unwrap(),
                            true,
                            now,
                            next,
                            unavailable,
                        )?;
                        continue;
                    }
                    Err(e) => return Err(context(&absolute, e)),
                };
                let verified = if directory.as_os_str().is_empty() {
                    source.inspect(&absolute, false).map(Some)
                } else {
                    inspect_present(source, &absolute, false)
                };
                let after = match verified {
                    Ok(info) => info.map(Node::from_info).transpose()?,
                    Err(e)
                        if !directory.as_os_str().is_empty()
                            && unavailable_reason(&e).is_some() =>
                    {
                        Self::exclude(
                            &directory,
                            unavailable_reason(&e).unwrap(),
                            false,
                            now,
                            next,
                            unavailable,
                        )?;
                        continue;
                    }
                    Err(e) => return Err(context(&absolute, e)),
                };
                if after.as_ref().is_none_or(|value| {
                    value.id != next[&directory].id || !value.dir() || value.excluded()
                }) {
                    attempted.remove(&directory);
                    recheck_directory_parent(
                        &directory,
                        &mut directory_rechecks,
                        &mut directories,
                    )?;
                    directories.extend(batch);
                    rechecking = true;
                    break;
                }
                next.insert(directory.clone(), after.unwrap())?;
                unavailable.remove(&directory)?;
                let mut seen = HashSet::new();
                for path in children {
                    crate::output::check(cancelled)?;
                    if path.parent() != Some(absolute.as_path()) {
                        return Err(invalid("열거한 경로가 추적 부모와 다릅니다"));
                    }
                    let rel = path
                        .strip_prefix(&self.root)
                        .map_err(|_| invalid("추적 범위 밖 경로"))?
                        .to_owned();
                    seen.insert(rel.clone());
                    if unavailable.contains_key(&rel)
                        && (!retry_ready.contains(&rel) || !attempted.insert(rel.clone()))
                    {
                        continue;
                    }
                    let info = match inspect_present(source, &path, self.detailed) {
                        Ok(Some(v)) => v,
                        Err(e) if unavailable_reason(&e).is_some() => {
                            Self::exclude(
                                &rel,
                                unavailable_reason(&e).unwrap(),
                                false,
                                now,
                                next,
                                unavailable,
                            )?;
                            continue;
                        }
                        Ok(None) => {
                            seen.remove(&rel);
                            continue;
                        }
                        Err(e) => return Err(context(&path, e)),
                    };
                    let node = Node::from_info(info)?;
                    if replacements
                        .get(&rel)
                        .is_some_and(|expected| *expected != (node.id, node.attributes & 0x410))
                    {
                        Self::exclude(
                            &rel,
                            UnavailableReason::Changing,
                            false,
                            now,
                            next,
                            unavailable,
                        )?;
                        continue;
                    }
                    let recovered = unavailable.remove(&rel)?.is_some();
                    let replaced = next.get(&rel).is_none_or(|old| {
                        old.id != node.id || old.attributes & 0x410 != node.attributes & 0x410
                    });
                    if replaced {
                        // 새 경로에는 기존 하위 노드가 없으므로 전체 목록 순회 생략.
                        if next.contains_key(&rel) {
                            next.retain(|p, _| !p.starts_with(&rel))?;
                        }
                        unavailable.retain(|p, _| !p.starts_with(&rel))?;
                    }
                    let descend = (replaced || recovered) && node.dir() && !node.excluded();
                    next.insert(rel.clone(), node)?;
                    if descend {
                        Self::collect(
                            &self.root,
                            &rel,
                            self.detailed,
                            source,
                            cancelled,
                            next,
                            unavailable,
                        )?;
                    }
                }
                let removed: Vec<_> = next
                    .keys()
                    .chain(unavailable.keys())
                    .filter(|p| p.parent() == Some(directory.as_path()) && !seen.contains(*p))
                    .cloned()
                    .collect();
                for p in removed {
                    next.retain(|key, _| !key.starts_with(&p))?;
                    unavailable.retain(|key, _| !key.starts_with(&p))?;
                }
            }
            if rechecking {
                continue;
            }
            if let Some(files) = remaining_files.take() {
                for path in files {
                    crate::output::check(cancelled)?;
                    if !next.contains_key(&path)
                        || next
                            .original(&path)
                            .is_none_or(|old| next[&path].id != old.id)
                    {
                        continue;
                    }
                    let absolute = self.root.join(&path);
                    let info = match inspect_present(source, &absolute, self.detailed) {
                        Ok(Some(v)) => v,
                        Err(e) if unavailable_reason(&e).is_some() => {
                            Self::exclude(
                                &path,
                                unavailable_reason(&e).unwrap(),
                                false,
                                now,
                                next,
                                unavailable,
                            )?;
                            continue;
                        }
                        Ok(None) => {
                            next.retain(|p, _| !p.starts_with(&path))?;
                            unavailable.retain(|p, _| !p.starts_with(&path))?;
                            continue;
                        }
                        Err(e) => return Err(context(&absolute, e)),
                    };
                    let node = Node::from_info(info)?;
                    if node.id != next[&path].id || node.dir() || node.excluded() {
                        replacements.insert(path.clone(), (node.id, node.attributes & 0x410));
                        directories.push(path.parent().unwrap().to_owned());
                        check_directories = true;
                        continue;
                    }
                    next.insert(path, node)?;
                }
            }
            if check_directories {
                // USN 확인 이후 이동한 폴더는 양쪽 부모의 현재 이름으로 재연결.
                let conflicts = conflicting_directories(next);
                let mut this_round = HashSet::new();
                for (id, before, after) in conflicts {
                    if repaired.contains(&id) {
                        return Err(invalid(&format!(
                            "폴더 위치 재확인 후에도 ID가 중복됩니다 [{} / {}]",
                            before.display(),
                            after.display()
                        )));
                    }
                    this_round.insert(id);
                    for path in [before, after] {
                        let parent = path
                            .parent()
                            .ok_or_else(|| invalid("추적 루트와 중복된 폴더 ID"))?;
                        directories.push(parent.to_owned());
                    }
                }
                repaired.extend(this_round);
            }
            if directories.is_empty() {
                break;
            }
        }
        Ok(())
    }
    pub fn snapshot(&mut self, cancelled: &impl Fn() -> bool) -> io::Result<SavedScan> {
        crate::output::check(cancelled)?;
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
            let mut folder_keys = vec![];
            let mut file_keys = vec![];
            let mut directories: Vec<_> = self
                .nodes
                .identified()
                .filter(|(_, _, n)| n.dir() && !n.excluded())
                .collect();
            directories.sort_by_key(|(p, _, _)| p.components().count());
            for (p, key, n) in directories {
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
                folder_keys.push(key);
                scan.folders.push(Folder {
                    access: Default::default(),
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
            for (p, key, n) in self.nodes.identified() {
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
                file_keys.push(key);
            }
            for (p, e) in &self.unavailable {
                let parent = *folders
                    .get(p.parent().unwrap())
                    .ok_or_else(|| invalid("미수집 경로의 분석 부모가 없습니다"))?;
                scan.folders[parent].access = crate::scanner::FolderAccess::Partial;
                if e.listing {
                    let id = *folders
                        .get(p)
                        .ok_or_else(|| invalid("미수집 폴더 ID가 없습니다"))?;
                    scan.folders[id].access = if e.reason == UnavailableReason::Os(5) {
                        crate::scanner::FolderAccess::Denied
                    } else {
                        crate::scanner::FolderAccess::Unavailable
                    };
                }
                scan.error_count += 1;
                if scan.issues.len() < 20 {
                    scan.issues.push(crate::scanner::ScanIssue {
                        path: self.root.join(p),
                        error: e.reason.error(),
                    });
                }
            }
            crate::scanner::finish(&mut scan)?;
            // 저장 자료의 행 순서는 다를 수 있으므로 실제 구성 순서까지 확인 후 재사용.
            if self
                .projection
                .as_ref()
                .is_none_or(|p| !p.matches_rows(&folder_keys, &file_keys))
            {
                self.projection = Some(Arc::new(self.nodes.project(
                    folder_keys,
                    file_keys,
                    cancelled,
                )?));
            }
            self.checkpoint.folders = refs;
            self.checkpoint.files = files;
            self.checkpoint.excluded = excluded;
            self.checkpoint.unavailable = self.unavailable.values().cloned().collect();
            self.checkpoint.version = 4;
            self.scan = Some(Arc::new(scan));
        }
        let mut record = SavedScan::new(
            Arc::clone(self.scan.as_ref().unwrap()),
            self.started,
            SystemTime::now(),
            Duration::ZERO,
        );
        if self.projection.is_none() {
            self.projection = Some(Arc::new(self.nodes.project_scan(&record.scan, cancelled)?));
        }
        record.entries = self.projection.clone();
        record.tracking = Some(self.checkpoint.clone());
        Ok(record)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::{
        fs,
        time::{SystemTime, UNIX_EPOCH},
    };
    struct Fixture {
        root: PathBuf,
        journal: Journal,
        events: Vec<Event>,
        blocked: Option<PathBuf>,
        failures: HashMap<PathBuf, i32>,
        list_failures: HashMap<PathBuf, i32>,
        clock: SystemTime,
        queries: Vec<PathBuf>,
        vanish: Option<PathBuf>,
        replace_after: Option<(PathBuf, usize)>,
        not_found_once: Option<PathBuf>,
        advance_on_journal: Option<Duration>,
        replace_on_read: Vec<(PathBuf, u64, u64)>,
        list_budget: HashMap<PathBuf, usize>,
        move_on_list: Option<(PathBuf, PathBuf, PathBuf)>,
        cancel_on_list: Option<(PathBuf, Arc<std::sync::atomic::AtomicBool>)>,
        identity_alias: Option<(PathBuf, u64)>,
        replace_directory_on_inspect: Option<PathBuf>,
        replace_directory_on_list: Option<PathBuf>,
        restore_directory_on_list: Option<(PathBuf, PathBuf, PathBuf)>,
        redirected: HashSet<PathBuf>,
        redirect_on_list: Option<PathBuf>,
        journal_on_read: Option<Journal>,
        rolling_journal: bool,
    }
    impl Source for Fixture {
        fn journal(&mut self) -> io::Result<Journal> {
            if let Some(elapsed) = self.advance_on_journal.take() {
                self.clock += elapsed;
            }
            Ok(self.journal.clone())
        }
        fn read(&mut self, start: i64, end: i64) -> io::Result<Vec<Event>> {
            verify_continuity(&self.journal.volume, self.journal.id, start, &self.journal)?;
            if let Some(journal) = self.journal_on_read.take() {
                self.journal = journal;
            }
            let events = self
                .events
                .iter()
                .filter(|e| e.usn >= start && e.usn < end)
                .cloned()
                .collect();
            for (i, (path, old, parent)) in std::mem::take(&mut self.replace_on_read)
                .into_iter()
                .enumerate()
            {
                fs::rename(&path, self.root.join(format!("previous-{i}")))?;
                fs::write(&path, b"12345")?;
                let new = self.id(&path);
                self.event(old, parent, 0x80000200);
                self.event(new, parent, 0x80000100);
            }
            Ok(events)
        }
        fn now(&self) -> SystemTime {
            self.clock
        }
        fn inspect(&mut self, path: &Path, detailed: bool) -> io::Result<crate::ntfs::Info> {
            self.queries.push(path.to_owned());
            if self.redirected.contains(path) {
                return Err(io::Error::other(RedirectedDirectory));
            }
            if self.replace_directory_on_inspect.as_deref() == Some(path) {
                self.replace_directory_on_inspect = None;
                fs::rename(path, self.root.join("previous-directory"))?;
                fs::create_dir(path)?;
                fs::write(path.join("fresh"), b"1234567")?;
            }
            if self.not_found_once.as_deref() == Some(path) {
                self.not_found_once = None;
                return Err(io::Error::from_raw_os_error(2));
            }
            if let Some((target, remaining)) = &mut self.replace_after {
                if path == target {
                    if *remaining == 0 {
                        fs::rename(path, self.root.join("raced-previous.bin"))?;
                        fs::write(path, b"999")?;
                        self.replace_after = None;
                    } else {
                        *remaining -= 1;
                    }
                }
            }
            if self.vanish.as_deref() == Some(path) {
                self.vanish = None;
                fs::remove_file(path)?;
                return Err(io::Error::from_raw_os_error(2));
            }
            if let Some(code) = self.failures.get(path) {
                return Err(io::Error::from_raw_os_error(*code));
            }
            if self.blocked.as_deref() == Some(path) {
                return Err(io::Error::from_raw_os_error(32));
            }
            let mut info = crate::ntfs_native::inspect_file(path, detailed, &self.journal.volume)?;
            if let Some((target, id)) = &self.identity_alias {
                if path == target {
                    info.reference = Some(crate::ntfs::Id::V2(*id));
                }
            }
            if self.rolling_journal {
                self.clock += Duration::from_secs(1);
                let Some(crate::ntfs::Id::V2(id)) = info.reference else {
                    unreachable!()
                };
                self.event(id, id, 0x80000001);
                self.journal.first = (self.journal.next - 32).max(0);
            }
            Ok(info)
        }
        fn list(&mut self, path: &Path) -> io::Result<Vec<PathBuf>> {
            if self.redirect_on_list.as_deref() == Some(path) {
                self.redirect_on_list = None;
                self.redirected.insert(path.to_owned());
                return Err(io::Error::other(RedirectedDirectory));
            }
            if self
                .restore_directory_on_list
                .as_ref()
                .is_some_and(|(parent, _, _)| parent == path)
            {
                let (_, child, previous) = self.restore_directory_on_list.take().unwrap();
                fs::rename(&child, self.root.join("interim-directory"))?;
                fs::rename(previous, child)?;
            }
            if self.replace_directory_on_list.as_deref() == Some(path) {
                self.replace_directory_on_list = None;
                self.replace_directory_on_inspect = Some(path.to_owned());
                self.inspect(path, false)?;
            }
            if let Some((target, cancel)) = &self.cancel_on_list {
                if path == target {
                    cancel.store(true, std::sync::atomic::Ordering::Relaxed);
                }
            }
            if self
                .move_on_list
                .as_ref()
                .is_some_and(|(parent, _, _)| parent == path)
            {
                let (_, old, new) = self.move_on_list.take().unwrap();
                let id = self.id(&old);
                let before = self.id(old.parent().unwrap());
                let after = self.id(new.parent().unwrap());
                fs::rename(old, new)?;
                self.event(id, before, 0x1000);
                self.event(id, after, 0x80002000);
            }
            if let Some(remaining) = self.list_budget.get_mut(path) {
                if *remaining == 0 {
                    return Err(io::Error::other("시험용 폴더 조회 예산 초과"));
                }
                *remaining -= 1;
            }
            if let Some(code) = self.list_failures.get(path) {
                return Err(io::Error::from_raw_os_error(*code));
            }
            fs::read_dir(path)?.map(|e| e.map(|e| e.path())).collect()
        }
    }
    impl Fixture {
        fn new() -> Self {
            static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let root = std::env::temp_dir().join(format!(
                "diskweave-usn-{}-{}-{}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_nanos(),
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
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
                blocked: None,
                failures: HashMap::new(),
                list_failures: HashMap::new(),
                clock: SystemTime::now(),
                queries: Vec::new(),
                vanish: None,
                replace_after: None,
                not_found_once: None,
                advance_on_journal: None,
                replace_on_read: vec![],
                list_budget: HashMap::new(),
                move_on_list: None,
                cancel_on_list: None,
                identity_alias: None,
                replace_directory_on_inspect: None,
                replace_directory_on_list: None,
                restore_directory_on_list: None,
                redirected: HashSet::new(),
                redirect_on_list: None,
                journal_on_read: None,
                rolling_journal: false,
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
    fn replacements_in_two_parents_finish_with_one_listing_per_parent_and_exact_metadata() {
        let mut source = Fixture::new();
        let scope = source.root.join("scope");
        for name in ["a", "b"] {
            fs::create_dir(scope.join(name)).unwrap();
        }
        let mut paths = vec![];
        for i in 0..64 {
            let parent = scope.join(if i % 2 == 0 { "a" } else { "b" });
            let path = parent.join(format!("file-{i}"));
            fs::write(&path, b"123").unwrap();
            paths.push((path.clone(), source.id(&path), source.id(&parent)));
        }
        let mut index = Index::begin(&scope, true, &mut source, &|| false).unwrap();
        for (_, id, parent) in &paths {
            source.event(*id, *parent, 0x80000001);
        }
        source.replace_on_read = paths;
        for name in ["a", "b"] {
            source.list_budget.insert(scope.join(name), 1);
        }
        assert!(index.refresh(&mut source, &|| false).unwrap());
        let record = index.snapshot(&|| false).unwrap();
        assert_eq!(record.scan.folders[0].logical_bytes, 320);
        assert_eq!(record.scan.files.len(), 64);
        let expected = crate::scanner::scan_with_options(&scope, true, || false, |_| {}).unwrap();
        let rows = |scan: &ScanResult| -> BTreeMap<_, _> {
            scan.files
                .iter()
                .map(|f| {
                    (
                        scan.folders[f.parent].path.join(&f.name),
                        (
                            f.logical_bytes,
                            f.created,
                            f.modified,
                            f.attributes,
                            f.extra,
                        ),
                    )
                })
                .collect()
        };
        assert_eq!(rows(&record.scan), rows(&expected));
        record
            .tracking
            .as_ref()
            .unwrap()
            .validate(&record.scan)
            .unwrap();
    }
    #[test]
    fn deletion_between_enumeration_and_inspection_is_confirmed_before_ignoring() {
        let mut f = Fixture::new();
        let root = f.root.clone();
        let path = root.join("gone.bin");
        fs::write(&path, b"1").unwrap();
        f.vanish = Some(path);
        let mut index = Index::begin(&root, true, &mut f, &|| false).unwrap();
        assert_eq!(index.snapshot(&|| false).unwrap().scan.error_count, 0);
        let bad = root.join("still-present.bin");
        fs::write(&bad, b"1").unwrap();
        f.failures.insert(bad, 2);
        assert!(Index::begin(&root, true, &mut f, &|| false).is_err());
    }
    #[test]
    fn folder_moving_after_the_journal_window_is_not_published_at_two_paths() {
        let mut f = Fixture::new();
        let root = f.root.clone();
        let old = root.join("old");
        let destination = root.join("scope");
        let new = destination.join("new");
        fs::create_dir(&old).unwrap();
        fs::write(old.join("payload"), b"123").unwrap();
        let mut index = Index::begin(&root, true, &mut f, &|| false).unwrap();
        let mut sender = stream::Sender::new(17, Default::default());
        let mut receiver = stream::Receiver::new(root.clone(), true, Default::default());
        let mut baseline = Vec::new();
        sender.write(&mut baseline, &mut index, &|| false).unwrap();
        receiver
            .receive(&mut baseline.as_slice(), &|| false)
            .unwrap();
        let id = f.id(&old);
        let trigger = destination.join("trigger");
        fs::write(&trigger, b"x").unwrap();
        let file = f.id(&trigger);
        let parent = f.id(&destination);
        f.event(file, parent, 0x80000100);
        f.move_on_list = Some((destination, old.clone(), new.clone()));
        index.refresh(&mut f, &|| false).unwrap();
        let result = index.snapshot(&|| false).unwrap();
        result
            .tracking
            .as_ref()
            .unwrap()
            .validate(&result.scan)
            .unwrap();
        assert!(!result.scan.folders.iter().any(|folder| folder.path == old));
        let position = result
            .scan
            .folders
            .iter()
            .position(|folder| folder.path == new)
            .unwrap();
        assert_eq!(result.tracking.as_ref().unwrap().folders[position], id);
        assert_eq!(result.scan.folders[0].logical_bytes, 4);
        let reference = crate::scanner::scan_with_options(&root, true, || false, |_| {}).unwrap();
        let files = |scan: &ScanResult| {
            scan.files
                .iter()
                .map(|file| {
                    (
                        scan.folders[file.parent].path.join(&file.name),
                        (
                            file.logical_bytes,
                            file.created,
                            file.modified,
                            file.attributes,
                            file.extra,
                        ),
                    )
                })
                .collect::<BTreeMap<_, _>>()
        };
        let folders = |scan: &ScanResult| {
            scan.folders
                .iter()
                .map(|folder| {
                    (
                        folder.path.clone(),
                        (
                            folder.access,
                            folder.logical_bytes,
                            folder.file_count,
                            folder.folder_count,
                            folder.created,
                            folder.modified,
                            folder.attributes,
                            folder.allocation,
                        ),
                    )
                })
                .collect::<BTreeMap<_, _>>()
        };
        assert_eq!(files(&result.scan), files(&reference));
        assert_eq!(folders(&result.scan), folders(&reference));
        let mut delta = Vec::new();
        sender.write(&mut delta, &mut index, &|| false).unwrap();
        receiver.receive(&mut delta.as_slice(), &|| false).unwrap();
        let received = receiver.snapshot(&|| false).unwrap();
        assert_eq!(files(&received.scan), files(&reference));
        assert_eq!(folders(&received.scan), folders(&reference));
        assert_eq!(received.tracking, result.tracking);
    }

    #[test]
    fn cancelling_folder_reconciliation_preserves_the_previous_result_and_checkpoint() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let mut f = Fixture::new();
        let root = f.root.clone();
        let old = root.join("old");
        let destination = root.join("scope");
        fs::create_dir(&old).unwrap();
        fs::write(old.join("payload"), b"123").unwrap();
        let mut index = Index::begin(&root, true, &mut f, &|| false).unwrap();
        let before = index.snapshot(&|| false).unwrap();
        let trigger = destination.join("trigger");
        fs::write(&trigger, b"x").unwrap();
        let id = f.id(&trigger);
        let parent = f.id(&destination);
        f.event(id, parent, 0x80000100);
        f.move_on_list = Some((destination.clone(), old, destination.join("new")));
        let cancel = Arc::new(AtomicBool::new(false));
        f.cancel_on_list = Some((root, Arc::clone(&cancel)));
        assert!(index
            .refresh(&mut f, &|| cancel.load(Ordering::Relaxed))
            .is_err());
        let after = index.snapshot(&|| false).unwrap();
        assert!(Arc::ptr_eq(&before.scan, &after.scan));
        assert_eq!(before.tracking, after.tracking);
    }

    #[test]
    fn a_persistent_folder_identity_conflict_stops_without_committing_a_bad_result() {
        let mut f = Fixture::new();
        let root = f.root.clone();
        let a = root.join("a");
        let b = root.join("b");
        fs::create_dir(&a).unwrap();
        fs::create_dir(&b).unwrap();
        let mut index = Index::begin(&root, true, &mut f, &|| false).unwrap();
        let before = index.snapshot(&|| false).unwrap();
        let id = f.id(&a);
        let parent = f.id(&root);
        f.event(id, parent, 0x80002000);
        f.identity_alias = Some((b, id));
        let error = index.refresh(&mut f, &|| false).unwrap_err();
        assert!(error.to_string().contains("재확인 후에도"));
        let after = index.snapshot(&|| false).unwrap();
        assert!(Arc::ptr_eq(&before.scan, &after.scan));
        assert_eq!(before.tracking, after.tracking);
    }

    #[test]
    fn a_case_only_folder_rename_after_the_window_keeps_one_identity_and_catches_up() {
        let mut f = Fixture::new();
        let root = f.root.clone();
        let old = root.join("mixed");
        let renamed = root.join("MIXED");
        fs::create_dir(&old).unwrap();
        fs::write(old.join("payload"), b"123").unwrap();
        let mut index = Index::begin(&root, true, &mut f, &|| false).unwrap();
        let id = f.id(&old);
        let trigger = old.join("trigger");
        fs::write(&trigger, b"x").unwrap();
        let file = f.id(&trigger);
        f.event(file, id, 0x80000100);
        f.move_on_list = Some((old.clone(), old.clone(), renamed.clone()));
        index.refresh(&mut f, &|| false).unwrap();
        let first = index.snapshot(&|| false).unwrap();
        first
            .tracking
            .as_ref()
            .unwrap()
            .validate(&first.scan)
            .unwrap();
        index.refresh(&mut f, &|| false).unwrap();
        let after = index.snapshot(&|| false).unwrap();
        after
            .tracking
            .as_ref()
            .unwrap()
            .validate(&after.scan)
            .unwrap();
        assert_eq!(after.scan.folders[0].logical_bytes, 4);
        assert!(!after.scan.folders.iter().any(|d| d.path == old));
        let position = after
            .scan
            .folders
            .iter()
            .position(|d| d.path == renamed)
            .unwrap();
        assert_eq!(after.tracking.unwrap().folders[position], id);
    }

    #[test]
    fn a_replaced_child_directory_is_reconnected_through_its_verified_parent() {
        let mut f = Fixture::new();
        let root = f.root.clone();
        let child = root.join("scope");
        fs::write(child.join("old"), b"123").unwrap();
        let mut index = Index::begin(&root, true, &mut f, &|| false).unwrap();
        let previous = f.id(&child);
        let trigger = child.join("trigger");
        fs::write(&trigger, b"x").unwrap();
        let id = f.id(&trigger);
        f.event(id, previous, 0x80000100);
        f.replace_directory_on_inspect = Some(child.clone());
        index.refresh(&mut f, &|| false).unwrap();
        let record = index.snapshot(&|| false).unwrap();
        record
            .tracking
            .as_ref()
            .unwrap()
            .validate(&record.scan)
            .unwrap();
        let current = record
            .scan
            .folders
            .iter()
            .position(|d| d.path == child)
            .unwrap();
        assert_ne!(record.tracking.as_ref().unwrap().folders[current], previous);
        assert_eq!(record.scan.folders[current].logical_bytes, 7);
        assert_eq!(record.scan.folders[0].logical_bytes, 11);
    }

    #[test]
    fn a_child_replaced_while_listing_does_not_keep_its_previous_identity() {
        let mut f = Fixture::new();
        let root = f.root.clone();
        let child = root.join("scope");
        fs::write(child.join("old"), b"123").unwrap();
        let mut index = Index::begin(&root, true, &mut f, &|| false).unwrap();
        let previous = f.id(&child);
        let trigger = child.join("trigger");
        fs::write(&trigger, b"x").unwrap();
        let id = f.id(&trigger);
        f.event(id, previous, 0x80000100);
        f.replace_directory_on_list = Some(child.clone());
        index.refresh(&mut f, &|| false).unwrap();
        let record = index.snapshot(&|| false).unwrap();
        record
            .tracking
            .as_ref()
            .unwrap()
            .validate(&record.scan)
            .unwrap();
        let current = record
            .scan
            .folders
            .iter()
            .position(|d| d.path == child)
            .unwrap();
        assert_ne!(record.tracking.as_ref().unwrap().folders[current], previous);
        assert_eq!(record.scan.folders[0].logical_bytes, 11);
    }

    #[test]
    fn a_directory_returning_before_parent_recheck_still_receives_its_child_changes() {
        let mut f = Fixture::new();
        let root = f.root.clone();
        let child = root.join("scope");
        fs::write(child.join("old"), b"123").unwrap();
        let mut index = Index::begin(&root, true, &mut f, &|| false).unwrap();
        let original = f.id(&child);
        let trigger = child.join("trigger");
        fs::write(&trigger, b"x").unwrap();
        let id = f.id(&trigger);
        f.event(id, original, 0x80000100);
        f.replace_directory_on_inspect = Some(child.clone());
        f.restore_directory_on_list =
            Some((root.clone(), child.clone(), root.join("previous-directory")));
        index.refresh(&mut f, &|| false).unwrap();
        let record = index.snapshot(&|| false).unwrap();
        record
            .tracking
            .as_ref()
            .unwrap()
            .validate(&record.scan)
            .unwrap();
        let folder = record
            .scan
            .folders
            .iter()
            .position(|d| d.path == child)
            .unwrap();
        assert_eq!(record.tracking.as_ref().unwrap().folders[folder], original);
        assert_eq!(record.scan.folders[folder].logical_bytes, 4);
        assert_eq!(record.scan.folders[0].logical_bytes, 11);
    }

    #[test]
    fn a_previously_unreadable_directory_can_recover_after_its_identity_changes() {
        let mut f = Fixture::new();
        let root = f.root.clone();
        let child = root.join("scope");
        fs::write(child.join("old"), b"123").unwrap();
        f.list_failures.insert(child.clone(), 5);
        let mut index = Index::begin(&root, true, &mut f, &|| false).unwrap();
        assert_eq!(index.snapshot(&|| false).unwrap().scan.error_count, 1);
        f.list_failures.clear();
        fs::rename(&child, root.join("previous")).unwrap();
        fs::create_dir(&child).unwrap();
        fs::write(child.join("fresh"), b"1234567").unwrap();
        f.clock += Duration::from_secs(31);
        index.refresh(&mut f, &|| false).unwrap();
        let record = index.snapshot(&|| false).unwrap();
        record
            .tracking
            .as_ref()
            .unwrap()
            .validate(&record.scan)
            .unwrap();
        assert_eq!(record.scan.error_count, 0);
        assert_eq!(record.scan.folders[0].logical_bytes, 10);
        assert!(record.tracking.unwrap().unavailable.is_empty());
    }

    #[test]
    fn unmarked_alias_directories_are_reported_without_following_or_double_counting_them() {
        let mut f = Fixture::new();
        let root = f.root.clone();
        let alias = root.join("scope");
        fs::write(root.join("known"), b"x").unwrap();
        fs::write(alias.join("payload"), b"123").unwrap();
        f.redirected.insert(alias.clone());
        let mut index = Index::begin(&root, true, &mut f, &|| false).unwrap();
        let record = index.snapshot(&|| false).unwrap();
        let state = record.tracking.as_ref().unwrap();
        state.validate(&record.scan).unwrap();
        assert_eq!(record.scan.folders[0].logical_bytes, 1);
        assert_eq!(record.scan.error_count, 1);
        assert!(state.unavailable[0].reason.label().contains("별칭"));
        assert!(state.unavailable[0].directory);
        let mut old_schema = state.clone();
        old_schema.version = 3;
        assert!(old_schema.validate(&record.scan).is_err());
        let mut sender = stream::Sender::new(19, Default::default());
        let mut receiver = stream::Receiver::new(root.clone(), true, Default::default());
        let mut wire = Vec::new();
        sender.write(&mut wire, &mut index, &|| false).unwrap();
        receiver.receive(&mut wire.as_slice(), &|| false).unwrap();
        let transferred = receiver.snapshot(&|| false).unwrap();
        assert_eq!(transferred.tracking, record.tracking);
        assert_eq!(transferred.scan.folders[0].logical_bytes, 1);
        let mut bytes = Vec::new();
        crate::snapshot::write_to(&record, &mut bytes, || false).unwrap();
        let restored = crate::snapshot::read_from(bytes.as_slice(), || false).unwrap();
        assert_eq!(restored.tracking, record.tracking);
        f.redirected.clear();
        f.clock += Duration::from_secs(31);
        index.refresh(&mut f, &|| false).unwrap();
        let recovered = index.snapshot(&|| false).unwrap();
        recovered
            .tracking
            .as_ref()
            .unwrap()
            .validate(&recovered.scan)
            .unwrap();
        assert_eq!(recovered.scan.error_count, 0);
        assert_eq!(recovered.scan.folders[0].logical_bytes, 4);
    }

    #[test]
    fn an_alias_detected_when_listing_drops_its_unverified_directory_node() {
        let mut f = Fixture::new();
        let root = f.root.clone();
        let alias = root.join("scope");
        fs::write(alias.join("payload"), b"123").unwrap();
        f.redirect_on_list = Some(alias.clone());
        let mut index = Index::begin(&root, true, &mut f, &|| false).unwrap();
        let record = index.snapshot(&|| false).unwrap();
        record
            .tracking
            .as_ref()
            .unwrap()
            .validate(&record.scan)
            .unwrap();
        assert_eq!(record.scan.folders.len(), 1);
        assert_eq!(record.scan.folders[0].logical_bytes, 0);
        assert_eq!(record.scan.error_count, 1);
        let excluded = &record.tracking.as_ref().unwrap().unavailable[0];
        assert_eq!(excluded.relative(), Path::new("scope"));
        assert!(!excluded.listing);
        assert_eq!(excluded.reason, UnavailableReason::Redirected);
    }

    #[test]
    fn denied_subtree_round_trips_and_recovers_without_claiming_unknown_bytes() {
        let mut f = Fixture::new();
        let scope = f.root.join("scope");
        fs::write(scope.join("data.bin"), b"12345").unwrap();
        fs::write(f.root.join("ok.bin"), b"123").unwrap();
        f.list_failures.insert(scope.clone(), 5);
        let root = f.root.clone();
        let mut index = Index::begin(&root, true, &mut f, &|| false).unwrap();
        let record = index.snapshot(&|| false).unwrap();
        assert_eq!(record.scan.error_count, 1);
        assert_eq!(record.scan.folders[0].logical_bytes, 3);
        assert_eq!(
            record.scan.folders[record.scan.folder_index(Path::new("scope")).unwrap()].access,
            crate::scanner::FolderAccess::Denied
        );
        let mut bytes = Vec::new();
        crate::snapshot::write_to(&record, &mut bytes, || false).unwrap();
        let mut corrupt: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        corrupt["folders"][0]["access"] = serde_json::json!("Complete");
        assert!(crate::snapshot::read_from(
            serde_json::to_vec(&corrupt).unwrap().as_slice(),
            || false
        )
        .is_err());
        let mut record = crate::snapshot::read_from(bytes.as_slice(), || false).unwrap();
        let hidden = record.scan.folder_index(Path::new("scope")).unwrap();
        Arc::get_mut(&mut record.scan).unwrap().files[0].parent = hidden;
        assert!(record
            .tracking
            .as_ref()
            .unwrap()
            .validate(&record.scan)
            .is_err());
        Arc::get_mut(&mut record.scan).unwrap().files[0].parent = 0;
        let mut resumed = Index::restore(&record).unwrap();
        let mut bad = record.tracking.clone().unwrap();
        bad.unavailable[0].parent = 0;
        assert!(bad.validate(&record.scan).is_err());
        f.list_failures.clear();
        f.clock += Duration::from_secs(30);
        assert!(resumed.refresh(&mut f, &|| false).unwrap());
        let restored = resumed.snapshot(&|| false).unwrap();
        assert_eq!(restored.scan.error_count, 0);
        assert_eq!(restored.scan.folders[0].logical_bytes, 8);
        restored
            .tracking
            .as_ref()
            .unwrap()
            .validate(&restored.scan)
            .unwrap();
    }
    #[test]
    fn successful_confirmation_covers_only_cycles_started_before_the_journal_boundary() {
        let mut f = Fixture::new();
        let scope = f.root.join("scope");
        let mut index = Index::begin(&scope, true, &mut f, &|| false).unwrap();
        let started = f.clock;
        f.advance_on_journal = Some(Duration::from_secs(1));
        assert!(!index.refresh(&mut f, &|| false).unwrap());
        let record = index.snapshot(&|| false).unwrap();
        assert_eq!(
            record.tracking.as_ref().unwrap().confirmed_after,
            Some(started)
        );
        let before = record.tracking.clone();
        assert!(index.refresh(&mut f, &|| true).is_err());
        assert_eq!(index.snapshot(&|| false).unwrap().tracking, before);
        let mut encoded = Vec::new();
        crate::snapshot::write_to(&record, &mut encoded, || false).unwrap();
        let mut legacy: serde_json::Value = serde_json::from_slice(&encoded).unwrap();
        legacy["tracking"]
            .as_object_mut()
            .unwrap()
            .remove("confirmed_after");
        let restored =
            crate::snapshot::read_from(serde_json::to_vec(&legacy).unwrap().as_slice(), || false)
                .unwrap();
        assert_eq!(restored.tracking.unwrap().confirmed_after, None);
    }
    #[test]
    fn a_file_update_and_parent_move_out_in_one_batch_remove_the_subtree() {
        let mut f = Fixture::new();
        let scope = f.root.join("scope");
        let folder = scope.join("sub");
        fs::create_dir(&folder).unwrap();
        let file = folder.join("data.bin");
        fs::write(&file, b"123").unwrap();
        let root_id = f.id(&scope);
        let folder_id = f.id(&folder);
        let file_id = f.id(&file);
        let outside_parent = f.id(&f.root.clone());
        let mut index = Index::begin(&scope, true, &mut f, &|| false).unwrap();
        fs::write(&file, b"12345").unwrap();
        fs::rename(&folder, f.root.join("outside")).unwrap();
        f.event(file_id, folder_id, 0x80000003);
        f.event(folder_id, root_id, 0x80001000);
        f.event(folder_id, outside_parent, 0x80002000);
        assert!(index.refresh(&mut f, &|| false).unwrap());
        let record = index.snapshot(&|| false).unwrap();
        assert_eq!(record.scan.folders[0].logical_bytes, 0);
        assert_eq!(record.scan.folders.len(), 1);
        assert!(record.scan.files.is_empty());
        record
            .tracking
            .as_ref()
            .unwrap()
            .validate(&record.scan)
            .unwrap();
    }
    #[test]
    fn transient_not_found_is_reconfirmed_once_when_the_parent_still_lists_the_name() {
        let mut f = Fixture::new();
        let scope = f.root.join("scope");
        let file = scope.join("racing.bin");
        fs::write(&file, b"123").unwrap();
        f.not_found_once = Some(file.clone());
        let mut index = Index::begin(&scope, true, &mut f, &|| false).unwrap();
        assert_eq!(
            index.snapshot(&|| false).unwrap().scan.folders[0].logical_bytes,
            3
        );
        assert_eq!(f.queries.iter().filter(|p| **p == file).count(), 2);
    }
    #[test]
    fn atomic_file_replacement_is_reconfirmed_without_reusing_the_old_identity() {
        let mut f = Fixture::new();
        let scope = f.root.join("scope");
        let file = scope.join("saved.bin");
        fs::write(&file, b"12345").unwrap();
        let parent = f.id(&scope);
        let old = f.id(&file);
        let mut index = Index::begin(&scope, true, &mut f, &|| false).unwrap();
        let before = index.snapshot(&|| false).unwrap();
        fs::rename(&file, f.root.join("previous.bin")).unwrap();
        fs::write(&file, b"123").unwrap();
        let current = f.id(&file);
        assert_ne!(old, current);
        f.event(old, parent, 0x80000001);
        f.list_failures.insert(scope.clone(), 5);
        assert!(index.refresh(&mut f, &|| false).is_err());
        let failed = index.snapshot(&|| false).unwrap();
        assert_eq!(failed.tracking, before.tracking);
        assert!(Arc::ptr_eq(&failed.scan, &before.scan));
        f.list_failures.clear();
        assert!(index.refresh(&mut f, &|| false).unwrap());
        let record = index.snapshot(&|| false).unwrap();
        assert_eq!(record.scan.folders[0].logical_bytes, 3);
        assert_eq!(record.tracking.as_ref().unwrap().files, vec![current]);
        assert_eq!(record.scan.error_count, 0);
        record
            .tracking
            .as_ref()
            .unwrap()
            .validate(&record.scan)
            .unwrap();
    }
    #[test]
    fn replacement_type_changes_recollect_only_the_confirmed_subtree() {
        let mut f = Fixture::new();
        let scope = f.root.join("scope");
        let file = scope.join("entry");
        fs::write(&file, b"12345").unwrap();
        fs::write(scope.join("entry-other"), b"123").unwrap();
        let parent = f.id(&scope);
        let old = f.id(&file);
        let mut index = Index::begin(&scope, true, &mut f, &|| false).unwrap();
        fs::rename(&file, f.root.join("old-file")).unwrap();
        fs::create_dir(&file).unwrap();
        fs::write(file.join("payload"), b"1234567").unwrap();
        let directory = f.id(&file);
        f.event(old, parent, 0x80000001);
        index.refresh(&mut f, &|| false).unwrap();
        let record = index.snapshot(&|| false).unwrap();
        assert_eq!(
            (
                record.scan.folders[0].logical_bytes,
                record.scan.folders[0].folder_count
            ),
            (10, 1)
        );
        fs::rename(&file, f.root.join("old-folder")).unwrap();
        fs::write(&file, b"12").unwrap();
        f.event(directory, parent, 0x80000001);
        index.refresh(&mut f, &|| false).unwrap();
        let record = index.snapshot(&|| false).unwrap();
        assert_eq!(
            (
                record.scan.folders[0].logical_bytes,
                record.scan.folders[0].folder_count
            ),
            (5, 0)
        );
        assert_eq!(record.scan.files.len(), 2);
        record
            .tracking
            .as_ref()
            .unwrap()
            .validate(&record.scan)
            .unwrap();
    }
    #[test]
    fn repeated_replacement_is_deferred_and_recovers_after_a_confirmed_retry() {
        let mut f = Fixture::new();
        let scope = f.root.join("scope");
        let file = scope.join("saved.bin");
        fs::write(&file, b"12345").unwrap();
        let parent = f.id(&scope);
        let old = f.id(&file);
        let mut index = Index::begin(&scope, true, &mut f, &|| false).unwrap();
        let before = index.snapshot(&|| false).unwrap();
        fs::rename(&file, f.root.join("previous.bin")).unwrap();
        fs::write(&file, b"12").unwrap();
        f.replace_after = Some((file.clone(), 1));
        f.event(old, parent, 0x80000001);
        assert!(index.refresh(&mut f, &|| false).unwrap());
        let pending = index.snapshot(&|| false).unwrap();
        let reason = &pending.tracking.as_ref().unwrap().unavailable[0].reason;
        assert_eq!(reason, &UnavailableReason::Changing);
        assert_eq!(reason.error().raw_os_error(), None);
        assert!(reason.label().contains("확인 대기"));
        assert_eq!(pending.scan.error_count, 1);
        assert_eq!(pending.scan.folders[0].logical_bytes, 0);
        assert!(pending.scan.files.is_empty());
        assert_ne!(pending.tracking, before.tracking);
        let mut bytes = Vec::new();
        crate::snapshot::write_to(&pending, &mut bytes, || false).unwrap();
        let record = crate::snapshot::read_from(bytes.as_slice(), || false).unwrap();
        let mut resumed = Index::restore(&record).unwrap();
        f.clock += Duration::from_secs(30);
        resumed.refresh(&mut f, &|| false).unwrap();
        let recovered = resumed.snapshot(&|| false).unwrap();
        assert_eq!(recovered.scan.error_count, 0);
        assert_eq!(recovered.scan.folders[0].logical_bytes, 3);
    }
    #[test]
    fn cancelling_an_exclusion_retry_keeps_the_previous_result_and_cursor() {
        let mut f = Fixture::new();
        for i in 0..70 {
            let path = f.root.join(format!("retry-{i}.bin"));
            fs::write(&path, b"1").unwrap();
            f.failures.insert(path, 32);
        }
        let root = f.root.clone();
        let mut index = Index::begin(&root, true, &mut f, &|| false).unwrap();
        let before = index.snapshot(&|| false).unwrap();
        f.failures.clear();
        f.queries.clear();
        f.clock += Duration::from_secs(30);
        let polls = std::cell::Cell::new(0);
        assert!(index
            .refresh(&mut f, &|| {
                polls.set(polls.get() + 1);
                polls.get() >= 20
            })
            .is_err());
        assert!(f.queries.len() < 64);
        let after = index.snapshot(&|| false).unwrap();
        assert_eq!(after.tracking, before.tracking);
        assert!(Arc::ptr_eq(&after.scan, &before.scan));
        assert!(index.refresh(&mut f, &|| false).unwrap());
        assert_eq!(
            index.snapshot(&|| false).unwrap().scan.folders[0].logical_bytes,
            64
        );
    }
    #[test]
    fn retry_batch_is_bounded_and_root_or_unexpected_failures_still_stop() {
        let mut f = Fixture::new();
        for i in 0..70 {
            let p = f.root.join(format!("locked-{i}.bin"));
            fs::write(&p, b"1").unwrap();
            f.failures.insert(p, 33);
        }
        let root = f.root.clone();
        let mut index = Index::begin(&root, true, &mut f, &|| false).unwrap();
        assert_eq!(index.snapshot(&|| false).unwrap().scan.error_count, 70);
        f.queries.clear();
        f.clock += Duration::from_secs(30);
        assert!(index.refresh(&mut f, &|| false).unwrap());
        assert_eq!(
            f.queries
                .iter()
                .filter(|p| f.failures.contains_key(*p))
                .count(),
            64
        );
        f.queries.clear();
        index.refresh(&mut f, &|| false).unwrap();
        assert_eq!(
            f.queries
                .iter()
                .filter(|p| f.failures.contains_key(*p))
                .count(),
            6
        );
        f.failures.insert(root.clone(), 5);
        assert!(index.refresh(&mut f, &|| false).is_err());
        assert_eq!(index.snapshot(&|| false).unwrap().scan.error_count, 70);
        f.failures.remove(&root);
        f.failures.insert(root.join("locked-0.bin"), 1117);
        f.clock += Duration::from_secs(30);
        assert!(index.refresh(&mut f, &|| false).is_err());
        assert_eq!(index.snapshot(&|| false).unwrap().scan.error_count, 70);
    }
    #[test]
    fn excluded_path_deletion_and_replacement_do_not_reuse_the_previous_identity() {
        let mut f = Fixture::new();
        let root = f.root.join("scope");
        let path = root.join("a.bin");
        fs::write(&path, b"12345").unwrap();
        let old = f.id(&path);
        let parent = f.id(&root);
        let mut index = Index::begin(&root, true, &mut f, &|| false).unwrap();
        f.failures.insert(path.clone(), 32);
        f.event(old, parent, 0x80000001);
        index.refresh(&mut f, &|| false).unwrap();
        fs::rename(&path, f.root.join("outside.bin")).unwrap();
        fs::write(&path, b"123").unwrap();
        f.failures.clear();
        let new = f.id(&path);
        assert_ne!(old, new);
        f.clock += Duration::from_secs(30);
        index.refresh(&mut f, &|| false).unwrap();
        let record = index.snapshot(&|| false).unwrap();
        assert_eq!(record.scan.folders[0].logical_bytes, 3);
        assert_eq!(record.tracking.unwrap().files, vec![new]);
        f.failures.insert(path.clone(), 5);
        f.event(new, parent, 0x80000001);
        index.refresh(&mut f, &|| false).unwrap();
        fs::remove_file(&path).unwrap();
        f.event(new, parent, 0x80000200);
        index.refresh(&mut f, &|| false).unwrap();
        assert_eq!(index.snapshot(&|| false).unwrap().scan.error_count, 0);
    }
    #[test]
    fn live_lock_is_excluded_and_recovers_only_after_the_retry_interval() {
        let mut f = Fixture::new();
        let path = f.root.join("live.bin");
        fs::write(&path, b"12345").unwrap();
        let id = f.id(&path);
        let root = f.root.clone();
        let parent = f.id(&root);
        let mut index = Index::begin(&root, true, &mut f, &|| false).unwrap();
        f.failures.insert(path.clone(), 32);
        f.event(id, parent, 0x80000001);
        assert!(index.refresh(&mut f, &|| false).unwrap());
        let record = index.snapshot(&|| false).unwrap();
        assert_eq!(record.scan.folders[0].logical_bytes, 0);
        assert_eq!(record.scan.error_count, 1);
        f.queries.clear();
        f.clock += Duration::from_secs(5);
        f.event(id, parent, 0x80000001);
        index.refresh(&mut f, &|| false).unwrap();
        assert!(!f.queries.contains(&path));
        f.failures.clear();
        f.clock += Duration::from_secs(25);
        assert!(index.refresh(&mut f, &|| false).unwrap());
        let record = index.snapshot(&|| false).unwrap();
        assert_eq!(record.scan.folders[0].logical_bytes, 5);
        assert_eq!(record.scan.error_count, 0);
    }
    #[test]
    fn locked_baseline_item_is_excluded_without_stopping_other_files() {
        let mut f = Fixture::new();
        let blocked = f.root.join("locked.bin");
        fs::write(&blocked, b"fixture").unwrap();
        fs::write(f.root.join("visible.bin"), b"123").unwrap();
        f.blocked = Some(blocked);
        let root = f.root.clone();
        let mut index = Index::begin(&root, true, &mut f, &|| false).unwrap();
        let record = index.snapshot(&|| false).unwrap();
        assert_eq!(record.scan.folders[0].logical_bytes, 3);
        assert_eq!(record.scan.error_count, 1);
        assert_eq!(
            record.scan.folders[0].access,
            crate::scanner::FolderAccess::Partial
        );
        assert!(!record.scan.files.iter().any(|f| f.name == "locked.bin"));
        record
            .tracking
            .as_ref()
            .unwrap()
            .validate(&record.scan)
            .unwrap();
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
    fn change_budget_failure_preserves_metadata_keys_and_checkpoint_then_allows_retry() {
        let mut f = Fixture::new();
        let scope = f.root.join("scope");
        for n in 0..20 {
            fs::write(scope.join(format!("file-{n}")), b"123").unwrap();
        }
        let mut index = Index::begin(&scope, false, &mut f, &|| false).unwrap();
        let before = index.snapshot(&|| false).unwrap();
        let changed = scope.join("file-0");
        fs::write(&changed, b"1234567").unwrap();
        let id = f.id(&changed);
        let parent = f.id(&scope);
        f.event(id, parent, 0x80000001);
        let error = index
            .refresh_bounded(&mut f, &|| false, 0)
            .expect_err("원본 보관 예산 0에서 변경 거부 필요");
        assert!(error.to_string().contains("예산"), "{error}");
        let kept = index.snapshot(&|| false).unwrap();
        assert_eq!(kept.tracking, before.tracking);
        assert!(Arc::ptr_eq(&kept.scan, &before.scan));
        assert!(Arc::ptr_eq(
            kept.entries.as_ref().unwrap(),
            before.entries.as_ref().unwrap()
        ));
        assert_eq!(kept.scan.folders[0].logical_bytes, 60);
        assert!(index.refresh_bounded(&mut f, &|| false, 16_384).unwrap());
        assert_eq!(
            index.snapshot(&|| false).unwrap().scan.folders[0].logical_bytes,
            64
        );
        assert_eq!(index.edit_stats().touched_nodes, 1);
        assert_eq!(index.edit_stats().changed_entries, 1);
    }
    #[test]
    fn final_validation_rolls_back_file_and_exclusion_edits_without_changing_published_ids() {
        let mut f = Fixture::new();
        let scope = f.root.join("scope");
        for n in 0..20 {
            fs::write(scope.join(format!("file-{n}")), b"123").unwrap();
        }
        let mut index = Index::begin(&scope, false, &mut f, &|| false).unwrap();
        let before = index.snapshot(&|| false).unwrap();
        let parent = f.id(&scope);
        let changed = scope.join("file-0");
        let blocked = scope.join("file-1");
        fs::write(&changed, b"1234567").unwrap();
        let changed_id = f.id(&changed);
        let blocked_id = f.id(&blocked);
        f.event(changed_id, parent, 0x80000001);
        f.event(blocked_id, parent, 0x80000001);
        f.blocked = Some(blocked);
        let journal = f.journal.clone();
        let mut replaced = journal.clone();
        replaced.id += 1;
        f.journal_on_read = Some(replaced);
        assert!(index.refresh(&mut f, &|| false).is_err());
        let kept = index.snapshot(&|| false).unwrap();
        assert_eq!(before.tracking, kept.tracking);
        assert!(Arc::ptr_eq(&before.scan, &kept.scan));
        assert!(Arc::ptr_eq(
            before.entries.as_ref().unwrap(),
            kept.entries.as_ref().unwrap()
        ));
        f.journal = journal;
        assert!(index.refresh(&mut f, &|| false).unwrap());
        let after = index.snapshot(&|| false).unwrap();
        assert_eq!(after.scan.folders[0].logical_bytes, 61);
        assert_eq!(after.scan.error_count, 1);
        assert_eq!(index.edit_stats().touched_nodes, 2);
        assert_eq!(index.edit_stats().touched_unavailable, 1);
        assert_eq!(index.edit_stats().changed_entries, 3);
        for n in [0, 2, 19] {
            let path = PathBuf::from(format!("file-{n}"));
            assert_eq!(
                before.entries.as_ref().unwrap().path_key(&path),
                after.entries.as_ref().unwrap().path_key(&path)
            );
        }
    }
    #[test]
    fn fixed_ids_keep_open_folders_and_fall_back_when_the_selected_subtree_is_deleted() {
        let mut f = Fixture::new();
        let scope = f.root.join("scope");
        fs::create_dir_all(scope.join("folder/deep/nested")).unwrap();
        fs::write(scope.join("folder/deep/file"), b"123").unwrap();
        let mut index = Index::begin(&scope, false, &mut f, &|| false).unwrap();
        let before = index.snapshot(&|| false).unwrap();
        let folder = before.scan.folder_index(Path::new("folder/deep")).unwrap();
        let mut explorer = crate::explorer::Explorer::new(&before.scan);
        explorer.open_folder(&before.scan, folder);
        explorer.toggle_folder(&before.scan, folder);
        explorer.select(crate::explorer::Item::File(0));
        let parent = f.id(&scope);
        fs::create_dir(scope.join("aaa")).unwrap();
        let added_id = f.id(&scope.join("aaa"));
        f.event(added_id, parent, 0x80000100);
        index.refresh(&mut f, &|| false).unwrap();
        let added = index.snapshot(&|| false).unwrap();
        let next = explorer.rebase_entries(
            &before.scan,
            &added.scan,
            before.entries.as_ref().unwrap(),
            added.entries.as_ref().unwrap(),
        );
        assert_ne!(folder, next.folder());
        assert_eq!(
            added.scan.folders[next.folder()].path,
            scope.join("folder/deep")
        );
        assert!(next.is_expanded(next.folder()));
        assert!(matches!(next.selected(), crate::explorer::Item::File(_)));
        let deleted = f.id(&scope.join("folder"));
        fs::remove_dir_all(scope.join("folder")).unwrap();
        f.event(deleted, parent, 0x80000200);
        index.refresh(&mut f, &|| false).unwrap();
        let removed = index.snapshot(&|| false).unwrap();
        let restored = next.rebase_entries(
            &added.scan,
            &removed.scan,
            added.entries.as_ref().unwrap(),
            removed.entries.as_ref().unwrap(),
        );
        assert_eq!(restored.folder(), 0);
        assert_eq!(restored.selected(), crate::explorer::Item::Folder(0));
    }
    #[test]
    fn restored_noncanonical_rows_reconnect_keys_to_the_correct_files_after_a_content_update() {
        let mut f = Fixture::new();
        let scope = f.root.join("scope");
        for name in ["a", "b"] {
            fs::write(scope.join(name), b"123").unwrap();
        }
        let mut saved = Index::begin(&scope, false, &mut f, &|| false)
            .unwrap()
            .snapshot(&|| false)
            .unwrap();
        let scan = Arc::get_mut(&mut saved.scan).unwrap();
        scan.files.reverse();
        crate::scanner::finish(scan).unwrap();
        saved.tracking.as_mut().unwrap().files.reverse();
        saved.entries = None;
        let mut restored = Index::restore(&saved).unwrap();
        let before = restored.snapshot(&|| false).unwrap();
        let key = before
            .entries
            .as_ref()
            .unwrap()
            .path_key(Path::new("a"))
            .unwrap();
        fs::write(scope.join("a"), b"1234567").unwrap();
        let id = f.id(&scope.join("a"));
        let parent = f.id(&scope);
        f.event(id, parent, 0x80000001);
        restored.refresh(&mut f, &|| false).unwrap();
        let after = restored.snapshot(&|| false).unwrap();
        let Some(crate::explorer::Item::File(row)) = after.entries.as_ref().unwrap().item(key)
        else {
            panic!("같은 파일 ID 보존 필요")
        };
        assert_eq!(after.scan.files[row].name, "a");
        assert_eq!(after.scan.files[row].logical_bytes, 7);
    }
    #[test]
    fn cancelled_id_projection_does_not_publish_an_incomplete_mapping() {
        let mut f = Fixture::new();
        let scope = f.root.join("scope");
        fs::write(scope.join("file"), b"123").unwrap();
        let before = Index::begin(&scope, false, &mut f, &|| false)
            .unwrap()
            .snapshot(&|| false)
            .unwrap();
        let mut restored = Index::restore(&before).unwrap();
        let checkpoint = restored.checkpoint.clone();
        let calls = std::cell::Cell::new(0);
        let error = restored
            .snapshot(&|| {
                calls.set(calls.get() + 1);
                calls.get() >= 2
            })
            .err()
            .expect("ID 대응표 구성 중 취소 필요");
        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
        assert_eq!(restored.checkpoint, checkpoint);
        let after = restored.snapshot(&|| false).unwrap();
        assert!(Arc::ptr_eq(&before.scan, &after.scan));
        assert!(after
            .entries
            .as_ref()
            .unwrap()
            .key(crate::explorer::Item::File(0))
            .is_some());
    }
    #[test]
    fn hard_link_paths_have_distinct_keys_and_removed_or_replaced_entries_get_new_keys() {
        let mut f = Fixture::new();
        let scope = f.root.join("scope");
        let a = scope.join("a");
        let b = scope.join("b");
        fs::write(&a, b"123").unwrap();
        fs::hard_link(&a, &b).unwrap();
        let mut index = Index::begin(&scope, false, &mut f, &|| false).unwrap();
        let before = index.snapshot(&|| false).unwrap();
        let key = |record: &SavedScan, name| {
            record
                .entries
                .as_ref()
                .unwrap()
                .path_key(Path::new(name))
                .unwrap()
        };
        let ka = key(&before, "a");
        let kb = key(&before, "b");
        assert_ne!(ka, kb);
        assert_eq!(
            before.tracking.as_ref().unwrap().files[0],
            before.tracking.as_ref().unwrap().files[1]
        );
        let parent = f.id(&scope);
        let old_id = f.id(&a);
        fs::write(&a, b"12345").unwrap();
        f.event(old_id, parent, 0x80000001);
        index.refresh(&mut f, &|| false).unwrap();
        let edited = index.snapshot(&|| false).unwrap();
        assert_eq!((key(&edited, "a"), key(&edited, "b")), (ka, kb));
        assert!(Arc::ptr_eq(
            before.entries.as_ref().unwrap(),
            edited.entries.as_ref().unwrap()
        ));
        assert_eq!(edited.scan.folders[0].logical_bytes, 10);
        fs::remove_file(&b).unwrap();
        f.event(old_id, parent, 0x80000200);
        index.refresh(&mut f, &|| false).unwrap();
        assert!(index
            .snapshot(&|| false)
            .unwrap()
            .entries
            .as_ref()
            .unwrap()
            .item(kb)
            .is_none());
        fs::hard_link(&a, &b).unwrap();
        f.event(old_id, parent, 0x80000100);
        index.refresh(&mut f, &|| false).unwrap();
        let restored = index.snapshot(&|| false).unwrap();
        assert_ne!(key(&restored, "b"), kb);
        assert_eq!(key(&restored, "a"), ka);
        fs::rename(&a, f.root.join("held")).unwrap();
        fs::create_dir(&a).unwrap();
        let new_id = f.id(&a);
        f.event(new_id, parent, 0x80000100);
        index.refresh(&mut f, &|| false).unwrap();
        let replaced = index.snapshot(&|| false).unwrap();
        assert_ne!(key(&replaced, "a"), ka);
        assert!(replaced.entries.as_ref().unwrap().item(ka).is_none());
    }
    #[test]
    fn excluded_files_recover_with_new_keys_and_snapshot_ids_are_not_persisted() {
        let mut f = Fixture::new();
        let scope = f.root.join("scope");
        let file = scope.join("file");
        fs::write(&file, b"123").unwrap();
        let mut index = Index::begin(&scope, false, &mut f, &|| false).unwrap();
        let before = index.snapshot(&|| false).unwrap();
        let key = before
            .entries
            .as_ref()
            .unwrap()
            .path_key(Path::new("file"))
            .unwrap();
        let id = f.id(&file);
        let parent = f.id(&scope);
        f.blocked = Some(file.clone());
        f.event(id, parent, 0x80000001);
        index.refresh(&mut f, &|| false).unwrap();
        let excluded = index.snapshot(&|| false).unwrap();
        assert!(excluded.entries.as_ref().unwrap().item(key).is_none());
        assert_eq!(excluded.scan.error_count, 1);
        f.blocked = None;
        f.clock += Duration::from_secs(31);
        index.refresh(&mut f, &|| false).unwrap();
        let recovered = index.snapshot(&|| false).unwrap();
        assert_ne!(
            recovered
                .entries
                .as_ref()
                .unwrap()
                .path_key(Path::new("file"))
                .unwrap(),
            key
        );
        let mut bytes = vec![];
        crate::snapshot::write_to(&recovered, &mut bytes, || false).unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert!(json.get("entries").is_none());
        let loaded = crate::snapshot::read_from(bytes.as_slice(), || false).unwrap();
        assert!(loaded.entries.is_none());
        let mut resumed = Index::restore(&loaded).unwrap();
        let fresh = resumed.snapshot(&|| false).unwrap();
        let previous = recovered
            .entries
            .as_ref()
            .unwrap()
            .path_key(Path::new("file"))
            .unwrap();
        assert!(fresh.entries.as_ref().unwrap().item(previous).is_none());
        let scan = Arc::clone(&fresh.scan);
        assert!(resumed.refresh(&mut f, &|| true).is_err());
        assert!(Arc::ptr_eq(
            &scan,
            &resumed.snapshot(&|| false).unwrap().scan
        ));
    }
    #[test]
    fn stable_ids_survive_inserting_and_removing_another_path() {
        let mut f = Fixture::new();
        let scope = f.root.join("scope");
        fs::write(scope.join("b"), b"123").unwrap();
        fs::write(scope.join("c"), b"1234").unwrap();
        let mut index = Index::begin(&scope, false, &mut f, &|| false).unwrap();
        let before = index.snapshot(&|| false).unwrap();
        let key = |record: &SavedScan, name: &str| {
            let row = record
                .scan
                .files
                .iter()
                .position(|file| file.name == name)
                .unwrap();
            record
                .entries
                .as_ref()
                .expect("실시간 결과의 고정 ID 필요")
                .key(crate::explorer::Item::File(row))
                .unwrap()
        };
        let b = key(&before, "b");
        let c = key(&before, "c");
        let a_path = scope.join("a");
        fs::write(&a_path, b"12").unwrap();
        let a = f.id(&a_path);
        let parent = f.id(&scope);
        f.event(a, parent, 0x80000100);
        index.refresh(&mut f, &|| false).unwrap();
        let added = index.snapshot(&|| false).unwrap();
        assert_eq!((key(&added, "b"), key(&added, "c")), (b, c));
        assert_eq!(added.scan.folders[0].logical_bytes, 9);
        assert_ne!(key(&added, "a"), b);
        fs::remove_file(&a_path).unwrap();
        f.event(a, parent, 0x80000200);
        index.refresh(&mut f, &|| false).unwrap();
        let removed = index.snapshot(&|| false).unwrap();
        assert_eq!((key(&removed, "b"), key(&removed, "c")), (b, c));
        assert_eq!(removed.scan.folders[0].logical_bytes, 7);
    }
    #[test]
    fn baseline_captures_records_before_the_os_discards_them() {
        let mut f = Fixture::new();
        let scope = f.root.join("scope");
        for i in 0..20 {
            fs::write(scope.join(format!("file-{i}")), b"abc").unwrap();
        }
        f.rolling_journal = true;
        let mut source = buffered::Buffered::new(f).unwrap();
        let mut index = Index::begin(&scope, false, &mut source, &|| false).unwrap();
        let result = index.snapshot(&|| false).unwrap();
        assert_eq!(result.scan.folders[0].logical_bytes, 60);
        assert_eq!(result.scan.files.len(), 20);
        result
            .tracking
            .as_ref()
            .unwrap()
            .validate(&result.scan)
            .unwrap();
    }
    #[test]
    fn records_already_read_need_not_remain_in_the_os_journal() {
        let mut f = Fixture::new();
        let scope = f.root.join("scope");
        let file = scope.join("a");
        fs::write(&file, b"a").unwrap();
        let mut index = Index::begin(&scope, false, &mut f, &|| false).unwrap();
        fs::write(&file, b"abc").unwrap();
        let id = f.id(&file);
        let parent = f.id(&scope);
        f.event(id, parent, 0x80000001);
        let end = f.journal.next;
        let mut after_read = f.journal.clone();
        after_read.first = end;
        f.journal_on_read = Some(after_read);
        assert!(index.refresh(&mut f, &|| false).unwrap());
        assert_eq!(index.checkpoint.next, end);
        assert_eq!(
            index.snapshot(&|| false).unwrap().scan.folders[0].logical_bytes,
            3
        );
    }
    #[test]
    fn continuity_failure_identifies_the_invalid_boundary_and_preserves_the_result() {
        let mut f = Fixture::new();
        let scope = f.root.join("scope");
        let mut index = Index::begin(&scope, false, &mut f, &|| false).unwrap();
        let previous = index.snapshot(&|| false).unwrap();
        let original = f.journal.clone();
        for (kind, value) in [
            ("volume", 0),
            ("generation", 1),
            ("retention", 2),
            ("future", 3),
        ] {
            f.journal = original.clone();
            match value {
                0 => f.journal.volume.push_str("-replaced"),
                1 => f.journal.id += 1,
                2 => f.journal.first = original.next + 1,
                _ => f.journal.next -= 1,
            }
            let error = index.refresh(&mut f, &|| false).unwrap_err().to_string();
            assert!(error.contains(kind), "{kind}: {error}");
            assert!(error.contains("cursor="), "{error}");
            assert_eq!(index.checkpoint, previous.tracking.clone().unwrap());
            assert!(Arc::ptr_eq(
                &previous.scan,
                &index.snapshot(&|| false).unwrap().scan
            ));
        }
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
