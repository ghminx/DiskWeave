use super::{Advance, Checkpoint, Index, Node, Unavailable};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    io::{self, Read, Write},
    os::windows::ffi::{OsStrExt, OsStringExt},
    path::{Component, Path, PathBuf},
    time::SystemTime,
};

#[derive(Clone, Copy)]
pub struct Limits {
    pub frame_bytes: usize,
    pub entries: usize,
    pub state_bytes: u64,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            frame_bytes: 1024 * 1024,
            entries: 10_000_000,
            state_bytes: 8 * 1024 * 1024 * 1024,
        }
    }
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) enum Change {
    Node {
        path: Vec<u16>,
        expected: Option<u64>,
        value: Option<Node>,
    },
    Unavailable {
        path: Vec<u16>,
        present: bool,
        value: Option<Unavailable>,
    },
}
fn wide(path: &Path) -> Vec<u16> {
    path.as_os_str().encode_wide().collect()
}
pub(super) fn diff(
    old: &super::entries::Entries,
    new: &super::entries::Entries,
    before: &BTreeMap<PathBuf, Unavailable>,
    after: &BTreeMap<PathBuf, Unavailable>,
) -> Vec<Change> {
    let mut changes = Vec::new();
    for (path, node) in old {
        if new.get(path) != Some(node) {
            changes.push(Change::Node {
                path: wide(path),
                expected: Some(node.id),
                value: new.get(path).cloned(),
            });
        }
    }
    for (path, node) in new {
        if !old.contains_key(path) {
            changes.push(Change::Node {
                path: wide(path),
                expected: None,
                value: Some(node.clone()),
            });
        }
    }
    for (path, item) in before {
        if after.get(path) != Some(item) {
            changes.push(Change::Unavailable {
                path: wide(path),
                present: true,
                value: after.get(path).cloned(),
            });
        }
    }
    for (path, item) in after {
        if !before.contains_key(path) {
            changes.push(Change::Unavailable {
                path: wide(path),
                present: false,
                value: Some(item.clone()),
            });
        }
    }
    changes
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Header {
    root: Vec<u16>,
    detailed: bool,
    started: SystemTime,
    volume: String,
    root_id: u64,
    journal: u64,
    next: i64,
    confirmed_after: Option<SystemTime>,
    finished: SystemTime,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
enum Frame {
    // 기존 도우미 제어 메시지와 같은 표현으로 전송 중 실패 원인 보존.
    Failed(String),
    Begin {
        session: u64,
        revision: u64,
        base: Option<u64>,
        quiet: bool,
        header: Header,
    },
    Change(Change),
    Pending(Vec<u64>),
    End {
        frames: u64,
        hash: u64,
    },
}
#[derive(Default, Debug)]
pub struct Stats {
    pub bytes: u64,
    pub changes: u64,
}
const HASH_START: u64 = 0xcbf29ce484222325;
fn hash(mut value: u64, bytes: &[u8]) -> u64 {
    for &byte in bytes {
        value ^= byte as u64;
        value = value.wrapping_mul(0x100000001b3);
    }
    value
}
fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}
fn emit(
    writer: &mut impl Write,
    frame: &Frame,
    limit: Limits,
    digest: &mut u64,
    stats: &mut Stats,
) -> io::Result<()> {
    let bytes = serde_json::to_vec(frame).map_err(crate::output::json_error)?;
    if bytes.len() > limit.frame_bytes || bytes.len() > u32::MAX as usize {
        return Err(invalid("갱신 메시지 크기 초과"));
    }
    let size = (bytes.len() as u32).to_le_bytes();
    writer.write_all(&size)?;
    writer.write_all(&bytes)?;
    *digest = hash(hash(*digest, &size), &bytes);
    stats.bytes += 4 + bytes.len() as u64;
    Ok(())
}
fn read_frame(reader: &mut impl Read, limit: Limits, digest: &mut u64) -> io::Result<Frame> {
    let mut size = [0; 4];
    reader.read_exact(&mut size)?;
    let n = u32::from_le_bytes(size) as usize;
    if n == 0 || n > limit.frame_bytes {
        return Err(invalid("갱신 메시지 길이 오류"));
    }
    let mut bytes = vec![0; n];
    reader.read_exact(&mut bytes)?;
    *digest = hash(hash(*digest, &size), &bytes);
    match serde_json::from_slice(&bytes).map_err(crate::output::json_error)? {
        Frame::Failed(error) => Err(io::Error::other(error)),
        frame => Ok(frame),
    }
}
fn path(value: &[u16]) -> io::Result<PathBuf> {
    if value.len() > 32767
        || value.contains(&0)
        || !value.is_empty()
            && value
                .split(|&n| n == 92)
                .any(|part| part.is_empty() || part.len() > 255)
    {
        return Err(invalid("갱신 상대 경로 길이 오류"));
    }
    let path = PathBuf::from(std::ffi::OsString::from_wide(value));
    if path
        .components()
        .any(|p| !matches!(p, Component::Normal(_)))
        || value.contains(&58)
        || value.contains(&47)
    {
        return Err(invalid("갱신 상대 경로 형식 오류"));
    }
    Ok(path)
}
fn node_cost(path: &Path, node: &Node) -> u64 {
    384 + std::mem::size_of::<Node>() as u64
        + (path.as_os_str().len() as u64) * 4
        + node.issue.as_ref().map_or(0, |s| s.len() as u64 * 2)
}
fn unavailable_cost(path: &Path, item: &Unavailable) -> u64 {
    384 + std::mem::size_of::<Unavailable>() as u64
        + path.as_os_str().len() as u64 * 4
        + item.path.len() as u64 * 2
}
fn estimate(index: &Index) -> u64 {
    index
        .nodes
        .iter()
        .map(|(p, n)| node_cost(p, n))
        .sum::<u64>()
        + index
            .unavailable
            .iter()
            .map(|(p, n)| unavailable_cost(p, n))
            .sum::<u64>()
}

pub struct Sender {
    session: u64,
    revision: u64,
    limits: Limits,
}
impl Sender {
    pub fn new(session: u64, limits: Limits) -> Self {
        Self {
            session,
            revision: 0,
            limits,
        }
    }
    /// 최초 목록 또는 직전 성공 refresh의 변경을 한 트랜잭션으로 전달.
    /// 각 refresh 성공 직후 호출하며, 전송 실패 시 해당 연결을 폐기하는 계약.
    pub fn write(
        &mut self,
        writer: &mut impl Write,
        index: &mut Index,
        cancelled: &impl Fn() -> bool,
    ) -> io::Result<Stats> {
        crate::output::check(cancelled)?;
        let quiet = self.revision != 0 && index.changes.is_empty();
        if self.session == 0
            || index.nodes.len() + index.unavailable.len() > self.limits.entries
            || !quiet && estimate(index) > self.limits.state_bytes / 2
        {
            return Err(invalid("갱신 목록이 항목·메모리 예산을 초과했습니다"));
        }
        let baseline = self.revision == 0;
        let revision = self
            .revision
            .checked_add(1)
            .ok_or_else(|| invalid("갱신 순서 초과"))?;
        let header = Header {
            root: wide(&index.root),
            detailed: index.detailed,
            started: index.started,
            volume: index.checkpoint.volume.clone(),
            root_id: index.checkpoint.root,
            journal: index.checkpoint.journal,
            next: index.checkpoint.next,
            confirmed_after: index.checkpoint.confirmed_after,
            finished: SystemTime::now(),
        };
        let mut digest = HASH_START;
        let mut stats = Stats::default();
        let mut frames = 1;
        emit(
            writer,
            &Frame::Begin {
                session: self.session,
                revision,
                base: (!baseline).then_some(self.revision),
                quiet,
                header,
            },
            self.limits,
            &mut digest,
            &mut stats,
        )?;
        let mut send = |change: Change| -> io::Result<()> {
            crate::output::check(cancelled)?;
            emit(
                writer,
                &Frame::Change(change),
                self.limits,
                &mut digest,
                &mut stats,
            )?;
            frames += 1;
            stats.changes += 1;
            Ok(())
        };
        if baseline {
            for (p, n) in &index.nodes {
                send(Change::Node {
                    path: wide(p),
                    expected: None,
                    value: Some(n.clone()),
                })?;
            }
            for (p, n) in &index.unavailable {
                send(Change::Unavailable {
                    path: wide(p),
                    present: false,
                    value: Some(n.clone()),
                })?;
            }
        } else {
            for change in &index.changes {
                send(change.clone())?;
            }
        }
        for pending in index.checkpoint.pending.chunks(8192) {
            crate::output::check(cancelled)?;
            emit(
                writer,
                &Frame::Pending(pending.to_vec()),
                self.limits,
                &mut digest,
                &mut stats,
            )?;
            frames += 1;
        }
        let end = Frame::End {
            frames,
            hash: digest,
        };
        emit(writer, &end, self.limits, &mut digest, &mut stats)?;
        writer.flush()?;
        self.revision = revision;
        index.changes.clear();
        Ok(stats)
    }
}
pub struct Receiver {
    root: PathBuf,
    detailed: bool,
    limits: Limits,
    index: Option<Index>,
    session: Option<u64>,
    revision: u64,
    units: u64,
    finished: SystemTime,
    ids: HashSet<u64>,
    dirty: bool,
    published: bool,
}
impl Receiver {
    pub fn new(root: PathBuf, detailed: bool, limits: Limits) -> Self {
        Self {
            root,
            detailed,
            limits,
            index: None,
            session: None,
            revision: 0,
            units: 0,
            finished: SystemTime::now(),
            ids: HashSet::new(),
            dirty: false,
            published: false,
        }
    }
    pub fn receive(
        &mut self,
        reader: &mut impl Read,
        cancelled: &impl Fn() -> bool,
    ) -> io::Result<bool> {
        crate::output::check(cancelled)?;
        let mut digest = HASH_START;
        let Frame::Begin {
            session,
            revision,
            base,
            quiet,
            header,
        } = read_frame(reader, self.limits, &mut digest)?
        else {
            return Err(invalid("갱신 시작 메시지 누락"));
        };
        if session == 0 || self.session.is_some_and(|s| s != session) {
            return Err(invalid("갱신 세션 불일치"));
        }
        if revision
            != self
                .revision
                .checked_add(1)
                .ok_or_else(|| invalid("갱신 순서 초과"))?
            || base != (self.index.is_some()).then_some(self.revision)
        {
            return Err(invalid("갱신 기준 버전·순서 불일치"));
        }
        let received_root = PathBuf::from(std::ffi::OsString::from_wide(&header.root));
        if received_root != self.root {
            return Err(invalid(&format!(
                "갱신 대상 경로 불일치: {:?} / {:?}",
                received_root, self.root
            )));
        }
        if header.detailed != self.detailed
            || header.volume.is_empty()
            || header.root_id == 0
            || header.next < 0
        {
            return Err(invalid("갱신 수집 옵션·볼륨 기준 오류"));
        }
        if let Some(old) = &self.index {
            if header.volume != old.checkpoint.volume
                || header.journal != old.checkpoint.journal
                || header.root_id != old.checkpoint.root
                || header.next < old.checkpoint.next
                || header.started != old.started
            {
                return Err(invalid("갱신 볼륨·저널·기준점 불일치"));
            }
        }
        if quiet {
            if self.index.is_none() {
                return Err(invalid("기준 목록 없는 확인 응답"));
            }
            let mut pending = Vec::new();
            let mut seen = HashSet::new();
            let mut frames = 1;
            loop {
                crate::output::check(cancelled)?;
                let before = digest;
                match read_frame(reader, self.limits, &mut digest)? {
                    Frame::Pending(ids) => {
                        for id in ids {
                            if !self.ids.contains(&id)
                                || !seen.insert(id)
                                || pending.len() >= self.limits.entries
                            {
                                return Err(invalid("확인 응답 대기 ID 오류"));
                            }
                            pending.push(id);
                        }
                    }
                    Frame::End {
                        frames: count,
                        hash,
                    } => {
                        if count != frames || hash != before {
                            return Err(invalid("확인 응답 누락·손상"));
                        }
                        break;
                    }
                    _ => return Err(invalid("확인 응답에 잘못된 변경 메시지")),
                }
                frames += 1;
                if frames > self.limits.entries as u64 + 2 {
                    return Err(invalid("확인 응답 개수 초과"));
                }
            }
            crate::output::check(cancelled)?;
            let checkpoint = &mut self.index.as_mut().unwrap().checkpoint;
            checkpoint.next = header.next;
            checkpoint.pending = pending;
            checkpoint.confirmed_after = header.confirmed_after;
            self.revision = revision;
            self.finished = header.finished;
            return Ok(false);
        }
        if self.units > self.limits.state_bytes / 2 {
            return Err(invalid("갱신 후보가 메모리 예산을 초과했습니다"));
        }
        let mut candidate = if let Some(old) = &self.index {
            if header.volume != old.checkpoint.volume
                || header.journal != old.checkpoint.journal
                || header.root_id != old.checkpoint.root
                || header.next < old.checkpoint.next
            {
                return Err(invalid("갱신 볼륨·저널·기준점 불일치"));
            }
            Index {
                root: old.root.clone(),
                detailed: old.detailed,
                nodes: old.nodes.clone(),
                unavailable: old.unavailable.clone(),
                checkpoint: old.checkpoint.clone(),
                scan: old.scan.clone(),
                projection: old.projection.clone(),
                started: old.started,
                changes: vec![],
            }
        } else {
            Index {
                root: self.root.clone(),
                detailed: self.detailed,
                nodes: super::entries::Entries::new(),
                unavailable: BTreeMap::new(),
                checkpoint: Checkpoint {
                    version: 4,
                    volume: header.volume.clone(),
                    root: header.root_id,
                    journal: header.journal,
                    next: header.next,
                    folders: vec![],
                    files: vec![],
                    excluded: vec![],
                    pending: vec![],
                    unavailable: vec![],
                    confirmed_after: header.confirmed_after,
                },
                scan: None,
                projection: None,
                started: header.started,
                changes: vec![],
            }
        };
        candidate.checkpoint.next = header.next;
        candidate.checkpoint.pending.clear();
        candidate.checkpoint.confirmed_after = header.confirmed_after;
        let baseline = self.index.is_none();
        let mut changed = baseline;
        let mut units = self.units;
        let mut frames = 1;
        let mut seen = HashSet::new();
        let mut pending_seen = HashSet::new();
        loop {
            crate::output::check(cancelled)?;
            let before_hash = digest;
            match read_frame(reader, self.limits, &mut digest)? {
                Frame::End {
                    frames: expected,
                    hash,
                } => {
                    if expected != frames || hash != before_hash {
                        return Err(invalid("갱신 메시지 누락·손상"));
                    }
                    break;
                }
                Frame::Change(change) => {
                    let (kind, encoded) = match &change {
                        Change::Node { path, .. } => (0, path),
                        Change::Unavailable { path, .. } => (1, path),
                    };
                    let p = path(encoded)?;
                    if !baseline && !seen.insert((kind, p.clone())) {
                        return Err(invalid("중복 갱신 항목"));
                    }
                    match change {
                        Change::Node {
                            expected, value, ..
                        } => {
                            if candidate.nodes.get(&p).map(|n| n.id) != expected
                                || baseline && expected.is_some()
                            {
                                return Err(invalid("갱신 대상 파일 ID 불일치"));
                            }
                            if let Some(old) = candidate.nodes.get(&p) {
                                units -= node_cost(&p, old);
                            }
                            if let Some(value) = value {
                                if value.id == 0 || value.dir() && value.bytes != 0 {
                                    return Err(invalid("갱신 항목 정보 오류"));
                                }
                                units += node_cost(&p, &value);
                                candidate.nodes.insert(p, value);
                            } else if expected.is_none() {
                                return Err(invalid("존재하지 않는 삭제 대상"));
                            } else {
                                candidate.nodes.remove(&p);
                            }
                        }
                        Change::Unavailable { present, value, .. } => {
                            if candidate.unavailable.contains_key(&p) != present
                                || baseline && present
                            {
                                return Err(invalid("확인 불가 항목 기준 불일치"));
                            }
                            if let Some(old) = candidate.unavailable.remove(&p) {
                                units -= unavailable_cost(&p, &old);
                            }
                            if let Some(value) = value {
                                if value.relative() != p {
                                    return Err(invalid("확인 불가 항목 경로 불일치"));
                                }
                                units += unavailable_cost(&p, &value);
                                candidate.unavailable.insert(p, value);
                            } else if !present {
                                return Err(invalid("존재하지 않는 제외 해제 대상"));
                            }
                        }
                    }
                    changed = true;
                }
                Frame::Pending(ids) => {
                    for id in ids {
                        if id == 0 || !pending_seen.insert(id) {
                            return Err(invalid("대기 파일 ID 중복·누락"));
                        }
                        candidate.checkpoint.pending.push(id);
                    }
                }
                Frame::Begin { .. } | Frame::Failed(_) => {
                    return Err(invalid("잘못된 갱신 메시지"))
                }
            }
            frames += 1;
            if candidate.nodes.len() + candidate.unavailable.len() > self.limits.entries
                || candidate.checkpoint.pending.len() > self.limits.entries
                || units > self.limits.state_bytes / 2
                || units
                    .checked_add(self.units)
                    .is_none_or(|n| n > self.limits.state_bytes)
                || frames > self.limits.entries as u64 * 4 + 8
            {
                return Err(invalid("갱신 목록이 항목·메모리 예산을 초과했습니다"));
            }
        }
        let ids = validate(&candidate, cancelled)?;
        if changed {
            candidate.scan = None;
            candidate.projection = None;
            candidate.checkpoint.folders.clear();
            candidate.checkpoint.files.clear();
            candidate.checkpoint.excluded.clear();
            candidate.checkpoint.unavailable.clear();
        }
        crate::output::check(cancelled)?;
        self.index = Some(candidate);
        self.session = Some(session);
        self.revision = revision;
        self.units = units;
        self.finished = header.finished;
        self.ids = ids;
        self.dirty |= changed;
        Ok(changed)
    }
    /// 모든 변경을 먼저 적용하고 소비자가 준비됐을 때만 완료 목록 재구성.
    pub fn pending(&self) -> bool {
        self.dirty
    }
    pub fn publication(
        &mut self,
        ready: bool,
        cancelled: &impl Fn() -> bool,
    ) -> io::Result<Option<super::WatchUpdate>> {
        if self.dirty {
            if !ready && self.published {
                return Ok(None);
            }
            let record = self.snapshot(cancelled)?;
            self.dirty = false;
            self.published = true;
            Ok(Some(super::WatchUpdate::Record(Box::new(record))))
        } else {
            Ok(Some(super::WatchUpdate::Cursor(self.advance()?)))
        }
    }
    pub fn snapshot(
        &mut self,
        cancelled: &impl Fn() -> bool,
    ) -> io::Result<crate::snapshot::SavedScan> {
        let index = self
            .index
            .as_mut()
            .ok_or_else(|| invalid("기준 목록 없음"))?;
        let mut record = index.snapshot(cancelled)?;
        record.finished = self.finished;
        record.tracking.as_ref().unwrap().validate(&record.scan)?;
        Ok(record)
    }
    pub fn advance(&self) -> io::Result<Advance> {
        let i = self
            .index
            .as_ref()
            .ok_or_else(|| invalid("기준 목록 없음"))?;
        Ok(Advance {
            next: i.checkpoint.next,
            pending: i.checkpoint.pending.clone(),
            confirmed_after: i.checkpoint.confirmed_after,
            finished: self.finished,
        })
    }
    pub fn into_index(self) -> io::Result<Index> {
        self.index.ok_or_else(|| invalid("기준 목록 없음"))
    }
}
fn validate(index: &Index, cancelled: &impl Fn() -> bool) -> io::Result<HashSet<u64>> {
    let root = index
        .nodes
        .get(Path::new(""))
        .ok_or_else(|| invalid("추적 루트 누락"))?;
    if root.id != index.checkpoint.root || !root.dir() || root.excluded() {
        return Err(invalid("추적 루트 불일치"));
    }
    let mut dirs = HashMap::new();
    let mut files = HashSet::new();
    let mut bytes = 0u64;
    let mut allocation = 0u64;
    for (p, n) in &index.nodes {
        crate::output::check(cancelled)?;
        if !p.as_os_str().is_empty()
            && (index
                .unavailable
                .get(p.parent().unwrap())
                .is_some_and(|e| e.listing)
                || !index
                    .nodes
                    .get(p.parent().unwrap())
                    .is_some_and(|parent| parent.dir() && !parent.excluded()))
        {
            return Err(invalid("갱신 항목 부모 누락"));
        }
        if n.excluded() {
            continue;
        }
        if n.dir() {
            if let Some(previous) = dirs.insert(n.id, p) {
                return Err(invalid(&format!(
                    "동일한 폴더 ID의 경로 불일치: {} / {} (ID {:X})",
                    previous.display(),
                    p.display(),
                    n.id
                )));
            }
        } else {
            files.insert(n.id);
            bytes = bytes
                .checked_add(n.bytes)
                .ok_or_else(|| invalid("논리 크기 합계 초과"))?;
            if let crate::metadata::Field::Value(n) = n.extra.allocation {
                allocation = allocation
                    .checked_add(n)
                    .ok_or_else(|| invalid("할당 크기 합계 초과"))?;
            }
        }
    }
    for (p, e) in &index.unavailable {
        crate::output::check(cancelled)?;
        if p.as_os_str().is_empty()
            || !index
                .nodes
                .get(p.parent().unwrap())
                .is_some_and(|n| n.dir() && !n.excluded() && n.id == e.parent)
            || !matches!(
                e.reason,
                super::UnavailableReason::Os(5 | 32 | 33)
                    | super::UnavailableReason::Changing
                    | super::UnavailableReason::Redirected
            )
            || e.previous_id == Some(0)
            || matches!(e.reason, super::UnavailableReason::Changing) && e.listing
            || e.reason == super::UnavailableReason::Redirected && (e.listing || !e.directory)
            || e.listing && (!e.directory || index.nodes.get(p).map(|n| n.id) != e.previous_id)
            || !e.listing && index.nodes.contains_key(p)
            || e.listing && !index.nodes.get(p).is_some_and(|n| n.dir() && !n.excluded())
        {
            return Err(invalid("확인 불가 항목 부모·상태 오류"));
        }
    }
    if dirs.keys().any(|id| files.contains(id))
        || index
            .checkpoint
            .pending
            .iter()
            .any(|id| !files.contains(id))
    {
        return Err(invalid("등록되지 않은 대기 파일 ID"));
    }
    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::usn::{Checkpoint, Index};
    use std::{
        io,
        path::Path,
        sync::Arc,
        time::{Duration, SystemTime},
    };
    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let root = std::env::temp_dir().join(format!(
                "diskweave-stream-{}-{}-{}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(SystemTime::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos(),
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            std::fs::create_dir(&root).unwrap();
            Self(root)
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let temporary = std::fs::canonicalize(std::env::temp_dir()).unwrap();
            let root = std::fs::canonicalize(&self.0).unwrap();
            assert!(
                root.starts_with(temporary)
                    && root
                        .file_name()
                        .unwrap()
                        .to_string_lossy()
                        .starts_with("diskweave-stream-")
            );
            std::fs::remove_dir_all(&root).unwrap();
        }
    }

    fn record(root: &Path) -> crate::snapshot::SavedScan {
        let scan =
            crate::scanner::scan_unified(root, Default::default(), || false, |_| {}, None).unwrap();
        let drive = root.components().take(2).collect::<PathBuf>();
        let volume = crate::ntfs_native::target(&drive).unwrap().volume;
        let mut ids = Vec::new();
        for path in scan.folders.iter().map(|f| f.path.clone()).chain(
            scan.files
                .iter()
                .map(|f| scan.folders[f.parent].path.join(&f.name)),
        ) {
            let Some(crate::ntfs::Id::V2(id)) =
                crate::ntfs_native::inspect_file(&path, false, &volume)
                    .unwrap()
                    .reference
            else {
                panic!("파일 ID 필요")
            };
            ids.push(id);
        }
        let mut record = crate::snapshot::SavedScan::new(
            Arc::new(scan),
            SystemTime::now(),
            SystemTime::now(),
            Duration::ZERO,
        );
        let files = ids.split_off(record.scan.folders.len());
        record.tracking = Some(Checkpoint {
            version: 3,
            volume,
            root: ids[0],
            journal: 7,
            next: 100,
            folders: ids,
            files,
            excluded: vec![],
            pending: vec![],
            unavailable: vec![],
            confirmed_after: None,
        });
        record
    }
    struct Source {
        clock: SystemTime,
        blocked: Option<PathBuf>,
        journal: crate::usn::Journal,
        events: Vec<crate::usn::Event>,
    }
    impl crate::usn::Source for Source {
        fn now(&self) -> SystemTime {
            self.clock
        }
        fn journal(&mut self) -> io::Result<crate::usn::Journal> {
            Ok(self.journal.clone())
        }
        fn read(&mut self, start: i64, end: i64) -> io::Result<Vec<crate::usn::Event>> {
            Ok(self
                .events
                .iter()
                .filter(|e| e.usn >= start && e.usn < end)
                .cloned()
                .collect())
        }
        fn inspect(&mut self, path: &Path, detailed: bool) -> io::Result<crate::ntfs::Info> {
            if self.blocked.as_deref() == Some(path) {
                return Err(io::Error::from_raw_os_error(32));
            }
            crate::ntfs_native::inspect_file(path, detailed, &self.journal.volume)
        }
        fn list(&mut self, path: &Path) -> io::Result<Vec<PathBuf>> {
            std::fs::read_dir(path)?
                .map(|e| e.map(|e| e.path()))
                .collect()
        }
    }

    #[test]
    fn resumed_baseline_accepts_the_same_root_with_a_trailing_separator() {
        let fixture = Fixture::new();
        std::fs::write(fixture.0.join("file"), b"123").unwrap();
        let mut original = Index::restore(&record(&fixture.0)).unwrap();
        let mut saved = original.snapshot(&|| false).unwrap();
        drop(original);
        let mut terminated = fixture.0.as_os_str().to_owned();
        terminated.push("\\");
        Arc::get_mut(&mut saved.scan).unwrap().folders[0].path = terminated.into();
        let mut resumed = Index::restore(&saved).unwrap();
        let mut wire = Vec::new();
        Sender::new(17, Limits::default())
            .write(&mut wire, &mut resumed, &|| false)
            .unwrap();
        let mut receiver = Receiver::new(fixture.0.clone(), true, Limits::default());
        receiver.receive(&mut wire.as_slice(), &|| false).unwrap();
        assert_eq!(
            receiver.snapshot(&|| false).unwrap().scan.folders[0].logical_bytes,
            3
        );
    }

    #[test]
    fn replacing_a_path_does_not_transfer_explorer_selection_to_another_file() {
        let fixture = Fixture::new();
        let path = fixture.0.join("selected");
        std::fs::write(&path, b"123").unwrap();
        let saved = record(&fixture.0);
        let state = saved.tracking.as_ref().unwrap();
        let mut source = Source {
            clock: SystemTime::now(),
            blocked: None,
            journal: crate::usn::Journal {
                volume: state.volume.clone(),
                id: 7,
                first: 0,
                lowest: 0,
                next: 100,
            },
            events: vec![],
        };
        let mut index = Index::restore(&saved).unwrap();
        let before = index.snapshot(&|| false).unwrap();
        let selected = crate::explorer::Item::File(0);
        let mut explorer = crate::explorer::Explorer::new(&before.scan);
        explorer.select(selected);
        std::fs::rename(&path, fixture.0.join("previous")).unwrap();
        std::fs::write(&path, b"12345").unwrap();
        event(&mut source, &path, state.root, 0x80000100);
        index.refresh(&mut source, &|| false).unwrap();
        let after = index.snapshot(&|| false).unwrap();
        let rebased = explorer.rebase_entries(
            &before.scan,
            &after.scan,
            before.entries.as_ref().unwrap(),
            after.entries.as_ref().unwrap(),
        );
        assert_eq!(rebased.selected(), crate::explorer::Item::Folder(0));
    }
    #[test]
    fn metadata_deltas_keep_received_entry_ids_and_failed_frames_keep_the_id_map() {
        let fixture = Fixture::new();
        let path = fixture.0.join("file");
        std::fs::write(&path, b"123").unwrap();
        let saved = record(&fixture.0);
        let state = saved.tracking.as_ref().unwrap();
        let mut source = Source {
            clock: SystemTime::now(),
            blocked: None,
            journal: crate::usn::Journal {
                volume: state.volume.clone(),
                id: 7,
                first: 0,
                lowest: 0,
                next: 100,
            },
            events: vec![],
        };
        let mut index = Index::restore(&saved).unwrap();
        let mut sender = Sender::new(17, Default::default());
        let mut receiver = Receiver::new(fixture.0.clone(), true, Default::default());
        let mut baseline = vec![];
        sender.write(&mut baseline, &mut index, &|| false).unwrap();
        receiver
            .receive(&mut baseline.as_slice(), &|| false)
            .unwrap();
        let before = receiver.snapshot(&|| false).unwrap();
        let key = before
            .entries
            .as_ref()
            .unwrap()
            .path_key(Path::new("file"))
            .unwrap();
        std::fs::write(&path, b"1234567").unwrap();
        event(&mut source, &path, state.root, 0x80000001);
        index.refresh(&mut source, &|| false).unwrap();
        let mut delta = vec![];
        sender.write(&mut delta, &mut index, &|| false).unwrap();
        assert!(receiver
            .receive(&mut &delta[..delta.len() - 1], &|| false)
            .is_err());
        let kept = receiver.snapshot(&|| false).unwrap();
        assert!(Arc::ptr_eq(
            before.entries.as_ref().unwrap(),
            kept.entries.as_ref().unwrap()
        ));
        assert_eq!(before.tracking, kept.tracking);
        assert!(receiver.receive(&mut delta.as_slice(), &|| true).is_err());
        receiver.receive(&mut delta.as_slice(), &|| false).unwrap();
        let after = receiver.snapshot(&|| false).unwrap();
        assert_eq!(
            after.entries.as_ref().unwrap().path_key(Path::new("file")),
            Some(key)
        );
        assert_eq!(after.scan.folders[0].logical_bytes, 7);
    }
    #[test]
    fn unchanged_confirmation_keeps_the_same_result_without_sending_metadata() {
        let fixture = Fixture::new();
        for i in 0..10 {
            std::fs::write(fixture.0.join(format!("file-{i}")), b"x").unwrap();
        }
        let mut index = Index::restore(&record(&fixture.0)).unwrap();
        let mut sender = Sender::new(17, Limits::default());
        let mut receiver = Receiver::new(fixture.0.clone(), true, Limits::default());
        let mut wire = Vec::new();
        sender.write(&mut wire, &mut index, &|| false).unwrap();
        receiver.receive(&mut wire.as_slice(), &|| false).unwrap();
        let before = receiver.snapshot(&|| false).unwrap();
        index.checkpoint.next = 108;
        let mut wire = Vec::new();
        let idle = sender.write(&mut wire, &mut index, &|| false).unwrap();
        assert!(!receiver.receive(&mut wire.as_slice(), &|| false).unwrap());
        let after = receiver.snapshot(&|| false).unwrap();
        assert_eq!(idle.changes, 0);
        assert!(Arc::ptr_eq(&before.scan, &after.scan));
        assert_eq!(after.tracking.unwrap().next, 108);
    }

    #[test]
    fn helper_failure_during_transfer_preserves_its_reason_and_the_previous_result() {
        let fixture = Fixture::new();
        std::fs::write(fixture.0.join("file"), b"x").unwrap();
        let mut index = Index::restore(&record(&fixture.0)).unwrap();
        let mut wire = Vec::new();
        Sender::new(17, Limits::default())
            .write(&mut wire, &mut index, &|| false)
            .unwrap();
        let mut receiver = Receiver::new(fixture.0.clone(), true, Limits::default());
        receiver.receive(&mut wire.as_slice(), &|| false).unwrap();
        let before = receiver.snapshot(&|| false).unwrap();
        let payload =
            serde_json::to_vec(&serde_json::json!({"Failed":"목록이 메모리 예산을 초과했습니다"}))
                .unwrap();
        let failed = [(payload.len() as u32).to_le_bytes().as_slice(), &payload].concat();
        assert_eq!(
            receiver
                .receive(&mut failed.as_slice(), &|| false)
                .unwrap_err()
                .to_string(),
            "목록이 메모리 예산을 초과했습니다"
        );
        assert!(Arc::ptr_eq(
            &before.scan,
            &receiver.snapshot(&|| false).unwrap().scan
        ));
    }

    #[test]
    fn duplicate_folder_ids_are_rejected_with_both_conflicting_paths() {
        let fixture = Fixture::new();
        std::fs::create_dir(fixture.0.join("first-dir")).unwrap();
        std::fs::create_dir(fixture.0.join("second-dir")).unwrap();
        let saved = record(&fixture.0);
        let first = saved
            .scan
            .folders
            .iter()
            .position(|f| f.path.ends_with("first-dir"))
            .unwrap();
        let id = saved.tracking.as_ref().unwrap().folders[first];
        let mut index = Index::restore(&saved).unwrap();
        let mut wire = vec![];
        Sender::new(17, Limits::default())
            .write(&mut wire, &mut index, &|| false)
            .unwrap();
        let changed = rewrite(&wire, |frame| {
            if let Frame::Change(Change::Node {
                path,
                value: Some(node),
                ..
            }) = frame
            {
                if *path == wide(Path::new("second-dir")) {
                    node.id = id;
                }
            }
        });
        let mut receiver = Receiver::new(fixture.0.clone(), true, Limits::default());
        let error = receiver
            .receive(&mut changed.as_slice(), &|| false)
            .unwrap_err()
            .to_string();
        assert!(error.contains("first-dir") && error.contains("second-dir"));
        assert!(receiver.snapshot(&|| false).is_err());
    }

    #[test]
    fn frame_and_entry_limits_reject_a_baseline_without_exposing_partial_state() {
        let fixture = Fixture::new();
        std::fs::write(fixture.0.join("file"), b"x").unwrap();
        let mut index = Index::restore(&record(&fixture.0)).unwrap();
        let mut wire = Vec::new();
        Sender::new(17, Limits::default())
            .write(&mut wire, &mut index, &|| false)
            .unwrap();
        for limits in [
            Limits {
                frame_bytes: 32,
                ..Limits::default()
            },
            Limits {
                entries: 1,
                ..Limits::default()
            },
        ] {
            let mut receiver = Receiver::new(fixture.0.clone(), true, limits);
            assert!(receiver.receive(&mut wire.as_slice(), &|| false).is_err());
            assert!(receiver.publication(true, &|| false).is_err());
        }
    }

    #[test]
    fn baseline_reserves_memory_for_an_atomic_replacement_before_publication() {
        let fixture = Fixture::new();
        for i in 0..10 {
            std::fs::write(fixture.0.join(format!("file-{i}")), b"x").unwrap();
        }
        let mut index = Index::restore(&record(&fixture.0)).unwrap();
        let mut wire = Vec::new();
        Sender::new(17, Limits::default())
            .write(&mut wire, &mut index, &|| false)
            .unwrap();
        let mut receiver = Receiver::new(
            fixture.0.clone(),
            true,
            Limits {
                state_bytes: 8000,
                ..Limits::default()
            },
        );
        assert!(receiver.receive(&mut wire.as_slice(), &|| false).is_err());
        assert!(receiver.snapshot(&|| false).is_err());
    }

    fn rewrite(wire: &[u8], mut edit: impl FnMut(&mut Frame)) -> Vec<u8> {
        let mut reader = wire;
        let mut output = Vec::new();
        let mut old = HASH_START;
        let mut digest = HASH_START;
        let mut stats = Stats::default();
        let mut count = 0;
        loop {
            let mut frame = read_frame(&mut reader, Limits::default(), &mut old).unwrap();
            if matches!(frame, Frame::End { .. }) {
                let end = Frame::End {
                    frames: count,
                    hash: digest,
                };
                emit(
                    &mut output,
                    &end,
                    Limits::default(),
                    &mut digest,
                    &mut stats,
                )
                .unwrap();
                break;
            }
            edit(&mut frame);
            emit(
                &mut output,
                &frame,
                Limits::default(),
                &mut digest,
                &mut stats,
            )
            .unwrap();
            count += 1;
        }
        output
    }

    #[test]
    fn broken_stale_and_cancelled_transactions_preserve_the_previous_result_and_cursor() {
        let fixture = Fixture::new();
        std::fs::write(fixture.0.join("file"), b"x").unwrap();
        let baseline = record(&fixture.0);
        let state = baseline.tracking.as_ref().unwrap();
        let mut source = Source {
            blocked: None,
            clock: SystemTime::now(),
            journal: crate::usn::Journal {
                volume: state.volume.clone(),
                id: 7,
                first: 0,
                lowest: 0,
                next: 108,
            },
            events: vec![crate::usn::Event {
                usn: 100,
                id: state.files[0],
                parent: state.root,
                reason: 0x80000001,
            }],
        };
        let mut index = Index::restore(&baseline).unwrap();
        let mut sender = Sender::new(17, Limits::default());
        let mut receiver = Receiver::new(fixture.0.clone(), true, Limits::default());
        let mut first = Vec::new();
        sender.write(&mut first, &mut index, &|| false).unwrap();
        receiver.receive(&mut first.as_slice(), &|| false).unwrap();
        let before = receiver.snapshot(&|| false).unwrap();
        std::fs::write(fixture.0.join("file"), b"12345").unwrap();
        index.refresh(&mut source, &|| false).unwrap();
        let mut wire = Vec::new();
        sender.write(&mut wire, &mut index, &|| false).unwrap();
        let mut corrupted = wire.clone();
        let at = corrupted
            .windows(9)
            .position(|b| b == b"\"bytes\":5")
            .unwrap();
        corrupted[at + 8] = b'4';
        let bad_order = rewrite(&wire, |f| {
            if let Frame::Begin { revision, .. } = f {
                *revision += 1;
            }
        });
        let wrong_session = rewrite(&wire, |f| {
            if let Frame::Begin { session, .. } = f {
                *session += 1;
            }
        });
        let wrong_id = rewrite(&wire, |f| {
            if let Frame::Change(Change::Node { expected, .. }) = f {
                *expected = Some(0);
            }
        });
        for broken in [
            &wire[..wire.len() - 2],
            corrupted.as_slice(),
            bad_order.as_slice(),
            wrong_session.as_slice(),
            wrong_id.as_slice(),
            first.as_slice(),
            &[255, 255, 255, 255],
        ] {
            assert!(receiver.receive(&mut &*broken, &|| false).is_err());
            let kept = receiver.snapshot(&|| false).unwrap();
            assert!(Arc::ptr_eq(&before.scan, &kept.scan));
            assert_eq!(kept.tracking.as_ref().unwrap().next, 100);
        }
        let calls = std::cell::Cell::new(0);
        assert!(receiver
            .receive(&mut wire.as_slice(), &|| {
                calls.set(calls.get() + 1);
                calls.get() > 2
            })
            .is_err());
        assert_eq!(
            receiver.snapshot(&|| false).unwrap().tracking.unwrap().next,
            100
        );
        assert!(receiver.receive(&mut wire.as_slice(), &|| false).unwrap());
        assert_eq!(
            receiver.snapshot(&|| false).unwrap().scan.folders[0].logical_bytes,
            5
        );
    }

    fn assert_same(expected: &crate::snapshot::SavedScan, actual: &crate::snapshot::SavedScan) {
        let mut a = Vec::new();
        let mut b = Vec::new();
        crate::snapshot::write_to(expected, &mut a, || false).unwrap();
        crate::snapshot::write_to(actual, &mut b, || false).unwrap();
        let a: serde_json::Value = serde_json::from_slice(&a).unwrap();
        let b: serde_json::Value = serde_json::from_slice(&b).unwrap();
        for field in [
            "folders",
            "files",
            "tracking",
            "error_count",
            "metadata_error_count",
            "skipped_reparse_points",
            "issues",
            "metadata_issues",
        ] {
            assert_eq!(a[field], b[field], "{field}");
        }
    }
    fn tick(source: &mut Source, index: &mut Index, sender: &mut Sender, receiver: &mut Receiver) {
        index.refresh(source, &|| false).unwrap();
        let expected = index.snapshot(&|| false).unwrap();
        let mut wire = Vec::new();
        sender.write(&mut wire, index, &|| false).unwrap();
        receiver.receive(&mut wire.as_slice(), &|| false).unwrap();
        let actual = receiver.snapshot(&|| false).unwrap();
        assert_same(&expected, &actual);
    }
    fn event(source: &mut Source, path: &Path, parent: u64, reason: u32) -> u64 {
        let Some(crate::ntfs::Id::V2(id)) =
            crate::ntfs_native::inspect_file(path, false, &source.journal.volume)
                .unwrap()
                .reference
        else {
            panic!("파일 ID 필요")
        };
        source.events.push(crate::usn::Event {
            usn: source.journal.next,
            id,
            parent,
            reason,
        });
        source.journal.next += 8;
        id
    }
    #[test]
    fn directory_moves_locked_file_recovery_and_removal_match_all_original_fields() {
        let fixture = Fixture::new();
        std::fs::create_dir(fixture.0.join("sub")).unwrap();
        std::fs::write(fixture.0.join("sub/file"), b"123").unwrap();
        let baseline = record(&fixture.0);
        let state = baseline.tracking.as_ref().unwrap();
        let mut source = Source {
            blocked: None,
            clock: SystemTime::now(),
            journal: crate::usn::Journal {
                volume: state.volume.clone(),
                id: 7,
                first: 0,
                lowest: 0,
                next: 100,
            },
            events: vec![],
        };
        let mut index = Index::restore(&baseline).unwrap();
        let mut sender = Sender::new(17, Limits::default());
        let mut receiver = Receiver::new(fixture.0.clone(), true, Limits::default());
        let mut wire = Vec::new();
        sender.write(&mut wire, &mut index, &|| false).unwrap();
        receiver.receive(&mut wire.as_slice(), &|| false).unwrap();
        let sub = event(&mut source, &fixture.0.join("sub"), state.root, 0x80003000);
        std::fs::rename(fixture.0.join("sub"), fixture.0.join("renamed")).unwrap();
        tick(&mut source, &mut index, &mut sender, &mut receiver);
        let file = fixture.0.join("renamed/file");
        event(&mut source, &file, sub, 0x80000001);
        source.blocked = Some(file.clone());
        tick(&mut source, &mut index, &mut sender, &mut receiver);
        assert_eq!(receiver.snapshot(&|| false).unwrap().scan.error_count, 1);
        source.blocked = None;
        source.clock += Duration::from_secs(31);
        event(&mut source, &file, sub, 0x80000001);
        tick(&mut source, &mut index, &mut sender, &mut receiver);
        assert_eq!(receiver.snapshot(&|| false).unwrap().scan.error_count, 0);
        event(&mut source, &file, sub, 0x80000200);
        std::fs::remove_file(&file).unwrap();
        tick(&mut source, &mut index, &mut sender, &mut receiver);
        assert_eq!(receiver.snapshot(&|| false).unwrap().scan.files.len(), 0);
    }

    #[test]
    fn malformed_paths_are_rejected_before_publishing() {
        let fixture = Fixture::new();
        std::fs::write(fixture.0.join("file"), b"x").unwrap();
        let mut index = Index::restore(&record(&fixture.0)).unwrap();
        let mut sender = Sender::new(17, Limits::default());
        let mut wire = Vec::new();
        sender.write(&mut wire, &mut index, &|| false).unwrap();
        for bad in [
            vec![b'x' as u16; 256],
            "..\\outside".encode_utf16().collect(),
            "C:\\escape".encode_utf16().collect(),
        ] {
            let malformed = rewrite(&wire, |frame| {
                if let Frame::Change(Change::Node { path, .. }) = frame {
                    if !path.is_empty() {
                        *path = bad.clone();
                    }
                }
            });
            let mut receiver = Receiver::new(fixture.0.clone(), true, Limits::default());
            assert!(receiver
                .receive(&mut malformed.as_slice(), &|| false)
                .is_err());
            assert!(receiver.snapshot(&|| false).is_err());
        }
    }

    #[test]
    fn a_slow_consumer_receives_all_ordered_changes_in_the_latest_complete_result() {
        let fixture = Fixture::new();
        std::fs::write(fixture.0.join("file"), b"x").unwrap();
        let baseline = record(&fixture.0);
        let checkpoint = baseline.tracking.as_ref().unwrap();
        let mut source = Source {
            blocked: None,
            clock: SystemTime::now(),
            journal: crate::usn::Journal {
                volume: checkpoint.volume.clone(),
                id: 7,
                first: 0,
                lowest: 0,
                next: 100,
            },
            events: vec![],
        };
        let mut index = Index::restore(&baseline).unwrap();
        let mut sender = Sender::new(17, Limits::default());
        let mut receiver = Receiver::new(fixture.0.clone(), true, Limits::default());
        let mut wire = Vec::new();
        sender.write(&mut wire, &mut index, &|| false).unwrap();
        receiver.receive(&mut wire.as_slice(), &|| false).unwrap();
        assert!(matches!(
            receiver.publication(true, &|| false).unwrap(),
            Some(crate::usn::WatchUpdate::Record(_))
        ));
        for i in 0..3 {
            let p = fixture.0.join(format!("new-{i}"));
            std::fs::write(&p, vec![1; i + 2]).unwrap();
            let Some(crate::ntfs::Id::V2(id)) =
                crate::ntfs_native::inspect_file(&p, false, &checkpoint.volume)
                    .unwrap()
                    .reference
            else {
                panic!("파일 ID 필요")
            };
            source.events.push(crate::usn::Event {
                usn: source.journal.next,
                id,
                parent: checkpoint.root,
                reason: 0x80000100,
            });
            source.journal.next += 8;
            index.refresh(&mut source, &|| false).unwrap();
            let mut wire = Vec::new();
            sender.write(&mut wire, &mut index, &|| false).unwrap();
            receiver.receive(&mut wire.as_slice(), &|| false).unwrap();
            assert!(receiver.publication(false, &|| false).unwrap().is_none());
        }
        let Some(crate::usn::WatchUpdate::Record(result)) =
            receiver.publication(true, &|| false).unwrap()
        else {
            panic!("최신 완료 결과 필요")
        };
        assert_eq!(result.scan.files.len(), 4);
        assert_eq!(result.scan.folders[0].logical_bytes, 10);
        assert_eq!(result.tracking.unwrap().next, 124);
    }

    #[test]
    fn baseline_and_delta_preserve_paths_metadata_and_hard_links_without_resending_unchanged_files()
    {
        let fixture = Fixture::new();
        std::fs::write(fixture.0.join("한글.bin"), b"12345").unwrap();
        std::fs::hard_link(fixture.0.join("한글.bin"), fixture.0.join("alias.bin")).unwrap();
        for i in 0..100 {
            std::fs::write(fixture.0.join(format!("unchanged-{i}")), b"a").unwrap();
        }
        let before = record(&fixture.0);
        let mut index = Index::restore(&before).unwrap();
        let mut sender = Sender::new(17, Limits::default());
        let mut receiver = Receiver::new(fixture.0.clone(), true, Limits::default());
        let mut initial = Vec::new();
        sender.write(&mut initial, &mut index, &|| false).unwrap();
        assert!(receiver
            .receive(&mut initial.as_slice(), &|| false)
            .unwrap());
        let received = receiver.snapshot(&|| false).unwrap();
        assert_eq!(received.scan.folders[0].logical_bytes, 110);
        assert_eq!(received.scan.files.len(), 102);
        assert_eq!(
            received
                .tracking
                .as_ref()
                .unwrap()
                .files
                .iter()
                .filter(|&&id| id == before.tracking.as_ref().unwrap().files[0])
                .count(),
            2
        );
        std::fs::write(fixture.0.join("한글.bin"), b"123456789").unwrap();
        let before_state = before.tracking.as_ref().unwrap();
        let mut source = Source {
            blocked: None,
            clock: SystemTime::now(),
            journal: crate::usn::Journal {
                volume: before_state.volume.clone(),
                id: 7,
                first: 0,
                lowest: 0,
                next: 108,
            },
            events: vec![crate::usn::Event {
                usn: 100,
                id: before_state.files[0],
                parent: before_state.root,
                reason: 0x80000001,
            }],
        };
        index.refresh(&mut source, &|| false).unwrap();
        let mut delta = Vec::new();
        sender.write(&mut delta, &mut index, &|| false).unwrap();
        assert!(delta.len() < initial.len() / 4, "변경분 전송량 제한");
        assert!(receiver.receive(&mut delta.as_slice(), &|| false).unwrap());
        let result = receiver.snapshot(&|| false).unwrap();
        assert_eq!(result.scan.folders[0].logical_bytes, 118);
        assert_eq!(result.tracking.as_ref().unwrap().next, 108);
    }
}
