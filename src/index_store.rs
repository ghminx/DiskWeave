use crate::snapshot::SavedScan;
use serde::{Deserialize, Serialize};
use std::io::Read;
use std::os::windows::{
    ffi::{OsStrExt, OsStringExt},
    fs::OpenOptionsExt,
};
use std::{
    collections::HashSet,
    fs, io,
    path::{Path, PathBuf},
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

pub const CATALOG_NAME: &str = "catalog.json";
const PREVIOUS_NAME: &str = "catalog.previous.json";
#[derive(Clone, Default)]
pub struct Catalog {
    pub locations: Vec<CachedLocation>,
    pub selected: Option<u64>,
}
#[derive(Clone)]
pub struct CachedLocation {
    pub id: u64,
    pub path: PathBuf,
    pub identity: PathBuf,
    pub enabled: bool,
    pub stale: bool,
    pub generation: u64,
    pub record: Option<Arc<SavedScan>>,
}
pub struct Loaded {
    pub catalog: Catalog,
    pub enabled: bool,
    pub writable: bool,
    pub notice: Option<String>,
}
#[derive(Clone, Copy, Debug)]
pub enum Phase {
    Location { current: usize, total: usize },
    Publishing,
}
#[derive(Clone, Serialize, Deserialize)]
struct Manifest {
    format: String,
    version: u32,
    sequence: u64,
    selected: Option<u64>,
    entries: Vec<Entry>,
}
#[derive(Clone, Serialize, Deserialize)]
struct Entry {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cursor: Option<Cursor>,
    id: u64,
    path: Vec<u16>,
    identity: Vec<u16>,
    enabled: bool,
    stale: bool,
    generation: u64,
    snapshot: Option<String>,
}
#[derive(Clone, Serialize, Deserialize)]
struct Cursor {
    next: i64,
    pending: Vec<u64>,
    checked: crate::snapshot::Time,
}
#[derive(Serialize, Deserialize)]
struct Preference {
    version: u32,
    enabled: bool,
}
pub struct Repository {
    _writer: Option<fs::File>,
    root: PathBuf,
    current: Option<Manifest>,
    cached: Catalog,
    loaded: bool,
    enabled: bool,
    cleanup_notice: Option<String>,
}
fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}
fn read_bounded(path: &Path, limit: u64) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    fs::File::open(path)?
        .take(limit + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        return Err(invalid("저장된 목록 또는 설정의 크기 제한을 초과했습니다"));
    }
    Ok(bytes)
}
fn wide(path: &Path) -> Vec<u16> {
    path.as_os_str().encode_wide().collect()
}
fn path(value: &[u16]) -> io::Result<PathBuf> {
    let p = PathBuf::from(std::ffi::OsString::from_wide(value));
    if value.len() > 32767 || value.contains(&0) || !p.is_absolute() {
        return Err(invalid("저장된 위치 경로가 올바르지 않습니다"));
    }
    Ok(p)
}
fn snapshot_name(name: &str) -> bool {
    name.strip_prefix("scan-")
        .and_then(|s| s.strip_suffix(".dwsnap"))
        .is_some_and(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit() || b == b'-'))
}
impl Repository {
    pub fn open(root: &Path) -> io::Result<Self> {
        let root = std::path::absolute(root)?;
        fs::create_dir_all(&root)?;
        let writer = match fs::File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .share_mode(1)
            .open(root.join("writer.lock"))
        {
            Ok(file) => Some(file),
            Err(e) if e.raw_os_error() == Some(32) => None,
            Err(e) => return Err(e),
        };
        if writer.is_some() && !root.join("readers.lock").exists() {
            fs::File::options()
                .write(true)
                .create(true)
                .truncate(false)
                .open(root.join("readers.lock"))?;
        }
        Ok(Self {
            _writer: writer,
            root,
            current: None,
            cached: Catalog::default(),
            loaded: false,
            enabled: true,
            cleanup_notice: None,
        })
    }
    pub fn enabled(&self) -> bool {
        self.enabled
    }
    pub fn writable(&self) -> bool {
        self._writer.is_some()
    }
    fn read_manifest(
        &self,
        name: &str,
        cancelled: &impl Fn() -> bool,
        progress: &mut impl FnMut(Phase),
    ) -> io::Result<(Manifest, Catalog)> {
        crate::output::check(cancelled)?;
        let file = self.root.join(name);
        let value: serde_json::Value = serde_json::from_slice(&read_bounded(&file, 1024 * 1024)?)
            .map_err(crate::output::json_error)?;
        if value["format"] != "DiskWeaveIndex" {
            return Err(invalid("올바른 DiskWeave 인덱스가 아닙니다"));
        }
        if value["version"].as_u64() != Some(1) {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "지원하지 않는 저장 목록 버전입니다",
            ));
        }
        let m: Manifest = serde_json::from_value(value).map_err(crate::output::json_error)?;
        let mut catalog = Catalog {
            locations: Vec::new(),
            selected: m.selected,
        };
        let mut ids = HashSet::new();
        for (i, e) in m.entries.iter().enumerate() {
            progress(Phase::Location {
                current: i + 1,
                total: m.entries.len(),
            });
            crate::output::check(cancelled)?;
            if e.id == 0 || !ids.insert(e.id) {
                return Err(invalid("중복되거나 잘못된 위치 ID"));
            }
            let root = path(&e.path)?;
            let identity = path(&e.identity)?;
            let record = match &e.snapshot {
                None => None,
                Some(name) => {
                    if !snapshot_name(name) {
                        return Err(invalid("저장된 결과의 파일명이 올바르지 않습니다"));
                    }
                    let mut record = crate::snapshot::load(&self.root.join(name), cancelled)
                        .map_err(|error| {
                            if error.kind() == io::ErrorKind::Interrupted {
                                error
                            } else {
                                invalid(&format!("저장된 위치 결과를 읽을 수 없습니다: {error}"))
                            }
                        })?;
                    if let Some(cursor) = &e.cursor {
                        let state = record
                            .tracking
                            .as_mut()
                            .ok_or_else(|| invalid("기준 결과 없는 추적 위치"))?;
                        if cursor.next < state.next {
                            return Err(invalid("저장된 변경 위치가 역행했습니다"));
                        }
                        state.next = cursor.next;
                        state.pending = cursor.pending.clone();
                        state.validate(&record.scan)?;
                        record.finished = cursor.checked.clone().decode()?;
                    }
                    let actual = &record.scan.folders[0].path;
                    if !crate::locations::contains_path(&root, actual)?
                        || !crate::locations::contains_path(actual, &root)?
                    {
                        return Err(invalid("저장 목록과 분석 결과의 대상이 다릅니다"));
                    }
                    Some(Arc::new(record))
                }
            };
            for previous in &catalog.locations {
                if crate::locations::contains_path(&previous.identity, &identity)?
                    || crate::locations::contains_path(&identity, &previous.identity)?
                {
                    return Err(invalid("저장된 위치 범위가 겹칩니다"));
                }
            }
            catalog.locations.push(CachedLocation {
                id: e.id,
                path: root,
                identity,
                enabled: e.enabled,
                stale: e.stale,
                generation: e.generation,
                record,
            });
        }
        if m.selected.is_some_and(|id| !ids.contains(&id)) {
            return Err(invalid("저장된 선택 위치가 없습니다"));
        }
        Ok((m, catalog))
    }
    pub fn load(&mut self, cancelled: impl Fn() -> bool) -> io::Result<Loaded> {
        self.load_with_progress(cancelled, |_| {})
    }
    pub fn load_with_progress(
        &mut self,
        cancelled: impl Fn() -> bool,
        mut progress: impl FnMut(Phase),
    ) -> io::Result<Loaded> {
        let lease = self.reader_lease(&cancelled)?;
        self.loaded = false;
        self.enabled = match read_bounded(&self.root.join("settings.json"), 1024) {
            Ok(bytes) => {
                let value: serde_json::Value =
                    serde_json::from_slice(&bytes).map_err(crate::output::json_error)?;
                if value["version"].as_u64() != Some(1) {
                    return Err(io::Error::new(
                        io::ErrorKind::Unsupported,
                        "지원하지 않는 저장 설정 버전입니다",
                    ));
                }
                serde_json::from_value::<Preference>(value)
                    .map_err(crate::output::json_error)?
                    .enabled
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => true,
            Err(e) => return Err(e),
        };
        crate::output::check(&cancelled)?;
        if !self.enabled {
            self.current = None;
            self.cached = Catalog::default();
            self.loaded = true;
            return Ok(Loaded {
                catalog: Catalog::default(),
                enabled: false,
                writable: self._writer.is_some(),
                notice: None,
            });
        }
        let mut notice = None;
        let (manifest, catalog) = match self.read_manifest(CATALOG_NAME, &cancelled, &mut progress)
        {
            Ok((m, c)) => (Some(m), c),
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::Interrupted | io::ErrorKind::Unsupported
                ) =>
            {
                return Err(e)
            }
            Err(current) => match self.read_manifest(PREVIOUS_NAME, &cancelled, &mut progress) {
                Ok((m, c)) => {
                    notice = Some(format!(
                        "최근 저장 목록을 읽지 못해 이전 완료 목록을 복원했습니다. {current}"
                    ));
                    (Some(m), c)
                }
                Err(previous)
                    if previous.kind() == io::ErrorKind::NotFound
                        && current.kind() == io::ErrorKind::NotFound
                        && !self.root.join(CATALOG_NAME).exists()
                        && !self.root.join(PREVIOUS_NAME).exists() =>
                {
                    (None, Catalog::default())
                }
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::Interrupted | io::ErrorKind::Unsupported
                    ) =>
                {
                    return Err(e)
                }
                Err(previous) => {
                    return Err(invalid(&format!(
                        "저장 목록을 복원할 수 없습니다. 최근: {current}; 이전: {previous}"
                    )))
                }
            },
        };
        self.current = manifest;
        self.cached = catalog.clone();
        self.loaded = true;
        drop(lease);
        self.cleanup_notice = self
            .cleanup()
            .err()
            .map(|e| format!("목록은 복원했지만 이전 저장 파일 정리를 마치지 못했습니다. {e}"));
        if notice.is_none() {
            notice = self.cleanup_notice.clone();
        }
        Ok(Loaded {
            catalog,
            enabled: true,
            writable: self._writer.is_some(),
            notice,
        })
    }
    fn reader_lease(&self, cancelled: &impl Fn() -> bool) -> io::Result<fs::File> {
        for _ in 0..100 {
            crate::output::check(cancelled)?;
            match fs::File::options()
                .read(true)
                .share_mode(1)
                .open(self.root.join("readers.lock"))
            {
                Ok(file) => return Ok(file),
                Err(e) if e.raw_os_error() == Some(32) || e.kind() == io::ErrorKind::NotFound => {
                    std::thread::sleep(std::time::Duration::from_millis(10))
                }
                Err(e) => return Err(e),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            "저장소 정리 중입니다. 잠시 후 다시 불러오세요",
        ))
    }
    fn cleanup(&self) -> io::Result<()> {
        if self._writer.is_none() {
            return Ok(());
        }
        let _exclusive = match fs::File::options()
            .read(true)
            .write(true)
            .share_mode(0)
            .open(self.root.join("readers.lock"))
        {
            Ok(file) => file,
            Err(e) if e.raw_os_error() == Some(32) => return Ok(()),
            Err(e) => return Err(e),
        };
        let mut keep = HashSet::new();
        if let Some(m) = &self.current {
            keep.extend(m.entries.iter().filter_map(|e| e.snapshot.clone()));
        }
        match read_bounded(&self.root.join(PREVIOUS_NAME), 1024 * 1024) {
            Ok(bytes) => {
                let m: Manifest =
                    serde_json::from_slice(&bytes).map_err(crate::output::json_error)?;
                if m.version != 1 || m.format != "DiskWeaveIndex" {
                    return Err(invalid("이전 저장 목록 버전 확인 필요"));
                }
                for e in m.entries {
                    if let Some(name) = e.snapshot {
                        if !snapshot_name(&name) {
                            return Err(invalid("이전 저장 파일명 확인 필요"));
                        }
                        keep.insert(name);
                    }
                }
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        for e in fs::read_dir(&self.root)? {
            let e = e?;
            let name = e.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            if snapshot_name(name) && !keep.contains(name) && e.file_type()?.is_file() {
                fs::remove_file(e.path())?;
            }
        }
        Ok(())
    }
    pub fn notice(&self) -> Option<&str> {
        self.cleanup_notice.as_deref()
    }
    fn require_writer(&self) -> io::Result<()> {
        if self._writer.is_none() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "다른 DiskWeave 창에서 저장 중입니다. 이 창은 조회만 가능합니다",
            ));
        }
        Ok(())
    }
    pub fn configure(&mut self, enabled: bool, cancelled: impl Fn() -> bool) -> io::Result<()> {
        self.require_writer()?;
        crate::output::write(&self.root.join("settings.json"), true, &cancelled, |w| {
            serde_json::to_writer(
                w,
                &Preference {
                    version: 1,
                    enabled,
                },
            )
            .map_err(crate::output::json_error)
        })?;
        self.enabled = enabled;
        self.loaded = false;
        Ok(())
    }
    pub fn clear(
        &mut self,
        cancelled: impl Fn() -> bool,
        mut progress: impl FnMut(Phase),
    ) -> io::Result<()> {
        self.require_writer()?;
        self.configure(self.enabled, &cancelled)?;
        let sequence = self.current.as_ref().map_or(Ok(1), |m| {
            m.sequence
                .checked_add(1)
                .ok_or_else(|| invalid("저장 세대 범위 초과"))
        })?;
        let m = Manifest {
            format: "DiskWeaveIndex".into(),
            version: 1,
            sequence,
            selected: None,
            entries: Vec::new(),
        };
        self.publish(
            m.clone(),
            &Catalog::default(),
            Some(m),
            &cancelled,
            &mut progress,
        )
    }
    /// 위치별 완료 파일을 먼저 완성하고 목록 파일 교체로 새 세대를 확정.
    pub fn save(
        &mut self,
        catalog: &Catalog,
        cancelled: impl Fn() -> bool,
        mut progress: impl FnMut(Phase),
    ) -> io::Result<()> {
        self.require_writer()?;
        if !self.enabled {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "분석 목록 자동 저장이 꺼져 있습니다",
            ));
        }
        if !self.loaded {
            return Err(io::Error::other("저장 목록을 먼저 불러와야 합니다"));
        }
        crate::output::check(&cancelled)?;
        let sequence = self.current.as_ref().map_or(Ok(1), |m| {
            m.sequence
                .checked_add(1)
                .ok_or_else(|| invalid("저장 세대 범위 초과"))
        })?;
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(io::Error::other)?
            .as_nanos();
        let mut entries = Vec::new();
        for (i, l) in catalog.locations.iter().enumerate() {
            crate::output::check(&cancelled)?;
            progress(Phase::Location {
                current: i + 1,
                total: catalog.locations.len(),
            });
            let snapshot = if let Some(record) = &l.record {
                let old_entry = self
                    .cached
                    .locations
                    .iter()
                    .find(|e| e.identity == l.identity);
                let old = old_entry.and_then(|e| e.record.as_ref());
                let existing = self
                    .current
                    .as_ref()
                    .and_then(|m| {
                        m.entries
                            .iter()
                            .find(|e| Some(e.id) == old_entry.map(|old| old.id))
                    })
                    .and_then(|e| e.snapshot.clone());
                if old.is_some_and(|old| {
                    Arc::ptr_eq(&old.scan, &record.scan)
                        && old.started == record.started
                        && (old.finished == record.finished
                            || old
                                .tracking
                                .as_ref()
                                .zip(record.tracking.as_ref())
                                .is_some_and(|(a, b)| {
                                    a.version == b.version
                                        && a.volume == b.volume
                                        && a.root == b.root
                                        && a.journal == b.journal
                                        && a.folders == b.folders
                                        && a.files == b.files
                                        && a.excluded == b.excluded
                                }))
                }) && existing.is_some()
                {
                    existing
                } else {
                    let name = format!("scan-{}-{nonce}-{}.dwsnap", std::process::id(), l.id);
                    crate::snapshot::save(record, &self.root.join(&name), false, &cancelled)?;
                    Some(name)
                }
            } else {
                None
            };
            entries.push(Entry {
                cursor: l.record.as_ref().and_then(|r| {
                    r.tracking.as_ref().map(|s| Cursor {
                        next: s.next,
                        pending: s.pending.clone(),
                        checked: r.finished.into(),
                    })
                }),
                id: l.id,
                path: wide(&l.path),
                identity: wide(&l.identity),
                enabled: l.enabled,
                stale: l.stale,
                generation: l.generation,
                snapshot,
            });
        }
        let m = Manifest {
            format: "DiskWeaveIndex".into(),
            version: 1,
            sequence,
            selected: catalog.selected,
            entries,
        };
        self.publish(m, catalog, self.current.clone(), &cancelled, &mut progress)
    }
    fn publish(
        &mut self,
        m: Manifest,
        catalog: &Catalog,
        previous: Option<Manifest>,
        cancelled: &impl Fn() -> bool,
        progress: &mut impl FnMut(Phase),
    ) -> io::Result<()> {
        let bytes = serde_json::to_vec(&m).map_err(crate::output::json_error)?;
        if bytes.len() > 1024 * 1024 {
            return Err(invalid("저장할 위치 목록이 너무 큽니다"));
        }
        if let Some(previous) = previous.as_ref() {
            crate::output::write(&self.root.join(PREVIOUS_NAME), true, cancelled, |w| {
                serde_json::to_writer(w, previous).map_err(crate::output::json_error)
            })?;
        }
        progress(Phase::Publishing);
        crate::output::write(&self.root.join(CATALOG_NAME), true, cancelled, |w| {
            w.write_all(&bytes)
        })?;
        self.current = Some(m);
        self.cached = catalog.clone();
        self.loaded = true;
        self.cleanup_notice = self
            .cleanup()
            .err()
            .map(|e| format!("목록은 저장했지만 이전 저장 파일 정리를 마치지 못했습니다. {e}"));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        sync::Arc,
        time::{Duration, SystemTime, UNIX_EPOCH},
    };
    fn fixture(label: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "diskweave-index-{label}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&p).unwrap();
        p
    }
    fn with_checkpoint(mut location: CachedLocation, next: i64) -> CachedLocation {
        let record = Arc::get_mut(location.record.as_mut().unwrap()).unwrap();
        record.tracking = Some(crate::usn::Checkpoint {
            version: 1,
            volume: "test".into(),
            root: 1,
            journal: 7,
            next,
            folders: (1..=record.scan.folders.len() as u64).collect(),
            files: vec![99; record.scan.files.len()],
            excluded: vec![],
            pending: vec![],
        });
        location
    }
    #[test]
    fn cursor_and_result_recover_as_one_generation_after_a_cancelled_publish() {
        use std::cell::Cell;
        let p = fixture("cursor");
        let root = p.join("source");
        fs::create_dir(&root).unwrap();
        fs::write(root.join("known"), b"12345").unwrap();
        let cache = p.join("cache");
        let mut store = Repository::open(&cache).unwrap();
        store.load(|| false).unwrap();
        let first = with_checkpoint(location(&root, 1), 100);
        let mut catalog = Catalog {
            locations: vec![first],
            selected: Some(1),
        };
        store.save(&catalog, || false, |_| {}).unwrap();
        let old = catalog.locations[0].record.as_ref().unwrap();
        let mut next = SavedScan::new(
            Arc::clone(&old.scan),
            old.started,
            old.finished + Duration::from_secs(1),
            old.elapsed,
        );
        next.tracking = old.tracking.clone();
        next.tracking.as_mut().unwrap().next = 108;
        catalog.locations[0].record = Some(Arc::new(next));
        let stop = Cell::new(false);
        assert!(store
            .save(
                &catalog,
                || stop.get(),
                |phase| if matches!(phase, Phase::Publishing) {
                    stop.set(true)
                }
            )
            .is_err());
        let mut reader = Repository::open(&cache).unwrap();
        assert_eq!(
            reader.load(|| false).unwrap().catalog.locations[0]
                .record
                .as_ref()
                .unwrap()
                .tracking
                .as_ref()
                .unwrap()
                .next,
            100
        );
        drop(reader);
        store.save(&catalog, || false, |_| {}).unwrap();
        drop(store);
        let mut reopened = Repository::open(&cache).unwrap();
        let loaded = reopened.load(|| false).unwrap();
        let record = loaded.catalog.locations[0].record.as_ref().unwrap();
        assert_eq!(record.tracking.as_ref().unwrap().next, 108);
        assert_eq!(record.scan.folders[0].logical_bytes, 5);
        drop(reopened);
        fs::remove_dir_all(p).unwrap();
    }
    fn location(root: &Path, id: u64) -> CachedLocation {
        CachedLocation {
            id,
            path: root.to_owned(),
            identity: fs::canonicalize(root).unwrap(),
            enabled: true,
            stale: false,
            generation: 1,
            record: Some(Arc::new(crate::snapshot::SavedScan::new(
                Arc::new(crate::scanner::scan_with_options(root, true, || false, |_| {}).unwrap()),
                SystemTime::now(),
                SystemTime::now(),
                Duration::from_millis(3),
            ))),
        }
    }
    #[test]
    fn unsupported_versions_and_external_snapshot_references_are_not_loaded_or_overwritten() {
        let p = fixture("untrusted");
        let source = p.join("source");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("known"), b"12345").unwrap();
        let cache = p.join("cache");
        let mut store = Repository::open(&cache).unwrap();
        store.load(|| false).unwrap();
        let catalog = Catalog {
            locations: vec![location(&source, 1)],
            selected: Some(1),
        };
        store.save(&catalog, || false, |_| {}).unwrap();
        let original = fs::read(cache.join(CATALOG_NAME)).unwrap();
        let mut header: serde_json::Value = serde_json::from_slice(&original).unwrap();
        header["version"] = 999.into();
        let future = serde_json::to_vec(&header).unwrap();
        fs::write(cache.join(CATALOG_NAME), &future).unwrap();
        assert_eq!(
            store.load(|| false).err().unwrap().kind(),
            io::ErrorKind::Unsupported
        );
        assert!(store.save(&catalog, || false, |_| {}).is_err());
        assert_eq!(fs::read(cache.join(CATALOG_NAME)).unwrap(), future);
        let outside = p.join("outside.dwsnap");
        crate::snapshot::save(
            catalog.locations[0].record.as_ref().unwrap(),
            &outside,
            false,
            || false,
        )
        .unwrap();
        let mut header: serde_json::Value = serde_json::from_slice(&original).unwrap();
        header["entries"][0]["snapshot"] = "../outside.dwsnap".into();
        fs::write(
            cache.join(CATALOG_NAME),
            serde_json::to_vec(&header).unwrap(),
        )
        .unwrap();
        assert_eq!(
            store.load(|| false).err().unwrap().kind(),
            io::ErrorKind::InvalidData
        );
        store.clear(|| false, |_| {}).unwrap();
        assert!(outside.is_file());
        drop(store);
        fs::remove_dir_all(p).unwrap();
    }
    #[test]
    fn forced_exit_before_catalog_commit_preserves_the_last_complete_generation() {
        use std::io::BufRead;
        use std::os::windows::process::CommandExt;
        let p = fixture("crash");
        let source = p.join("source");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("known"), b"12345").unwrap();
        let cache = p.join("cache");
        let mut store = Repository::open(&cache).unwrap();
        store.load(|| false).unwrap();
        store
            .save(
                &Catalog {
                    locations: vec![with_checkpoint(location(&source, 1), 100)],
                    selected: Some(1),
                },
                || false,
                |_| {},
            )
            .unwrap();
        drop(store);
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--ignored",
                "--exact",
                "index_store::tests::crash_writer_fixture_process",
                "--nocapture",
            ])
            .env("DISKWEAVE_CRASH_FIXTURE", &p)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .creation_flags(0x0800_0000)
            .spawn()
            .unwrap();
        let mut output = io::BufReader::new(child.stdout.take().unwrap());
        let mut line = String::new();
        loop {
            line.clear();
            assert_ne!(
                output.read_line(&mut line).unwrap(),
                0,
                "시험 프로세스가 준비 전에 종료됨"
            );
            if line.contains("DW_INDEX_READY") {
                break;
            }
        }
        child.kill().unwrap();
        child.wait().unwrap();
        let mut reopened = Repository::open(&cache).unwrap();
        let loaded = reopened.load(|| false).unwrap();
        assert!(loaded.writable);
        assert_eq!(
            loaded.catalog.locations[0]
                .record
                .as_ref()
                .unwrap()
                .tracking
                .as_ref()
                .unwrap()
                .next,
            100
        );
        assert_eq!(
            loaded.catalog.locations[0]
                .record
                .as_ref()
                .unwrap()
                .scan
                .folders[0]
                .logical_bytes,
            5
        );
        drop(reopened);
        fs::remove_dir_all(p).unwrap();
    }
    #[test]
    #[ignore = "강제 종료 시험의 전용 하위 프로세스"]
    fn crash_writer_fixture_process() {
        use std::io::Write;
        let Some(root) = std::env::var_os("DISKWEAVE_CRASH_FIXTURE") else {
            return;
        };
        let p = fs::canonicalize(PathBuf::from(root)).unwrap();
        assert!(p.starts_with(fs::canonicalize(std::env::temp_dir()).unwrap()));
        assert!(p
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("diskweave-index-crash-"));
        let source = p.join("source");
        fs::write(source.join("known"), b"123456789").unwrap();
        let mut store = Repository::open(&p.join("cache")).unwrap();
        store.load(|| false).unwrap();
        store
            .save(
                &Catalog {
                    locations: vec![with_checkpoint(location(&source, 1), 108)],
                    selected: Some(1),
                },
                || false,
                |phase| {
                    if matches!(phase, Phase::Publishing) {
                        println!("DW_INDEX_READY");
                        io::stdout().flush().unwrap();
                        let mut line = String::new();
                        io::stdin().read_line(&mut line).unwrap();
                    }
                },
            )
            .unwrap();
    }
    #[test]
    fn a_reader_finishes_one_complete_generation_while_the_writer_publishes_new_ones() {
        let p = fixture("reader");
        let source = p.join("source");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("known"), b"12345").unwrap();
        let cache = p.join("cache");
        let mut writer = Repository::open(&cache).unwrap();
        writer.load(|| false).unwrap();
        writer
            .save(
                &Catalog {
                    locations: vec![location(&source, 1)],
                    selected: Some(1),
                },
                || false,
                |_| {},
            )
            .unwrap();
        let (ready, wait) = std::sync::mpsc::channel();
        let (resume, signal) = std::sync::mpsc::channel();
        let reader_path = cache.clone();
        let reader = std::thread::spawn(move || {
            let mut reader = Repository::open(&reader_path).unwrap();
            reader
                .load_with_progress(
                    || false,
                    |phase| {
                        if matches!(phase, Phase::Location { .. }) {
                            ready.send(()).unwrap();
                            signal.recv().unwrap();
                        }
                    },
                )
                .unwrap()
        });
        wait.recv_timeout(Duration::from_secs(5)).unwrap();
        for size in [9, 13, 17] {
            fs::write(source.join("known"), vec![0; size]).unwrap();
            writer
                .save(
                    &Catalog {
                        locations: vec![location(&source, 1)],
                        selected: Some(1),
                    },
                    || false,
                    |_| {},
                )
                .unwrap();
        }
        resume.send(()).unwrap();
        let loaded = reader.join().unwrap();
        assert_eq!(
            loaded.catalog.locations[0]
                .record
                .as_ref()
                .unwrap()
                .scan
                .folders[0]
                .logical_bytes,
            5
        );
        drop(writer);
        fs::remove_dir_all(p).unwrap();
    }
    #[test]
    fn disabled_restoration_and_clear_preserve_original_files_and_do_not_resurrect_old_lists() {
        let p = fixture("settings");
        let source = p.join("source");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("known"), b"12345").unwrap();
        let cache = p.join("cache");
        let mut store = Repository::open(&cache).unwrap();
        store.load(|| false).unwrap();
        store
            .save(
                &Catalog {
                    locations: vec![location(&source, 1)],
                    selected: Some(1),
                },
                || false,
                |_| {},
            )
            .unwrap();
        store.configure(false, || false).unwrap();
        drop(store);
        let mut store = Repository::open(&cache).unwrap();
        let loaded = store.load(|| false).unwrap();
        assert!(!loaded.enabled);
        assert!(loaded.catalog.locations.is_empty());
        assert!(store.save(&Catalog::default(), || false, |_| {}).is_err());
        store.clear(|| false, |_| {}).unwrap();
        store.configure(true, || false).unwrap();
        let loaded = store.load(|| false).unwrap();
        assert!(loaded.catalog.locations.is_empty());
        fs::write(cache.join(CATALOG_NAME), b"corrupt").unwrap();
        assert!(store.load(|| false).unwrap().catalog.locations.is_empty());
        assert_eq!(fs::read(source.join("known")).unwrap(), b"12345");
        drop(store);
        fs::remove_dir_all(p).unwrap();
    }
    #[test]
    fn cancellation_and_a_blocked_catalog_replace_keep_the_last_completed_result() {
        let p = fixture("failed-save");
        let source = p.join("source");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("known"), b"12345").unwrap();
        let cache = p.join("cache");
        let mut store = Repository::open(&cache).unwrap();
        store.load(|| false).unwrap();
        store
            .save(
                &Catalog {
                    locations: vec![location(&source, 1)],
                    selected: Some(1),
                },
                || false,
                |_| {},
            )
            .unwrap();
        fs::write(source.join("known"), b"123456789").unwrap();
        let next = Catalog {
            locations: vec![location(&source, 1)],
            selected: Some(1),
        };
        let cancel = std::cell::Cell::new(false);
        let e = store
            .save(
                &next,
                || cancel.get(),
                |phase| {
                    if matches!(phase, Phase::Publishing) {
                        cancel.set(true)
                    }
                },
            )
            .unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::Interrupted);
        let guard = fs::File::options()
            .read(true)
            .share_mode(1)
            .open(cache.join(CATALOG_NAME))
            .unwrap();
        assert!(store.save(&next, || false, |_| {}).is_err());
        drop(guard);
        let loaded = store.load(|| false).unwrap();
        assert_eq!(
            loaded.catalog.locations[0]
                .record
                .as_ref()
                .unwrap()
                .scan
                .folders[0]
                .logical_bytes,
            5
        );
        drop(store);
        fs::remove_dir_all(p).unwrap();
    }
    #[test]
    fn damaged_latest_catalog_recovers_the_previous_complete_generation() {
        let p = fixture("recovery");
        let source = p.join("source");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("known"), b"12345").unwrap();
        let mut store = Repository::open(&p.join("cache")).unwrap();
        store.load(|| false).unwrap();
        store
            .save(
                &Catalog {
                    locations: vec![location(&source, 1)],
                    selected: Some(1),
                },
                || false,
                |_| {},
            )
            .unwrap();
        fs::write(source.join("known"), b"123456789").unwrap();
        store
            .save(
                &Catalog {
                    locations: vec![location(&source, 1)],
                    selected: Some(1),
                },
                || false,
                |_| {},
            )
            .unwrap();
        fs::write(p.join("cache").join(CATALOG_NAME), b"corrupt").unwrap();
        let loaded = store.load(|| false).unwrap();
        assert!(loaded.notice.is_some());
        assert_eq!(
            loaded.catalog.locations[0]
                .record
                .as_ref()
                .unwrap()
                .scan
                .folders[0]
                .logical_bytes,
            5
        );
        drop(store);
        fs::remove_dir_all(p).unwrap();
    }
    #[test]
    fn only_the_first_instance_writes_and_process_lifetime_releases_ownership() {
        let p = fixture("ownership");
        let source = p.join("source");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("known"), b"12345").unwrap();
        let catalog = Catalog {
            locations: vec![location(&source, 1)],
            selected: Some(1),
        };
        let mut first = Repository::open(&p.join("cache")).unwrap();
        first.load(|| false).unwrap();
        first.save(&catalog, || false, |_| {}).unwrap();
        let mut second = Repository::open(&p.join("cache")).unwrap();
        assert!(!second.load(|| false).unwrap().writable);
        assert_eq!(
            second
                .save(&Catalog::default(), || false, |_| {})
                .unwrap_err()
                .kind(),
            io::ErrorKind::PermissionDenied
        );
        drop(first);
        drop(second);
        let mut next = Repository::open(&p.join("cache")).unwrap();
        let loaded = next.load(|| false).unwrap();
        assert!(loaded.writable);
        assert_eq!(
            loaded.catalog.locations[0]
                .record
                .as_ref()
                .unwrap()
                .scan
                .folders[0]
                .logical_bytes,
            5
        );
        drop(next);
        fs::remove_dir_all(p).unwrap();
    }
    #[test]
    fn completed_locations_reopen_offline_with_exact_metadata_and_selection() {
        let p = fixture("reopen");
        let first = p.join("source/a");
        let second = p.join("source/b");
        fs::create_dir_all(&first).unwrap();
        fs::create_dir_all(&second).unwrap();
        fs::write(first.join("a.npy"), b"1234567").unwrap();
        fs::write(second.join("b.bin"), b"12345678901").unwrap();
        let catalog = Catalog {
            locations: vec![location(&first, 1), location(&second, 2)],
            selected: Some(2),
        };
        let mut store = Repository::open(&p.join("cache")).unwrap();
        assert!(store.load(|| false).unwrap().catalog.locations.is_empty());
        store.save(&catalog, || false, |_| {}).unwrap();
        drop(store);
        fs::remove_dir_all(p.join("source")).unwrap();
        let mut reopened = Repository::open(&p.join("cache")).unwrap();
        let loaded = reopened.load(|| false).unwrap();
        assert!(loaded.enabled && loaded.writable);
        assert_eq!(loaded.catalog.selected, Some(2));
        assert_eq!(loaded.catalog.locations.len(), 2);
        for (i, bytes) in [(0, 7), (1, 11)] {
            let r = loaded.catalog.locations[i].record.as_ref().unwrap();
            assert_eq!(r.scan.folders[0].logical_bytes, bytes);
            assert_eq!(r.scan.files.len(), 1);
            assert_eq!(
                r.scan.files[0].extra,
                catalog.locations[i].record.as_ref().unwrap().scan.files[0].extra
            );
        }
        drop(reopened);
        fs::remove_dir_all(p).unwrap();
    }
}
