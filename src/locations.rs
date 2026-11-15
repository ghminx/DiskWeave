use crate::scan_job::{CompletedScan, ScanController, ScanStatus};
use crate::scanner::{ScanProgress, ScanResult};
use std::collections::VecDeque;
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

pub struct Location {
    pub id: u64,
    pub path: PathBuf,
    identity: PathBuf,
    pub scan: ScanController,
    pub stale: bool,
    pub enabled: bool,
    pub generation: u64,
}
#[derive(Clone)]
pub struct Snapshot {
    pub id: u64,
    pub generation: u64,
    pub scan: Arc<ScanResult>,
}
impl Snapshot {
    pub fn analyze(&self, item: crate::explorer::Item) -> crate::explorer::Explorer {
        let folder = match item {
            crate::explorer::Item::Folder(id) => id,
            crate::explorer::Item::File(id) => self.scan.files[id].parent,
        };
        let mut explorer = crate::explorer::Explorer::new(&self.scan);
        explorer.open_folder(&self.scan, folder);
        explorer.select(item);
        explorer
    }
}
#[derive(Default)]
pub struct Locations {
    settings: crate::metadata::ScanSettings,
    pub entries: Vec<Location>,
    next_id: u64,
    selected: Option<u64>,
    active: Option<u64>,
    queue: VecDeque<u64>,
    changed: bool,
    pub epoch: u64,
    idle: ScanStatus,
}

#[link(name = "kernel32")]
extern "system" {
    fn CompareStringOrdinal(
        a: *const u16,
        a_len: i32,
        b: *const u16,
        b_len: i32,
        ignore_case: i32,
    ) -> i32;
}

pub(crate) fn contains_path(parent: &Path, child: &Path) -> io::Result<bool> {
    let mut children = child.components();
    for part in parent.components() {
        let Some(other) = children.next() else {
            return Ok(false);
        };
        let a: Vec<u16> = part.as_os_str().encode_wide().collect();
        let b: Vec<u16> = other.as_os_str().encode_wide().collect();
        // NUL 종료에 의존하지 않는 원본 UTF-16 구성요소 비교. 버퍼 수명을 호출 동안 유지.
        let compared = unsafe {
            CompareStringOrdinal(a.as_ptr(), a.len() as i32, b.as_ptr(), b.len() as i32, 1)
        };
        if compared == 0 {
            return Err(io::Error::last_os_error());
        }
        if compared != 2 {
            return Ok(false);
        }
    }
    Ok(true)
}

impl Locations {
    pub fn from_saved(record: &crate::snapshot::SavedScan) -> Self {
        let path = record.scan.folders[0].path.clone();
        Self {
            entries: vec![Location {
                id: 1,
                identity: path.clone(),
                path,
                scan: ScanController::from_saved(record),
                stale: false,
                enabled: true,
                generation: 1,
            }],
            selected: Some(1),
            next_id: 1,
            epoch: 1,
            ..Default::default()
        }
    }
    pub fn details_requested(&self) -> bool {
        self.settings.detailed
    }
    pub fn configure(&mut self, detailed: bool) -> io::Result<()> {
        if self.is_running() {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "수집 중에는 방식을 변경할 수 없습니다",
            ));
        }
        self.settings.detailed = detailed;
        Ok(())
    }
    pub fn add(&mut self, path: &Path) -> io::Result<u64> {
        let metadata = std::fs::symlink_metadata(path)?;
        if !metadata.is_dir() || metadata.file_attributes() & 0x400 != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "재분석 지점이 아닌 폴더를 등록하세요",
            ));
        }
        let identity = std::fs::canonicalize(path)?;
        for entry in &self.entries {
            if contains_path(&entry.identity, &identity)?
                || contains_path(&identity, &entry.identity)?
            {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "이미 등록됐거나 상위·하위 위치가 겹칩니다",
                ));
            }
        }
        self.next_id += 1;
        self.entries.push(Location {
            id: self.next_id,
            path: std::path::absolute(path)?,
            identity,
            scan: ScanController::default(),
            stale: false,
            enabled: true,
            generation: 0,
        });
        self.epoch += 1;
        if self.selected.is_none() {
            self.selected = Some(self.next_id);
            self.changed = true;
        }
        Ok(self.next_id)
    }
    pub fn remove(&mut self, id: u64) -> io::Result<()> {
        if self.is_running() {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "스캔 종료 후 위치를 제거하세요",
            ));
        }
        let index = self
            .entries
            .iter()
            .position(|entry| entry.id == id)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "등록 위치가 없습니다"))?;
        self.entries.remove(index);
        self.epoch += 1;
        if self.selected == Some(id) {
            self.selected = self.entries.first().map(|e| e.id);
            self.changed = true;
        }
        Ok(())
    }
    pub fn get(&self, id: u64) -> Option<&Location> {
        self.entries.iter().find(|e| e.id == id)
    }
    pub fn selected_id(&self) -> Option<u64> {
        self.selected
    }
    pub fn active_path(&self) -> Option<&Path> {
        self.active
            .and_then(|id| self.get(id))
            .map(|entry| entry.path.as_path())
    }
    pub fn choose(&mut self, id: u64) {
        if self.selected != Some(id) && self.get(id).is_some() {
            self.selected = Some(id);
            self.changed = true;
        }
    }
    pub fn start(&mut self, path: PathBuf) -> io::Result<()> {
        if self.is_running() {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "이미 스캔 중입니다",
            ));
        }
        let absolute = std::path::absolute(&path)?;
        let mut existing = None;
        for entry in &self.entries {
            if contains_path(&entry.path, &absolute)? && contains_path(&absolute, &entry.path)? {
                existing = Some(entry.id);
                break;
            }
        }
        let id = match existing {
            Some(id) => id,
            None => self.add(&absolute)?,
        };
        self.choose(id);
        self.start_selected(&[id])
    }
    pub fn start_selected(&mut self, ids: &[u64]) -> io::Result<()> {
        if self.is_running() {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "이미 스캔 중입니다",
            ));
        }
        if ids.is_empty() || ids.iter().any(|id| self.get(*id).is_none()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "등록 위치를 선택하세요",
            ));
        }
        for id in ids {
            if !self.queue.contains(id) {
                self.queue.push_back(*id);
            }
        }
        self.launch_next();
        Ok(())
    }
    fn launch_next(&mut self) {
        if self.active.is_some() {
            return;
        }
        if let Some(id) = self.queue.pop_front() {
            let entry = self
                .entries
                .iter_mut()
                .find(|e| e.id == id)
                .expect("등록된 대기 위치");
            entry
                .scan
                .configure(self.settings.detailed)
                .expect("대기 중인 위치의 수집 설정");
            entry
                .scan
                .start(entry.path.clone())
                .expect("동시에 하나의 스캔만 실행");
            self.active = Some(id);
        }
    }
    pub fn poll(&mut self) -> bool {
        let mut changed = std::mem::take(&mut self.changed);
        if let Some(id) = self.active {
            let entry = self
                .entries
                .iter_mut()
                .find(|e| e.id == id)
                .expect("활성 등록 위치");
            if entry.scan.poll() {
                entry.stale = false;
                entry.generation += 1;
                self.epoch += 1;
                changed |= self.selected == Some(id);
            }
            if !entry.scan.is_running() {
                if matches!(entry.scan.status(), ScanStatus::Failed(_)) {
                    entry.stale = true;
                    self.epoch += 1;
                }
                self.active = None;
            }
        }
        self.launch_next();
        changed
    }
    pub fn cancel(&mut self) {
        self.queue.clear();
        if let Some(id) = self.active {
            self.entries
                .iter_mut()
                .find(|e| e.id == id)
                .expect("활성 위치")
                .scan
                .cancel();
        }
    }
    pub fn is_running(&self) -> bool {
        self.active.is_some() || !self.queue.is_empty()
    }
    pub fn status(&self) -> &ScanStatus {
        self.active
            .or(self.selected)
            .and_then(|id| self.get(id))
            .map(|e| e.scan.status())
            .unwrap_or(&self.idle)
    }
    pub fn progress(&self) -> Option<ScanProgress> {
        self.active
            .and_then(|id| self.get(id))
            .and_then(|e| e.scan.progress())
    }
    pub fn elapsed(&self) -> Option<Duration> {
        self.active
            .and_then(|id| self.get(id))
            .and_then(|e| e.scan.elapsed())
    }
    pub fn completed(&self) -> Option<&CompletedScan> {
        self.selected
            .and_then(|id| self.get(id))
            .and_then(|e| e.scan.completed())
    }
    pub fn snapshots(&self) -> Vec<Snapshot> {
        self.entries
            .iter()
            .filter(|e| !e.stale)
            .filter_map(|e| {
                e.scan.completed().map(|c| Snapshot {
                    id: e.id,
                    generation: e.generation,
                    scan: Arc::clone(&c.result),
                })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};
    #[test]
    fn queued_locations_keep_one_metadata_mode_until_the_batch_finishes() {
        let root =
            std::env::temp_dir().join(format!("diskweave-location-options-{}", std::process::id()));
        for name in ["a", "b"] {
            std::fs::create_dir_all(root.join(name)).unwrap();
            std::fs::write(root.join(name).join("data"), [0; 3]).unwrap();
        }
        let mut locations = Locations::default();
        assert!(locations.details_requested());
        let ids = [
            locations.add(&root.join("a")).unwrap(),
            locations.add(&root.join("b")).unwrap(),
        ];
        locations.configure(false).unwrap();
        locations.start_selected(&ids).unwrap();
        assert!(locations.configure(true).is_err());
        let deadline = Instant::now() + Duration::from_secs(5);
        while locations.is_running() && Instant::now() < deadline {
            locations.poll();
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(!locations.is_running());
        for id in ids {
            assert!(
                !locations
                    .get(id)
                    .unwrap()
                    .scan
                    .completed()
                    .unwrap()
                    .result
                    .detailed
            );
        }
        locations.configure(true).unwrap();
        locations.start_selected(&ids).unwrap();
        while locations.is_running() && Instant::now() < deadline {
            locations.poll();
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(!locations.is_running());
        for id in ids {
            let result = &locations.get(id).unwrap().scan.completed().unwrap().result;
            assert!(result.detailed);
            assert_eq!(result.folders[0].allocation.known, 1);
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn registration_rejects_overlap_and_removal_preserves_original_files() {
        let root = std::env::temp_dir().join(format!("diskweave-locations-{}", std::process::id()));
        std::fs::create_dir_all(root.join("Data").join("child")).unwrap();
        std::fs::create_dir(root.join("Database")).unwrap();
        let mut locations = Locations::default();
        let first = locations.add(&root.join("Data")).unwrap();
        assert!(locations.add(&root.join("Data")).is_err());
        assert!(locations.add(&root.join("data")).is_err());
        assert!(locations.add(&root.join("Data").join("child")).is_err());
        assert!(locations.add(&root).is_err());
        assert!(locations.add(&root.join("Database")).is_ok());
        use std::os::windows::ffi::OsStringExt;
        for unit in [0xd800, 0xd801] {
            let path = root.join(std::ffi::OsString::from_wide(&[unit]));
            std::fs::create_dir(&path).unwrap();
            assert!(locations.add(&path).is_ok());
        }
        locations.remove(first).unwrap();
        assert!(root.join("Data").join("child").exists());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn batch_continues_after_failure_and_keeps_previous_result_as_stale() {
        let root = std::env::temp_dir().join(format!("diskweave-batch-{}", std::process::id()));
        std::fs::create_dir_all(root.join("a")).unwrap();
        std::fs::create_dir(root.join("b")).unwrap();
        std::fs::write(root.join("a").join("one.bin"), [0; 3]).unwrap();
        std::fs::write(root.join("b").join("two.bin"), [0; 7]).unwrap();
        let mut locations = Locations::default();
        let a = locations.add(&root.join("a")).unwrap();
        let b = locations.add(&root.join("b")).unwrap();
        locations.start_selected(&[a, b]).unwrap();
        assert_eq!(locations.active_path(), Some(root.join("a").as_path()));
        let deadline = Instant::now() + Duration::from_secs(5);
        while locations.is_running() && Instant::now() < deadline {
            locations.poll();
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(locations.snapshots().len(), 2);
        std::fs::remove_dir_all(root.join("a")).unwrap();
        locations.start_selected(&[a, b]).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while locations.is_running() && Instant::now() < deadline {
            locations.poll();
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(!locations.is_running());
        assert!(locations.get(a).unwrap().stale);
        assert!(locations.get(a).unwrap().scan.completed().is_some());
        assert_eq!(locations.snapshots().len(), 1);
        assert_eq!(locations.snapshots()[0].id, b);
        locations.start_selected(&[a, b]).unwrap();
        locations.cancel();
        let deadline = Instant::now() + Duration::from_secs(5);
        while locations.is_running() && Instant::now() < deadline {
            locations.poll();
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(locations.snapshots().len(), 1);
        std::fs::remove_dir_all(root).unwrap();
    }
}
