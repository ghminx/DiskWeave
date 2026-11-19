use super::{Advance, Checkpoint, Index, Node, Unavailable};
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
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
pub(super) fn node_cost(path: &Path, node: &Node) -> u64 {
    384 + std::mem::size_of::<Node>() as u64
        + (path.as_os_str().len() as u64) * 4
        + node.issue.as_ref().map_or(0, |s| s.len() as u64 * 2)
}
pub(super) fn unavailable_cost(path: &Path, item: &Unavailable) -> u64 {
    384 + std::mem::size_of::<Unavailable>() as u64
        + path.as_os_str().len() as u64 * 4
        + item.path.len() as u64 * 2
}
fn estimate(index: &Index, cancelled: &impl Fn() -> bool) -> io::Result<u64> {
    let mut cost = 0u64;
    for value in index.nodes.iter().map(|(p, n)| node_cost(p, n)).chain(
        index
            .unavailable
            .iter()
            .map(|(p, e)| unavailable_cost(p, e)),
    ) {
        crate::output::check(cancelled)?;
        cost = cost
            .checked_add(value)
            .ok_or_else(|| invalid("갱신 목록이 메모리 예산을 초과했습니다"))?;
    }
    Ok(cost)
}

pub struct Sender {
    units: u64,
    session: u64,
    revision: u64,
    limits: Limits,
}
impl Sender {
    pub fn new(session: u64, limits: Limits) -> Self {
        Self {
            units: 0,
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
        let baseline = self.revision == 0;
        let quiet = !baseline && index.changes.is_empty();
        let units = {
            let _sample = super::cost::measure(super::cost::Stage::SendBudget);
            if self.session == 0
                || index.nodes.len() + index.unavailable.len() > self.limits.entries
            {
                return Err(invalid("갱신 목록이 항목·메모리 예산을 초과했습니다"));
            }
            let units = if baseline {
                estimate(index, cancelled)?
            } else if quiet {
                self.units
            } else {
                u64::try_from(i128::from(self.units) + index.budget_change)
                    .map_err(|_| invalid("갱신 목록의 메모리 예산 합계 오류"))?
            };
            if units > self.limits.state_bytes / 2 {
                return Err(invalid("갱신 목록이 항목·메모리 예산을 초과했습니다"));
            }
            units
        };
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
            // 새 항목 전송 전에 기존 항목 감소분을 반영하여 수신 예산 유지.
            for growing in [false, true] {
                for change in &index.changes {
                    crate::output::check(cancelled)?;
                    if (change.budget_delta() > 0) == growing {
                        send(match change {
                            super::edit::Change::Node {
                                path,
                                expected,
                                value,
                                ..
                            } => Change::Node {
                                path: wide(path),
                                expected: *expected,
                                value: value.clone(),
                            },
                            super::edit::Change::Unavailable {
                                path,
                                present,
                                value,
                                ..
                            } => Change::Unavailable {
                                path: wide(path),
                                present: *present,
                                value: value.clone(),
                            },
                        })?;
                    }
                }
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
        self.units = units;
        index.changes.clear();
        index.budget_change = 0;
        Ok(stats)
    }
}
#[derive(Default)]
struct Validated {
    // 폴더 ID는 0, 파일 ID는 현재 분석 범위의 경로 개수로 보관.
    identities: HashMap<u64, i64>,
    logical: u64,
    allocation: u64,
}

fn file_sizes(node: &Node) -> (u64, u64) {
    if node.dir() || node.excluded() {
        (0, 0)
    } else {
        (node.bytes, node.extra.allocation.value().unwrap_or(0))
    }
}

/// 현재 스트림과 수신 검증을 통과한 발행 응답.
pub struct Publication(super::WatchUpdate);
impl Publication {
    #[cfg(test)]
    pub(crate) fn validated_fixture(update: super::WatchUpdate) -> Self {
        Self(update)
    }

    pub fn into_update(self) -> super::WatchUpdate {
        self.0
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
    validation: Validated,
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
            validation: Validated::default(),
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
                            if self
                                .validation
                                .identities
                                .get(&id)
                                .is_none_or(|count| *count == 0)
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
            let index = self.index.as_mut().unwrap();
            index.last_edit = Default::default();
            let checkpoint = &mut index.checkpoint;
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
        let baseline = self.index.is_none();
        let mut fresh = baseline.then(|| Index {
            root: self.root.clone(),
            detailed: self.detailed,
            nodes: super::entries::Entries::new(),
            unavailable: super::edit::Issues::new(),
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
            last_edit: Default::default(),
            budget_change: 0,
        });
        let index = if baseline {
            fresh.as_mut().unwrap()
        } else {
            self.index.as_mut().unwrap()
        };
        let mut candidate = super::edit::Edit::begin(index, self.limits.state_bytes / 2, baseline)?;
        let mut pending = Vec::new();
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
                                candidate.nodes.insert(p, value)?;
                            } else if expected.is_none() {
                                return Err(invalid("존재하지 않는 삭제 대상"));
                            } else {
                                candidate.nodes.remove(&p)?;
                            }
                        }
                        Change::Unavailable { present, value, .. } => {
                            if candidate.unavailable.contains_key(&p) != present
                                || baseline && present
                            {
                                return Err(invalid("확인 불가 항목 기준 불일치"));
                            }
                            if let Some(old) = candidate.unavailable.remove(&p)? {
                                units -= unavailable_cost(&p, &old);
                            }
                            if let Some(value) = value {
                                if value.relative() != p {
                                    return Err(invalid("확인 불가 항목 경로 불일치"));
                                }
                                units += unavailable_cost(&p, &value);
                                candidate.unavailable.insert(p, value)?;
                            } else if !present {
                                return Err(invalid("존재하지 않는 제외 해제 대상"));
                            }
                        }
                    }
                }
                Frame::Pending(ids) => {
                    for id in ids {
                        if id == 0 || !pending_seen.insert(id) {
                            return Err(invalid("대기 파일 ID 중복·누락"));
                        }
                        pending.push(id);
                    }
                }
                Frame::Begin { .. } | Frame::Failed(_) => {
                    return Err(invalid("잘못된 갱신 메시지"))
                }
            }
            frames += 1;
            if candidate.nodes.len() + candidate.unavailable.len() > self.limits.entries
                || pending.len() > self.limits.entries
                || units > self.limits.state_bytes / 2
                || units
                    .checked_add(candidate.undo_bytes())
                    .is_none_or(|n| n > self.limits.state_bytes)
                || frames > self.limits.entries as u64 * 4 + 8
            {
                return Err(invalid("갱신 목록이 항목·메모리 예산을 초과했습니다"));
            }
        }
        let validation = {
            let _sample = super::cost::measure(super::cost::Stage::ReceiveValidation);
            if baseline {
                Validation::Baseline(validate(&candidate, &pending, cancelled)?)
            } else {
                validate_delta(
                    &candidate,
                    seen.iter().map(|(_, path)| path.as_path()).collect(),
                    &self.validation,
                    &pending,
                    cancelled,
                )?
            }
        };
        let changed = candidate.commit(cancelled, false)?;
        let index = if baseline {
            fresh.as_mut().unwrap()
        } else {
            self.index.as_mut().unwrap()
        };
        index.checkpoint.next = header.next;
        index.checkpoint.pending = pending;
        index.checkpoint.confirmed_after = header.confirmed_after;
        if changed && index.scan.is_none() {
            index.checkpoint.folders.clear();
            index.checkpoint.files.clear();
            index.checkpoint.excluded.clear();
            index.checkpoint.unavailable.clear();
        }
        if baseline {
            self.index = fresh;
        }
        self.session = Some(session);
        self.revision = revision;
        self.units = units;
        self.finished = header.finished;
        validation.commit(&mut self.validation);
        self.dirty |= changed;
        Ok(changed)
    }
    #[allow(dead_code)] // 개발용 비교 도구의 수신 변경량 계측.
    pub fn edit_stats(&self) -> super::edit::Stats {
        self.index
            .as_ref()
            .map_or_else(Default::default, Index::edit_stats)
    }
    /// 모든 변경을 먼저 적용하고 소비자가 준비됐을 때만 완료 목록 재구성.
    pub fn pending(&self) -> bool {
        self.dirty
    }
    pub fn publication(
        &mut self,
        ready: bool,
        cancelled: &impl Fn() -> bool,
    ) -> io::Result<Option<Publication>> {
        if self.dirty {
            if !ready && self.published {
                return Ok(None);
            }
            let record = self.build_snapshot(cancelled, self.published)?;
            self.dirty = false;
            self.published = true;
            Ok(Some(Publication(super::WatchUpdate::Record(Box::new(
                record,
            )))))
        } else {
            Ok(Some(Publication(super::WatchUpdate::Cursor(
                self.advance()?,
            ))))
        }
    }
    #[allow(dead_code, reason = "개발용 비교 도구의 전체 Snapshot 검증 경계")]
    pub fn snapshot(
        &mut self,
        cancelled: &impl Fn() -> bool,
    ) -> io::Result<crate::snapshot::SavedScan> {
        self.build_snapshot(cancelled, false)
    }
    fn build_snapshot(
        &mut self,
        cancelled: &impl Fn() -> bool,
        allow_partial: bool,
    ) -> io::Result<crate::snapshot::SavedScan> {
        let index = self
            .index
            .as_mut()
            .ok_or_else(|| invalid("기준 목록 없음"))?;
        let mut record = index.snapshot(cancelled)?;
        record.finished = self.finished;
        let _sample = super::cost::measure(super::cost::Stage::SnapshotValidation);
        let checkpoint = record.tracking.as_ref().unwrap();
        if allow_partial && record.scan.partial.is_some() {
            // 이미 검증한 원본과 변경된 행으로 만든 내부 결과의 형상 확인.
            checkpoint.validate_shape(&record.scan)?;
        } else {
            checkpoint.validate(&record.scan)?;
        }
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
        let mut index = self.index.ok_or_else(|| invalid("기준 목록 없음"))?;
        index.nodes.enable_id_lookup();
        Ok(index)
    }
}
fn validate_root(index: &Index) -> io::Result<()> {
    let root = index
        .nodes
        .get(Path::new(""))
        .ok_or_else(|| invalid("추적 루트 누락"))?;
    if root.id != index.checkpoint.root || !root.dir() || root.excluded() {
        return Err(invalid("추적 루트 불일치"));
    }
    Ok(())
}

fn available_directory(index: &Index, path: &Path) -> bool {
    index.nodes.get(path).and_then(Node::directory_id).is_some()
        && !index.unavailable.get(path).is_some_and(|e| e.listing)
}

fn validate_unavailable(index: &Index, path: &Path, value: &Unavailable) -> io::Result<()> {
    if !path.parent().is_some_and(|parent| {
        available_directory(index, parent)
            && index.nodes.get(parent).and_then(Node::directory_id) == Some(value.parent)
    }) || !matches!(
        value.reason,
        super::UnavailableReason::Os(5 | 32 | 33)
            | super::UnavailableReason::Changing
            | super::UnavailableReason::Redirected
    ) || value.previous_id == Some(0)
        || matches!(value.reason, super::UnavailableReason::Changing) && value.listing
        || value.reason == super::UnavailableReason::Redirected
            && (value.listing || !value.directory)
        || value.listing
            && (!value.directory
                || index.nodes.get(path).and_then(Node::directory_id) != value.previous_id
                || value.previous_id.is_none())
        || !value.listing && index.nodes.contains_key(path)
    {
        return Err(invalid("확인 불가 항목 부모·상태 오류"));
    }
    Ok(())
}

enum Validation {
    Baseline(Validated),
    Delta {
        ids: HashMap<u64, Option<i64>>,
        logical: u64,
        allocation: u64,
    },
}
impl Validation {
    fn commit(self, current: &mut Validated) {
        match self {
            Self::Baseline(value) => *current = value,
            Self::Delta {
                ids,
                logical,
                allocation,
            } => {
                for (id, value) in ids {
                    if let Some(count) = value {
                        current.identities.insert(id, count);
                    } else {
                        current.identities.remove(&id);
                    }
                }
                current.logical = logical;
                current.allocation = allocation;
            }
        }
    }
}

fn validate_delta(
    index: &Index,
    paths: HashSet<&Path>,
    previous: &Validated,
    pending: &[u64],
    cancelled: &impl Fn() -> bool,
) -> io::Result<Validation> {
    validate_root(index)?;
    let mut changes = HashMap::<u64, (i64, i64)>::new();
    let mut logical = i128::from(previous.logical);
    let mut allocation = i128::from(previous.allocation);
    for path in paths {
        crate::output::check(cancelled)?;
        let after = index.nodes.get(path);
        let before = index.nodes.original(path);
        if after.is_some()
            && path
                .parent()
                .is_some_and(|parent| !available_directory(index, parent))
        {
            return Err(invalid("갱신 항목 부모 누락"));
        }
        if let Some(issue) = index.unavailable.get(path) {
            validate_unavailable(index, path, issue)?;
        }
        if before.and_then(Node::directory_id).is_some() {
            if !available_directory(index, path)
                && (index.nodes.subtree_keys(path).any(|child| child != path)
                    || index
                        .unavailable
                        .subtree_keys(path)
                        .any(|child| child != path))
            {
                return Err(invalid("갱신 항목 부모 누락"));
            }
            if before.and_then(Node::directory_id) != after.and_then(Node::directory_id) {
                for child in index.unavailable.subtree_keys(path) {
                    crate::output::check(cancelled)?;
                    if child.parent() == Some(path) {
                        validate_unavailable(index, child, &index.unavailable[child])?;
                    }
                }
            }
        }
        for (node, sign) in [(before, -1i64), (after, 1)] {
            let Some(node) = node.filter(|n| !n.excluded()) else {
                continue;
            };
            let (folders, files) = changes.entry(node.id).or_default();
            if node.dir() {
                *folders += sign;
            } else {
                *files += sign;
                let (size, allocated) = file_sizes(node);
                logical += i128::from(sign) * i128::from(size);
                allocation += i128::from(sign) * i128::from(allocated);
            }
        }
    }
    let mut ids = HashMap::with_capacity(changes.len());
    for (id, (folders, files)) in changes {
        crate::output::check(cancelled)?;
        let old = previous.identities.get(&id).copied();
        let folders = folders + i64::from(old == Some(0));
        let files = files + old.unwrap_or(0);
        let count = match (folders, files) {
            (0, 0) => None,
            (1, 0) => Some(0),
            (0, count) if count > 0 => Some(count),
            _ => return Err(invalid("폴더 ID 중복 또는 갱신 파일 경로 개수 오류")),
        };
        ids.insert(id, count);
    }
    for id in pending {
        crate::output::check(cancelled)?;
        if ids
            .get(id)
            .copied()
            .unwrap_or_else(|| previous.identities.get(id).copied())
            .is_none_or(|count| count == 0)
        {
            return Err(invalid("등록되지 않은 대기 파일 ID"));
        }
    }
    Ok(Validation::Delta {
        ids,
        logical: u64::try_from(logical).map_err(|_| invalid("논리 크기 합계 초과"))?,
        allocation: u64::try_from(allocation).map_err(|_| invalid("할당 크기 합계 초과"))?,
    })
}

fn validate(
    index: &Index,
    pending: &[u64],
    cancelled: &impl Fn() -> bool,
) -> io::Result<Validated> {
    validate_root(index)?;
    let mut dirs = HashMap::new();
    let mut files = HashMap::<u64, i64>::new();
    let mut bytes = 0u64;
    let mut allocation = 0u64;
    for (p, n) in &index.nodes {
        crate::output::check(cancelled)?;
        if p.parent()
            .is_some_and(|parent| !available_directory(index, parent))
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
            *files.entry(n.id).or_default() += 1;
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
        validate_unavailable(index, p, e)?;
    }
    if dirs.keys().any(|id| files.contains_key(id))
        || pending.iter().any(|id| !files.contains_key(id))
    {
        return Err(invalid("등록되지 않은 대기 파일 ID"));
    }
    files.extend(dirs.into_keys().map(|id| (id, 0)));
    Ok(Validated {
        identities: files,
        logical: bytes,
        allocation,
    })
}

#[cfg(test)]
pub(crate) mod tests {
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
        let branch = fixture.0.join("branch");
        let sibling = fixture.0.join("sibling");
        std::fs::create_dir(&branch).unwrap();
        std::fs::create_dir(&sibling).unwrap();
        std::fs::write(sibling.join("keep"), b"123456").unwrap();
        std::fs::write(branch.join("other"), b"12345").unwrap();
        let path = branch.join("file");
        std::fs::write(&path, b"123").unwrap();
        for i in 0..128 {
            std::fs::write(sibling.join(format!("unrelated-{i}")), []).unwrap();
        }
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
            .path_key(Path::new("branch\\file"))
            .unwrap();
        std::fs::write(&path, b"1234567").unwrap();
        let branch_row = before.scan.folder_index(Path::new("branch")).unwrap();
        let sibling_row = before.scan.folder_index(Path::new("sibling")).unwrap();
        let parent = before.tracking.as_ref().unwrap().folders[branch_row];
        event(&mut source, &path, parent, 0x80000001);
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
        for logical in [true, false] {
            let overflow = rewrite(&delta, |frame| {
                if let Frame::Change(Change::Node {
                    value: Some(node), ..
                }) = frame
                {
                    if node.bytes == 7 {
                        if logical {
                            node.bytes = u64::MAX;
                        } else {
                            node.extra.allocation = crate::metadata::Field::Value(u64::MAX);
                        }
                    }
                }
            });
            let error = receiver
                .receive(&mut overflow.as_slice(), &|| false)
                .unwrap_err();
            assert!(error.to_string().contains("합계 초과"), "{error}");
            assert_eq!(receiver.advance().unwrap().next, state.next);
        }
        let checks = std::cell::Cell::new(0);
        receiver
            .receive(&mut delta.as_slice(), &|| {
                checks.set(checks.get() + 1);
                checks.get() > 64
            })
            .expect("변경 없는 항목은 수신 재검증 범위에서 제외");
        let after = receiver.snapshot(&|| false).unwrap();
        assert_eq!(
            after
                .entries
                .as_ref()
                .unwrap()
                .path_key(Path::new("branch\\file")),
            Some(key)
        );
        assert!(
            Arc::ptr_eq(
                before.entries.as_ref().unwrap(),
                after.entries.as_ref().unwrap()
            ),
            "내용 변경에 불변인 경로/ID 대응표 재사용 필요"
        );
        assert!(!Arc::ptr_eq(&before.scan, &after.scan));
        assert!(!after.scan.subtree_unchanged_from(&before.scan, 0));
        assert!(after.scan.has_partial_update_from(&before.scan));
        assert!(after.scan.subtree_unchanged_from(&before.scan, sibling_row));
        assert!(!after.scan.subtree_unchanged_from(&before.scan, branch_row));
        assert_eq!(after.scan.folders[0].logical_bytes, 18);
        assert_eq!(after.scan.folders[branch_row].logical_bytes, 12);
        assert_eq!(after.scan.folders[0].file_count, 131);
        assert_eq!(after.scan.children[0][0], branch_row);
        let first = after.scan.file_children[branch_row][0];
        assert_eq!(after.scan.files[first].name, "file");
        let query = crate::search::SearchInput {
            min_size: "6".into(),
            ..Default::default()
        }
        .compile(0)
        .unwrap();
        let old_matches = crate::search::search(&before.scan, &query, || false).unwrap();
        let delta_matches =
            crate::search::refresh_matches(&before.scan, &after.scan, &query, old_matches, || {
                false
            })
            .unwrap()
            .unwrap();
        let full_matches = crate::search::search(&after.scan, &query, || false).unwrap();
        assert_eq!(delta_matches.items, full_matches.items);
        assert_eq!(delta_matches.unknown_dates, full_matches.unknown_dates);
        let independent = record(&fixture.0);
        for relative in ["", "branch", "sibling"] {
            let folder = |scan: &crate::scanner::ScanResult| {
                let row = scan.folder_index(Path::new(relative)).unwrap();
                let f = &scan.folders[row];
                (f.logical_bytes, f.allocation, f.file_count, f.folder_count)
            };
            assert_eq!(folder(&after.scan), folder(&independent.scan));
        }
    }
    #[test]
    fn single_file_deletion_does_not_revalidate_unrelated_paths() {
        let fixture = Fixture::new();
        let directory = fixture.0.join("delete-here");
        std::fs::create_dir(&directory).unwrap();
        let path = directory.join("probe");
        std::fs::write(&path, b"abc").unwrap();
        std::fs::hard_link(&path, fixture.0.join("alias")).unwrap();
        let unrelated = fixture.0.join("unrelated");
        std::fs::create_dir(&unrelated).unwrap();
        for i in 0..128 {
            std::fs::write(unrelated.join(format!("keep-{i}")), []).unwrap();
        }
        let saved = record(&fixture.0);
        let state = saved.tracking.as_ref().unwrap();
        let parent = state.folders[saved.scan.folder_index(Path::new("delete-here")).unwrap()];
        let mut source = Source {
            clock: SystemTime::now(),
            blocked: None,
            events: vec![],
            journal: crate::usn::Journal {
                volume: state.volume.clone(),
                id: 7,
                first: 0,
                lowest: 0,
                next: 100,
            },
        };
        let mut index = Index::restore(&saved).unwrap();
        let mut sender = Sender::new(17, Default::default());
        let mut receiver = Receiver::new(fixture.0.clone(), true, Default::default());
        let mut wire = vec![];
        sender.write(&mut wire, &mut index, &|| false).unwrap();
        receiver.receive(&mut wire.as_slice(), &|| false).unwrap();

        let id = event(&mut source, &path, parent, 0x80000200);
        std::fs::remove_file(&path).unwrap();
        let alias = fixture.0.join("alias");
        assert_eq!(event(&mut source, &alias, state.root, 1), id);
        index.refresh(&mut source, &|| false).unwrap();
        wire.clear();
        sender.write(&mut wire, &mut index, &|| false).unwrap();
        assert!(receiver.receive(&mut wire.as_slice(), &|| true).is_err());
        assert_eq!(receiver.advance().unwrap().next, state.next);
        let checks = std::cell::Cell::new(0);
        receiver
            .receive(&mut wire.as_slice(), &|| {
                checks.set(checks.get() + 1);
                checks.get() > 64
            })
            .expect("파일 한 경로 삭제에 무관한 전체 항목 검사 불필요");
        assert_eq!(receiver.advance().unwrap().pending, vec![id]);
        let after = receiver.snapshot(&|| false).unwrap();
        assert_eq!(after.scan.folders[0].logical_bytes, 3);
        assert_eq!(after.scan.files.len(), 129);
        assert_eq!(
            after
                .scan
                .files
                .iter()
                .filter(|file| file.name == "alias")
                .count(),
            1
        );

        let unrelated_id = state.folders[saved.scan.folder_index(Path::new("unrelated")).unwrap()];
        let keep_id = event(&mut source, &unrelated.join("keep-0"), unrelated_id, 1);
        assert_eq!(event(&mut source, &alias, state.root, 0x80000200), id);
        std::fs::remove_file(&alias).unwrap();
        index.refresh(&mut source, &|| false).unwrap();
        wire.clear();
        sender.write(&mut wire, &mut index, &|| false).unwrap();
        let invalid_pending = rewrite(&wire, |frame| {
            if let Frame::Pending(ids) = frame {
                assert_eq!(*ids, vec![keep_id]);
                *ids = vec![id];
            }
        });
        let error = receiver
            .receive(&mut invalid_pending.as_slice(), &|| false)
            .unwrap_err();
        assert!(error.to_string().contains("대기 파일 ID"), "{error}");
        let retained = receiver.snapshot(&|| false).unwrap();
        assert!(Arc::ptr_eq(&after.scan, &retained.scan));
        assert_eq!(after.tracking, retained.tracking);
        assert!(receiver.receive(&mut wire.as_slice(), &|| false).unwrap());
        let final_record = receiver.snapshot(&|| false).unwrap();
        assert_eq!(final_record.scan.folders[0].logical_bytes, 0);
        assert_eq!(final_record.scan.files.len(), 128);
    }

    #[test]
    fn mixed_file_changes_do_not_rebuild_unrelated_rows() {
        let fixture = Fixture::new();
        let root = &fixture.0;
        for name in ["a-remove", "m-keep", "z-tail"] {
            std::fs::create_dir(root.join(name)).unwrap();
        }
        let removed = root.join("a-remove/probe");
        std::fs::write(&removed, b"abc").unwrap();
        std::fs::write(root.join("z-tail/tail"), b"12345").unwrap();
        for i in 0..520 {
            std::fs::write(root.join(format!("m-keep/{i:04}")), []).unwrap();
        }
        let saved = record(root);
        let state = saved.tracking.as_ref().unwrap();
        let mut source = Source {
            clock: SystemTime::now(),
            blocked: None,
            events: vec![],
            journal: crate::usn::Journal {
                volume: state.volume.clone(),
                id: state.journal,
                first: 0,
                lowest: 0,
                next: state.next,
            },
        };
        let mut index = Index::begin(root, true, &mut source, &|| false).unwrap();
        let mut sender = Sender::new(17, Default::default());
        let mut receiver = Receiver::new(root.clone(), true, Default::default());
        let mut wire = vec![];
        sender.write(&mut wire, &mut index, &|| false).unwrap();
        receiver.receive(&mut wire.as_slice(), &|| false).unwrap();
        let previous = receiver.snapshot(&|| false).unwrap();
        let old_projection = previous.entries.as_ref().unwrap();
        let keep = old_projection.path_key(Path::new("m-keep/0300")).unwrap();
        let tail = old_projection.path_key(Path::new("z-tail/tail")).unwrap();
        let parent = state.folders[saved.scan.folder_index(Path::new("a-remove")).unwrap()];
        event(&mut source, &removed, parent, 0x80000200);
        std::fs::remove_file(removed).unwrap();
        let updated = root.join("m-keep/0000");
        let updated_parent = state.folders[saved.scan.folder_index(Path::new("m-keep")).unwrap()];
        std::fs::write(&updated, b"x").unwrap();
        event(&mut source, &updated, updated_parent, 0x80000001);
        let next_removed = root.join("m-keep/0519");
        event(&mut source, &next_removed, updated_parent, 0x80000200);
        std::fs::remove_file(next_removed).unwrap();
        let inserted = root.join("a-remove/inserted");
        std::fs::write(&inserted, b"ab").unwrap();
        event(&mut source, &inserted, parent, 0x80000100);

        index.refresh(&mut source, &|| false).unwrap();
        wire.clear();
        sender.write(&mut wire, &mut index, &|| false).unwrap();
        let checks = std::cell::Cell::new(0);
        assert!(receiver
            .receive(&mut wire.as_slice(), &|| {
                checks.set(checks.get() + 1);
                false
            })
            .unwrap());
        assert!(
            checks.get() < 256,
            "수신 취소 확인 {}회 / 송신 {:?} / 수신 {:?}",
            checks.get(),
            index.edit_stats(),
            receiver.edit_stats()
        );
        let current = receiver.snapshot(&|| false).unwrap();
        let entries = current.entries.as_ref().unwrap();
        assert_eq!(entries.item(keep), old_projection.item(keep));
        assert_eq!(entries.path_key(Path::new("z-tail/tail")), Some(tail));
        assert!(entries.path_key(Path::new("a-remove/probe")).is_none());
        assert!(entries.path_key(Path::new("m-keep/0519")).is_none());
        assert!(entries.path_key(Path::new("a-remove/inserted")).is_some());
        assert_eq!(current.scan.folders[0].logical_bytes, 8);
        assert_eq!(current.scan.files.len(), 521);
        let query = crate::search::SearchInput {
            text: "tail".into(),
            ..Default::default()
        }
        .compile(0)
        .unwrap();
        let old_results = crate::search::search(&previous.scan, &query, || false).unwrap();
        let new_results = crate::search::refresh_matches(
            &previous.scan,
            &current.scan,
            &query,
            old_results,
            || false,
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            new_results.items,
            crate::search::search(&current.scan, &query, || false)
                .unwrap()
                .items
        );

        let added = root.join("a-remove/new");
        std::fs::write(&added, b"1234567").unwrap();
        event(&mut source, &added, parent, 0x80000100);
        index.refresh(&mut source, &|| false).unwrap();
        wire.clear();
        sender.write(&mut wire, &mut index, &|| false).unwrap();
        checks.set(0);
        assert!(receiver
            .receive(&mut wire.as_slice(), &|| {
                checks.set(checks.get() + 1);
                checks.get() > 96
            })
            .unwrap());
        let added_record = receiver.snapshot(&|| false).unwrap();
        let added_entries = added_record.entries.as_ref().unwrap();
        assert_eq!(added_entries.item(keep), entries.item(keep));
        assert_eq!(added_entries.item(tail), entries.item(tail));
        let new_key = added_entries.path_key(Path::new("a-remove/new")).unwrap();
        let Some(crate::explorer::Item::File(row)) = added_entries.item(new_key) else {
            panic!("새 파일 행 필요");
        };
        assert_eq!(added_record.scan.files[row].logical_bytes, 7);
        assert_eq!(added_record.scan.folders[0].logical_bytes, 15);
        let query = crate::search::SearchInput {
            text: "new".into(),
            ..Default::default()
        }
        .compile(0)
        .unwrap();
        let old_results = crate::search::search(&current.scan, &query, || false).unwrap();
        assert!(old_results.items.is_empty());
        let results = crate::search::refresh_matches(
            &current.scan,
            &added_record.scan,
            &query,
            old_results,
            || false,
        )
        .unwrap()
        .unwrap();
        assert_eq!(results.items, vec![crate::explorer::Item::File(row)]);
    }

    #[test]
    fn repeated_unavailable_check_does_not_rebuild_unrelated_files_when_a_file_is_deleted() {
        let fixture = Fixture::new();
        let root = &fixture.0;
        for name in ["changes", "unrelated"] {
            std::fs::create_dir(root.join(name)).unwrap();
        }
        let deleted = root.join("changes/deleted");
        let blocked = root.join("changes/blocked");
        std::fs::write(&deleted, b"123").unwrap();
        std::fs::write(&blocked, b"hidden").unwrap();
        for row in 0..512 {
            std::fs::write(root.join(format!("unrelated/{row:04}")), b"x").unwrap();
        }
        let saved = record(root);
        let state = saved.tracking.as_ref().unwrap();
        let mut source = Source {
            clock: SystemTime::now(),
            blocked: Some(blocked),
            journal: crate::usn::Journal {
                volume: state.volume.clone(),
                id: state.journal,
                first: 0,
                lowest: 0,
                next: state.next,
            },
            events: vec![],
        };
        let mut index = Index::begin(root, true, &mut source, &|| false).unwrap();
        let mut sender = Sender::new(17, Default::default());
        let mut receiver = Receiver::new(root.clone(), true, Default::default());
        let mut wire = Vec::new();
        sender.write(&mut wire, &mut index, &|| false).unwrap();
        receiver.receive(&mut wire.as_slice(), &|| false).unwrap();
        assert!(receiver.publication(true, &|| false).unwrap().is_some());
        let before = receiver.snapshot(&|| false).unwrap();
        let before_checkpoint = before.tracking.as_ref().unwrap();
        assert_eq!(before_checkpoint.unavailable.len(), 1);
        let parent =
            before_checkpoint.folders[before.scan.folder_index(Path::new("changes")).unwrap()];

        source.clock += Duration::from_secs(31);
        event(&mut source, &deleted, parent, 0x80000200);
        std::fs::remove_file(&deleted).unwrap();
        index.refresh(&mut source, &|| false).unwrap();
        wire.clear();
        sender.write(&mut wire, &mut index, &|| false).unwrap();
        let checks = std::cell::Cell::new(0);
        let error = receiver
            .receive(&mut wire.as_slice(), &|| {
                checks.set(checks.get() + 1);
                checks.get() > 7
            })
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
        let kept = receiver.snapshot(&|| false).unwrap();
        assert!(Arc::ptr_eq(&kept.scan, &before.scan));
        assert_eq!(kept.tracking.as_ref(), Some(before_checkpoint));
        checks.set(0);
        assert!(receiver
            .receive(&mut wire.as_slice(), &|| {
                checks.set(checks.get() + 1);
                checks.get() > 96
            })
            .unwrap());
        let Some(published) = receiver.publication(true, &|| false).unwrap() else {
            panic!("삭제와 재확인 시각을 포함한 결과 필요");
        };
        let super::super::WatchUpdate::Record(after) = published.into_update() else {
            panic!("변경 목록 필요");
        };
        assert_eq!(after.scan.files.len(), 512);
        assert_eq!(after.scan.folders[0].logical_bytes, 512);
        assert_eq!(after.scan.error_count, 1);
        assert_eq!(
            after.tracking.as_ref().unwrap().unavailable[0].checked,
            source.clock
        );
        assert_same(&index.snapshot(&|| false).unwrap(), &after);
    }

    #[test]
    fn file_exclusion_and_recovery_only_revisit_affected_entries() {
        let fixture = Fixture::new();
        let root = &fixture.0;
        std::fs::create_dir(root.join("affected")).unwrap();
        std::fs::create_dir(root.join("unrelated")).unwrap();
        for row in 0..512 {
            std::fs::write(root.join(format!("unrelated/{row:04}")), b"x").unwrap();
        }
        let locked = root.join("affected/locked");
        let deleted = root.join("affected/deleted");
        std::fs::write(&locked, b"lock").unwrap();
        std::fs::write(&deleted, b"del").unwrap();
        let saved = record(root);
        let start = saved.tracking.as_ref().unwrap();
        let mut source = Source {
            clock: SystemTime::now(),
            blocked: None,
            journal: crate::usn::Journal {
                volume: start.volume.clone(),
                id: start.journal,
                first: 0,
                lowest: 0,
                next: start.next,
            },
            events: vec![],
        };
        let mut index = Index::begin(root, true, &mut source, &|| false).unwrap();
        let mut sender = Sender::new(17, Default::default());
        let mut receiver = Receiver::new(root.clone(), true, Default::default());
        let mut wire = Vec::new();
        sender.write(&mut wire, &mut index, &|| false).unwrap();
        receiver.receive(&mut wire.as_slice(), &|| false).unwrap();
        assert!(receiver.publication(true, &|| false).unwrap().is_some());
        let mut previous = receiver.snapshot(&|| false).unwrap();
        let branch = previous.scan.folder_index(Path::new("affected")).unwrap();
        let parent = previous.tracking.as_ref().unwrap().folders[branch];
        event(&mut source, &deleted, parent, 0x80000200);
        std::fs::remove_file(&deleted).unwrap();
        event(&mut source, &locked, parent, 0x80000001);
        source.blocked = Some(locked.clone());
        for (bytes, errors) in [(512, 1), (516, 0)] {
            index.refresh(&mut source, &|| false).unwrap();
            wire.clear();
            sender.write(&mut wire, &mut index, &|| false).unwrap();
            if errors == 1 {
                let corrupt = rewrite(&wire, |frame| {
                    if let Frame::Change(Change::Unavailable {
                        value: Some(issue), ..
                    }) = frame
                    {
                        issue.parent = start.root;
                    }
                });
                assert!(receiver
                    .receive(&mut corrupt.as_slice(), &|| false)
                    .is_err());
                let checks = std::cell::Cell::new(0);
                let error = receiver
                    .receive(&mut wire.as_slice(), &|| {
                        checks.set(checks.get() + 1);
                        checks.get() > 8
                    })
                    .unwrap_err();
                assert_eq!(error.kind(), io::ErrorKind::Interrupted);
                let kept = receiver.snapshot(&|| false).unwrap();
                assert!(Arc::ptr_eq(&kept.scan, &previous.scan));
                assert_eq!(kept.tracking, previous.tracking);
            }
            let checks = std::cell::Cell::new(0);
            assert!(receiver
                .receive(&mut wire.as_slice(), &|| {
                    checks.set(checks.get() + 1);
                    checks.get() > 128
                })
                .unwrap());
            assert!(receiver.publication(true, &|| false).unwrap().is_some());
            let current = receiver.snapshot(&|| false).unwrap();
            assert!(current.scan.has_partial_update_from(&previous.scan));
            assert_eq!(current.scan.folders[0].logical_bytes, bytes);
            assert_eq!(current.scan.error_count, errors);
            assert_same(&index.snapshot(&|| false).unwrap(), &current);
            previous = current;
            source.blocked = None;
            source.clock += Duration::from_secs(31);
        }
    }

    #[test]
    fn excluded_reparse_entries_do_not_rebuild_visible_file_rows() {
        let fixture = Fixture::new();
        let root = &fixture.0;
        std::fs::write(root.join("visible"), b"abc").unwrap();
        let saved = record(root);
        let before = saved.tracking.as_ref().unwrap();
        let mut source = Source {
            clock: SystemTime::now(),
            blocked: None,
            journal: crate::usn::Journal {
                volume: before.volume.clone(),
                id: before.journal,
                first: 0,
                lowest: 0,
                next: before.next,
            },
            events: vec![],
        };
        let mut index = Index::begin(root, true, &mut source, &|| false).unwrap();
        let mut sender = Sender::new(17, Default::default());
        let mut receiver = Receiver::new(root.clone(), true, Default::default());
        let mut wire = Vec::new();
        sender.write(&mut wire, &mut index, &|| false).unwrap();
        receiver.receive(&mut wire.as_slice(), &|| false).unwrap();
        let initial = receiver.snapshot(&|| false).unwrap();
        let alias = root.join("alias");
        std::fs::write(&alias, b"skip").unwrap();
        event(&mut source, &alias, before.root, 0x80000100);
        index.refresh(&mut source, &|| false).unwrap();
        wire.clear();
        sender.write(&mut wire, &mut index, &|| false).unwrap();
        let reparse = rewrite(&wire, |frame| {
            if let Frame::Change(Change::Node {
                path,
                value: Some(node),
                ..
            }) = frame
            {
                if path.as_slice() == crate::snapshot::wide(std::ffi::OsStr::new("alias")) {
                    node.attributes |= 0x400;
                }
            }
        });
        receiver
            .receive(&mut reparse.as_slice(), &|| false)
            .unwrap();
        let added = receiver.snapshot(&|| false).unwrap();
        assert!(added.scan.has_partial_update_from(&initial.scan));
        assert_eq!(added.scan.files.len(), 1);
        assert_eq!(added.scan.folders[0].logical_bytes, 3);
        assert_eq!(added.scan.skipped_reparse_points, 1);
        assert_eq!(added.tracking.as_ref().unwrap().excluded.len(), 1);

        event(&mut source, &alias, before.root, 0x80000200);
        std::fs::remove_file(&alias).unwrap();
        index.refresh(&mut source, &|| false).unwrap();
        wire.clear();
        sender.write(&mut wire, &mut index, &|| false).unwrap();
        receiver.receive(&mut wire.as_slice(), &|| false).unwrap();
        let removed = receiver.snapshot(&|| false).unwrap();
        assert!(removed.scan.has_partial_update_from(&added.scan));
        assert_eq!(removed.scan.files.len(), 1);
        assert_eq!(removed.scan.skipped_reparse_points, 0);
        assert!(removed.tracking.as_ref().unwrap().excluded.is_empty());
    }

    #[test]
    fn adding_nested_folders_and_deleting_a_file_preserves_unrelated_rows() {
        let fixture = Fixture::new();
        let root = &fixture.0;
        std::fs::create_dir(root.join("kept")).unwrap();
        let removed_folder = root.join("b-remove");
        std::fs::create_dir(&removed_folder).unwrap();
        for row in 0..512 {
            std::fs::write(root.join(format!("kept/{row:04}")), b"x").unwrap();
        }
        let file = root.join("delete");
        std::fs::write(&file, b"old").unwrap();
        let saved = record(root);
        let checkpoint = saved.tracking.as_ref().unwrap();
        let mut source = Source {
            clock: SystemTime::now(),
            blocked: None,
            journal: crate::usn::Journal {
                volume: checkpoint.volume.clone(),
                id: checkpoint.journal,
                first: 0,
                lowest: 0,
                next: checkpoint.next,
            },
            events: vec![],
        };
        let mut index = Index::begin(root, true, &mut source, &|| false).unwrap();
        let mut sender = Sender::new(17, Default::default());
        let mut receiver = Receiver::new(root.clone(), true, Default::default());
        let mut wire = Vec::new();
        sender.write(&mut wire, &mut index, &|| false).unwrap();
        receiver.receive(&mut wire.as_slice(), &|| false).unwrap();
        let before = receiver.snapshot(&|| false).unwrap();
        let kept = before.scan.folder_index(Path::new("kept")).unwrap();
        let directory = root.join("aaa");
        std::fs::create_dir_all(directory.join("child")).unwrap();
        std::fs::write(directory.join("child/file"), b"copy").unwrap();
        event(&mut source, &directory, checkpoint.root, 0x80000100);
        event(&mut source, &file, checkpoint.root, 0x80000200);
        std::fs::remove_file(&file).unwrap();
        index.refresh(&mut source, &|| false).unwrap();
        wire.clear();
        sender.write(&mut wire, &mut index, &|| false).unwrap();
        let duplicate = rewrite(&wire, |frame| {
            if let Frame::Change(Change::Node {
                path,
                value: Some(node),
                ..
            }) = frame
            {
                if path.as_slice() == crate::snapshot::wide(std::ffi::OsStr::new("aaa")) {
                    node.id = before.tracking.as_ref().unwrap().folders[kept];
                }
            }
        });
        let error = receiver
            .receive(&mut duplicate.as_slice(), &|| false)
            .unwrap_err();
        assert!(error.to_string().contains("폴더 ID 중복"));
        assert_eq!(receiver.advance().unwrap().next, checkpoint.next);
        let checks = std::cell::Cell::new(0);
        assert!(receiver
            .receive(&mut wire.as_slice(), &|| {
                checks.set(checks.get() + 1);
                checks.get() > 128
            })
            .unwrap());
        let after = receiver.snapshot(&|| false).unwrap();
        assert_eq!(after.scan.folder_index(Path::new("kept")).unwrap(), kept);
        assert!(after.scan.has_partial_update_from(&before.scan));
        assert_eq!(after.scan.folders[0].logical_bytes, 516);
        assert_eq!(after.scan.folders[0].folder_count, 4);
        let child = after.scan.folder_index(Path::new("aaa/child")).unwrap();
        assert_eq!(after.scan.folders[child].logical_bytes, 4);
        assert_eq!(after.scan.file_children[child].len(), 1);

        let query = crate::search::SearchInput {
            text: "aaa".into(),
            ..Default::default()
        }
        .compile(0)
        .unwrap();
        let old = crate::search::search(&before.scan, &query, || false).unwrap();
        let result =
            crate::search::refresh_matches(&before.scan, &after.scan, &query, old, || false)
                .unwrap()
                .unwrap();
        assert_eq!(
            result.items,
            crate::search::search(&after.scan, &query, || false)
                .unwrap()
                .items
        );
        event(&mut source, &removed_folder, checkpoint.root, 0x80000200);
        std::fs::remove_dir(removed_folder).unwrap();
        index.refresh(&mut source, &|| false).unwrap();
        wire.clear();
        sender.write(&mut wire, &mut index, &|| false).unwrap();
        let orphan = rewrite(&wire, |frame| {
            if let Frame::Change(Change::Node {
                path,
                expected,
                value: None,
            }) = frame
            {
                if path.as_slice() == crate::snapshot::wide(std::ffi::OsStr::new("b-remove")) {
                    *path = crate::snapshot::wide(std::ffi::OsStr::new("aaa"));
                    let folder = after.scan.folder_index(Path::new("aaa")).unwrap();
                    *expected = Some(after.tracking.as_ref().unwrap().folders[folder]);
                }
            }
        });
        let error = receiver
            .receive(&mut orphan.as_slice(), &|| false)
            .unwrap_err();
        assert!(error.to_string().contains("부모 누락"));
        assert!(Arc::ptr_eq(
            &receiver.snapshot(&|| false).unwrap().scan,
            &after.scan
        ));
        checks.set(0);
        assert!(receiver
            .receive(&mut wire.as_slice(), &|| {
                checks.set(checks.get() + 1);
                checks.get() > 128
            })
            .unwrap());
        let compacted = receiver.snapshot(&|| false).unwrap();
        assert!(compacted.scan.has_partial_update_from(&after.scan));
        assert!(compacted.scan.folder_index(Path::new("b-remove")).is_err());
        assert_eq!(compacted.scan.folders[0].folder_count, 3);
        assert_eq!(compacted.scan.folders[0].logical_bytes, 516);
        let ancestor = compacted.scan.folder_index(Path::new("aaa")).unwrap();
        let child = compacted.scan.folder_index(Path::new("aaa/child")).unwrap();
        assert!(ancestor < child);
        assert_eq!(compacted.scan.folders[child].parent, Some(ancestor));
        assert_eq!(
            compacted.scan.files[compacted.scan.file_children[child][0]].parent,
            child
        );
        let updated =
            crate::search::refresh_matches(&after.scan, &compacted.scan, &query, result, || false)
                .unwrap()
                .unwrap();
        assert_eq!(
            updated.items,
            crate::search::search(&compacted.scan, &query, || false)
                .unwrap()
                .items
        );
    }

    #[test]
    fn structural_delta_budget_failure_rolls_back_deleted_paths_and_can_retry_the_same_revision() {
        let fixture = Fixture::new();
        for n in 0..20 {
            std::fs::write(fixture.0.join(format!("a-{n:02}")), b"123").unwrap();
        }
        let saved = record(&fixture.0);
        let state = saved.tracking.as_ref().unwrap();
        let mut source = Source {
            clock: SystemTime::now(),
            blocked: None,
            events: vec![],
            journal: crate::usn::Journal {
                volume: state.volume.clone(),
                id: 7,
                first: 0,
                lowest: 0,
                next: 100,
            },
        };
        let mut index = Index::restore(&saved).unwrap();
        let mut sender = Sender::new(17, Default::default());
        let mut receiver = Receiver::new(fixture.0.clone(), true, Default::default());
        let mut wire = vec![];
        sender.write(&mut wire, &mut index, &|| false).unwrap();
        receiver.receive(&mut wire.as_slice(), &|| false).unwrap();
        let before = receiver.snapshot(&|| false).unwrap();
        for n in 0..20 {
            let new = fixture.0.join(format!("z-{n:02}"));
            std::fs::rename(fixture.0.join(format!("a-{n:02}")), &new).unwrap();
            event(&mut source, &new, state.root, 0x80003000);
        }
        index.refresh(&mut source, &|| false).unwrap();
        wire.clear();
        sender.write(&mut wire, &mut index, &|| false).unwrap();
        receiver.limits.state_bytes = receiver.units * 2;
        let revision = receiver.revision;
        let error = receiver
            .receive(&mut wire.as_slice(), &|| false)
            .unwrap_err();
        assert!(error.to_string().contains("예산"), "{error}");
        let kept = receiver.snapshot(&|| false).unwrap();
        assert!(Arc::ptr_eq(&kept.scan, &before.scan));
        assert!(Arc::ptr_eq(
            kept.entries.as_ref().unwrap(),
            before.entries.as_ref().unwrap()
        ));
        assert_eq!(kept.tracking, before.tracking);
        assert_eq!(receiver.revision, revision);
        receiver.limits = Default::default();
        assert!(receiver.receive(&mut wire.as_slice(), &|| false).unwrap());
        let after = receiver.snapshot(&|| false).unwrap();
        assert_eq!(after.scan.folders[0].logical_bytes, 60);
        assert!(after.scan.has_partial_update_from(&before.scan));
        assert_eq!(after.scan.files.len(), 20);
        assert!(after
            .scan
            .files
            .iter()
            .all(|file| file.name.to_string_lossy().starts_with("z-")));
        for n in 0..20 {
            let key = before
                .entries
                .as_ref()
                .unwrap()
                .path_key(Path::new(&format!("a-{n:02}")))
                .unwrap();
            assert!(after.entries.as_ref().unwrap().item(key).is_none());
        }
        assert!((40..=41).contains(&receiver.edit_stats().changed_entries));
    }
    #[test]
    fn file_create_move_and_delete_update_both_branches_without_reusing_shifted_rows() {
        let fixture = Fixture::new();
        let root = &fixture.0;
        for folder in ["a", "b", "z"] {
            std::fs::create_dir(root.join(folder)).unwrap();
        }
        std::fs::write(root.join("z/stable"), b"123").unwrap();
        // 구조 갱신 뒤에도 앞쪽의 논리 행과 저장소가 그대로 유지되는 자료.
        for i in 0..260 {
            std::fs::write(root.join(format!("{i:04}.bin")), []).unwrap();
        }
        let saved = record(root);
        let state = saved.tracking.as_ref().unwrap();
        let mut source = Source {
            clock: SystemTime::now(),
            blocked: None,
            events: vec![],
            journal: super::super::Journal {
                volume: state.volume.clone(),
                id: 7,
                first: 0,
                lowest: 0,
                next: 100,
            },
        };
        let mut index = Index::restore(&saved).unwrap();
        let mut sender = Sender::new(17, Default::default());
        let mut receiver = Receiver::new(root.clone(), true, Default::default());
        let mut wire = vec![];
        sender.write(&mut wire, &mut index, &|| false).unwrap();
        receiver.receive(&mut wire.as_slice(), &|| false).unwrap();
        let mut previous = receiver.snapshot(&|| false).unwrap();
        let a = previous.scan.folder_index(Path::new("a")).unwrap();
        let b = previous.scan.folder_index(Path::new("b")).unwrap();
        let z = previous.scan.folder_index(Path::new("z")).unwrap();
        let folders = &previous.tracking.as_ref().unwrap().folders;
        let (a_id, b_id) = (folders[a], folders[b]);
        let from = root.join("a/file");
        let to = root.join("b/file");
        for (stage, totals) in [(0, [7, 0, 3]), (1, [0, 7, 3]), (2, [0, 0, 3])] {
            match stage {
                0 => {
                    std::fs::write(&from, b"1234567").unwrap();
                    event(&mut source, &from, a_id, 0x80000100);
                }
                1 => {
                    event(&mut source, &from, a_id, 0x80001000);
                    std::fs::rename(&from, &to).unwrap();
                    event(&mut source, &to, b_id, 0x80002000);
                }
                _ => {
                    event(&mut source, &to, b_id, 0x80000200);
                    std::fs::remove_file(&to).unwrap();
                }
            }
            tick(&mut source, &mut index, &mut sender, &mut receiver);
            let current = receiver.snapshot(&|| false).unwrap();
            assert!(current.scan.files.shares_row_with(&previous.scan.files, 0));
            assert!(current.scan.has_partial_update_from(&previous.scan));
            assert_eq!(
                current.scan.subtree_unchanged_from(&previous.scan, z),
                current.scan.file_children[z] == previous.scan.file_children[z],
                "다른 폴더의 행 주소가 실제로 바뀐 경우에만 다시 계산 (단계 {stage})"
            );
            let query = crate::search::SearchInput {
                text: "file".into(),
                ..Default::default()
            }
            .compile(0)
            .unwrap();
            let old_matches = crate::search::search(&previous.scan, &query, || false).unwrap();
            let patched = crate::search::refresh_matches(
                &previous.scan,
                &current.scan,
                &query,
                old_matches,
                || false,
            )
            .unwrap()
            .unwrap();
            let full = crate::search::search(&current.scan, &query, || false).unwrap();
            assert_eq!(patched.items, full.items);
            let independent = record(root);
            for ((name, row), bytes) in [("a", a), ("b", b), ("z", z)].into_iter().zip(totals) {
                let actual = &current.scan.folders[row];
                let expected = &independent.scan.folders
                    [independent.scan.folder_index(Path::new(name)).unwrap()];
                assert_eq!(actual.logical_bytes, bytes);
                assert_eq!(actual.file_count, expected.file_count);
                assert_eq!(actual.allocation, expected.allocation);
            }
            previous = current;
        }
    }

    #[test]
    fn sender_budget_tracks_path_growth_without_consuming_a_failed_revision() {
        let fixture = Fixture::new();
        let path = fixture.0.join("file");
        std::fs::write(&path, b"123").unwrap();
        let saved = record(&fixture.0);
        let state = saved.tracking.as_ref().unwrap();
        let mut source = Source {
            clock: SystemTime::now(),
            blocked: None,
            events: vec![],
            journal: super::super::Journal {
                volume: state.volume.clone(),
                id: 7,
                first: 0,
                lowest: 0,
                next: 100,
            },
        };
        let mut index = Index::restore(&saved).unwrap();
        let baseline_cost = estimate(&index, &|| false).unwrap();
        let mut sender = Sender::new(
            17,
            Limits {
                state_bytes: baseline_cost * 2,
                ..Default::default()
            },
        );
        let mut receiver = Receiver::new(fixture.0.clone(), true, Default::default());
        let mut baseline = Vec::new();
        sender.write(&mut baseline, &mut index, &|| false).unwrap();
        receiver
            .receive(&mut baseline.as_slice(), &|| false)
            .unwrap();
        let moved = fixture.0.join("longer-name-for-budget.bin");
        std::fs::rename(&path, &moved).unwrap();
        event(&mut source, &moved, state.root, 0x80003000);
        assert!(index.refresh(&mut source, &|| false).unwrap());
        let mut wire = vec![];
        assert!(sender.write(&mut wire, &mut index, &|| false).is_err());
        assert!(wire.is_empty());
        sender.limits = Default::default();
        sender.write(&mut wire, &mut index, &|| false).unwrap();
        assert!(receiver.receive(&mut wire.as_slice(), &|| false).unwrap());
        assert_eq!(
            receiver.snapshot(&|| false).unwrap().scan.files[0].name,
            "longer-name-for-budget.bin"
        );
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
            assert!(Arc::ptr_eq(
                before.entries.as_ref().unwrap(),
                kept.entries.as_ref().unwrap()
            ));
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

    #[test]
    fn disappearing_child_folder_is_removed_or_recollected_without_stopping_tracking() {
        struct Vanishes {
            inner: Source,
            folder: Option<PathBuf>,
            replacement: bool,
        }
        impl crate::usn::Source for Vanishes {
            fn journal(&mut self) -> io::Result<crate::usn::Journal> {
                self.inner.journal()
            }
            fn read(&mut self, start: i64, end: i64) -> io::Result<Vec<crate::usn::Event>> {
                self.inner.read(start, end)
            }
            fn inspect(&mut self, path: &Path, detailed: bool) -> io::Result<crate::ntfs::Info> {
                self.inner.inspect(path, detailed)
            }
            fn list(&mut self, path: &Path) -> io::Result<Vec<PathBuf>> {
                if Some(path) != self.folder.as_deref() {
                    return self.inner.list(path);
                }
                self.folder = None;
                std::fs::remove_file(path.join("probe")).unwrap();
                std::fs::remove_dir(path).unwrap();
                let missing = self.inner.list(path);
                if self.replacement {
                    std::fs::create_dir(path).unwrap();
                    std::fs::write(path.join("new"), b"1234567").unwrap();
                }
                missing
            }
        }
        for replacement in [false, true] {
            let fixture = Fixture::new();
            let folder = fixture.0.join("child");
            std::fs::create_dir(&folder).unwrap();
            let path = folder.join("probe");
            std::fs::write(&path, b"abc").unwrap();
            std::fs::write(fixture.0.join("keep"), b"12345").unwrap();
            let saved = record(&fixture.0);
            let state = saved.tracking.as_ref().unwrap();
            let mut source = Source {
                clock: SystemTime::now(),
                blocked: None,
                journal: crate::usn::Journal {
                    volume: state.volume.clone(),
                    id: state.journal,
                    first: 0,
                    lowest: 0,
                    next: state.next,
                },
                events: vec![],
            };
            let parent = state.folders[saved.scan.folder_index(Path::new("child")).unwrap()];
            event(&mut source, &path, parent, 0x80000200);
            let next = source.journal.next;
            let mut index = Index::restore(&saved).unwrap();
            assert!(index
                .refresh(
                    &mut Vanishes {
                        inner: source,
                        folder: Some(folder),
                        replacement,
                    },
                    &|| false
                )
                .unwrap());
            let after = index.snapshot(&|| false).unwrap();
            assert_eq!(after.tracking.as_ref().unwrap().next, next);
            assert_eq!(after.scan.folders.len(), if replacement { 2 } else { 1 });
            assert_eq!(
                after.scan.folders[0].logical_bytes,
                if replacement { 12 } else { 5 }
            );
        }
    }

    #[test]
    fn rename_at_the_entry_limit_preserves_the_final_count_and_accepts_both_directions() {
        let fixture = Fixture::new();
        let a = fixture.0.join("a.txt");
        let z = fixture.0.join("z.txt");
        std::fs::write(&z, b"abc").unwrap();
        let saved = record(&fixture.0);
        let state = saved.tracking.as_ref().unwrap();
        let mut source = Source {
            clock: SystemTime::now(),
            blocked: None,
            journal: crate::usn::Journal {
                volume: state.volume.clone(),
                id: state.journal,
                first: 0,
                lowest: 0,
                next: state.next,
            },
            events: vec![],
        };
        let mut index = Index::restore(&saved).unwrap();
        let limits = Limits {
            entries: 2,
            ..Limits::default()
        };
        let mut sender = Sender::new(17, limits);
        let mut receiver = Receiver::new(fixture.0.clone(), saved.scan.detailed, limits);
        let mut wire = vec![];
        sender.write(&mut wire, &mut index, &|| false).unwrap();
        receiver.receive(&mut wire.as_slice(), &|| false).unwrap();
        for (old, new) in [(&z, &a), (&a, &z)] {
            event(&mut source, old, state.root, 0x80001000);
            std::fs::rename(old, new).unwrap();
            event(&mut source, new, state.root, 0x80002000);
            assert!(index.refresh(&mut source, &|| false).unwrap());
            wire.clear();
            sender.write(&mut wire, &mut index, &|| false).unwrap();
            let previous = receiver.advance().unwrap().next;
            let attempts = std::cell::Cell::new(0);
            assert!(receiver
                .receive(&mut wire.as_slice(), &|| {
                    attempts.set(attempts.get() + 1);
                    attempts.get() > 2
                })
                .is_err());
            assert_eq!(receiver.advance().unwrap().next, previous);
            assert!(receiver.receive(&mut wire.as_slice(), &|| false).unwrap());
            let updated = receiver.snapshot(&|| false).unwrap();
            assert_eq!(updated.scan.files.len(), 1);
            assert_eq!(&updated.scan.files[0].name, new.file_name().unwrap());
            assert_eq!(updated.scan.folders[0].logical_bytes, 3);
        }
        let extra = fixture.0.join("extra");
        std::fs::write(&extra, b"x").unwrap();
        event(&mut source, &extra, state.root, 0x80000100);
        index.refresh(&mut source, &|| false).unwrap();
        wire.clear();
        assert!(sender.write(&mut wire, &mut index, &|| false).is_err());
        assert!(wire.is_empty());
    }

    #[test]
    fn validated_stream_publication_keeps_cursor_and_location_bound_to_the_result() {
        let fixture = Fixture::new();
        let other = Fixture::new();
        std::fs::write(fixture.0.join("probe"), b"abc").unwrap();
        let saved = record(&fixture.0);
        let expected_next = saved.tracking.as_ref().unwrap().next;
        let mut index = Index::restore(&saved).unwrap();
        let mut sender = Sender::new(17, Default::default());
        let mut wire = Vec::new();
        sender.write(&mut wire, &mut index, &|| false).unwrap();
        index.checkpoint.next += 8;
        sender.write(&mut wire, &mut index, &|| false).unwrap();

        let mut locations = crate::locations::Locations::default();
        let id = locations.add(&fixture.0).unwrap();
        let other_id = locations.add(&other.0).unwrap();
        let (done, completed) = std::sync::mpsc::channel();
        let mut controller = crate::tracking::Controller::default();
        controller
            .start_verified_with(
                id,
                fixture.0.clone(),
                saved.scan.detailed,
                None,
                move |root, detailed, _, cancelled, publish| {
                    let mut receiver = Receiver::new(root.to_owned(), detailed, Default::default());
                    let mut frames = wire.as_slice();
                    for _ in 0..2 {
                        receiver.receive(&mut frames, &cancelled)?;
                        publish(receiver.publication(true, &cancelled)?.unwrap())?;
                    }
                    done.send(()).unwrap();
                    Ok(())
                },
            )
            .unwrap();
        completed.recv_timeout(Duration::from_secs(5)).unwrap();
        let Some(crate::tracking::Update::Record(result)) = controller.poll() else {
            panic!("검증된 결과 필요");
        };
        assert_eq!(result.tracking.as_ref().unwrap().next, expected_next + 8);
        assert!(locations.accept_published(other_id, &result).is_err());
        assert!(locations.accept_published(id, &result).unwrap());
        assert_eq!(
            locations
                .get(id)
                .unwrap()
                .scan
                .completed()
                .unwrap()
                .result
                .folders[0]
                .logical_bytes,
            3
        );
    }

    fn assert_same(expected: &crate::snapshot::SavedScan, actual: &crate::snapshot::SavedScan) {
        let mut a = Vec::new();
        let mut b = Vec::new();
        crate::snapshot::write_to(expected, &mut a, || false).unwrap();
        crate::snapshot::write_to(actual, &mut b, || false).unwrap();
        let mut a: serde_json::Value = serde_json::from_slice(&a).unwrap();
        let mut b: serde_json::Value = serde_json::from_slice(&b).unwrap();
        // 내부 파일 행의 순서와 별개로 각 경로의 메타데이터와 기준점 ID 일치 확인.
        let files_by_path = |scan: &mut serde_json::Value| {
            let files = scan["files"].take();
            let ids = scan["tracking"]["files"].take();
            let files = files.as_array().unwrap();
            let ids = ids.as_array().unwrap();
            assert_eq!(files.len(), ids.len());
            files
                .iter()
                .zip(ids)
                .map(|(file, id)| {
                    let parent = file["parent"].as_u64().unwrap();
                    let name = serde_json::from_value::<Vec<u16>>(file["name"].clone()).unwrap();
                    ((parent, name), (file.clone(), id.clone()))
                })
                .collect::<std::collections::BTreeMap<_, _>>()
        };
        let expected_files = files_by_path(&mut a);
        let actual_files = files_by_path(&mut b);
        assert_eq!(expected_files.len(), actual_files.len());
        for (path, file) in &expected_files {
            assert_eq!(Some(file), actual_files.get(path), "{path:?}");
        }
        for field in [
            "folders",
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
    pub(crate) fn folder_move_snapshots() -> (crate::locations::Snapshot, crate::locations::Snapshot)
    {
        let fixture = Fixture::new();
        let root = &fixture.0;
        for row in 0..520 {
            std::fs::create_dir(root.join(format!("{row:04}"))).unwrap();
        }
        std::fs::create_dir(root.join("a")).unwrap();
        std::fs::create_dir(root.join("z")).unwrap();
        std::fs::write(root.join("a/moved.bin"), b"12345").unwrap();
        std::fs::write(root.join("z/pinned.bin"), b"123").unwrap();
        let saved = record(root);
        let state = saved.tracking.as_ref().unwrap();
        let mut source = Source {
            clock: SystemTime::now(),
            blocked: None,
            events: vec![],
            journal: super::super::Journal {
                volume: state.volume.clone(),
                id: 7,
                first: 0,
                lowest: 0,
                next: 100,
            },
        };
        let mut index = Index::restore(&saved).unwrap();
        let mut sender = Sender::new(17, Default::default());
        let mut receiver = Receiver::new(root.clone(), true, Default::default());
        let mut wire = vec![];
        sender.write(&mut wire, &mut index, &|| false).unwrap();
        receiver.receive(&mut wire.as_slice(), &|| false).unwrap();
        let previous = receiver.snapshot(&|| false).unwrap();
        let from = root.join("a");
        let to = root.join("z/a");
        let folder = previous.scan.folder_index(Path::new("z")).unwrap();
        let target = previous.tracking.as_ref().unwrap().folders[folder];
        event(&mut source, &from, state.root, 0x80001000);
        std::fs::rename(&from, &to).unwrap();
        event(&mut source, &to, target, 0x80002000);
        tick(&mut source, &mut index, &mut sender, &mut receiver);
        let current = receiver.snapshot(&|| false).unwrap();
        let snapshot =
            |record: crate::snapshot::SavedScan, generation| crate::locations::Snapshot {
                id: 1,
                generation,
                scan: record.scan,
                entries: record.entries,
            };
        (snapshot(previous, 1), snapshot(current, 2))
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
        let before_move = receiver.snapshot(&|| false).unwrap();
        let sub = event(&mut source, &fixture.0.join("sub"), state.root, 0x80003000);
        std::fs::rename(fixture.0.join("sub"), fixture.0.join("renamed")).unwrap();
        tick(&mut source, &mut index, &mut sender, &mut receiver);
        let after_move = receiver.snapshot(&|| false).unwrap();
        assert!(after_move.scan.has_partial_update_from(&before_move.scan));
        assert_eq!(
            after_move.scan.folders[0].folder_count,
            before_move.scan.folders[0].folder_count
        );
        for text in ["sub", "renamed", ""] {
            let query = crate::search::SearchInput {
                text: text.into(),
                ..Default::default()
            }
            .compile(0)
            .unwrap();
            let original = crate::search::search(&before_move.scan, &query, || false).unwrap();
            let updated = crate::search::refresh_matches(
                &before_move.scan,
                &after_move.scan,
                &query,
                original,
                || false,
            )
            .unwrap()
            .expect("폴더 이동 검색의 증분 반영");
            let expected = crate::search::search(&after_move.scan, &query, || false).unwrap();
            assert_eq!(updated.items, expected.items);
        }
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
            receiver
                .publication(true, &|| false)
                .unwrap()
                .map(Publication::into_update),
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
        let Some(crate::usn::WatchUpdate::Record(result)) = receiver
            .publication(true, &|| false)
            .unwrap()
            .map(Publication::into_update)
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
