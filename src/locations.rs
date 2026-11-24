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

impl Location {
    pub fn relative_folder<'a>(&self, canonical: &'a Path) -> io::Result<Option<&'a Path>> {
        relative_folder(&self.identity, canonical)
    }
}
#[derive(Clone)]
pub struct Snapshot {
    pub entries: Option<Arc<crate::usn::entries::Projection>>,
    pub id: u64,
    pub generation: u64,
    pub scan: Arc<ScanResult>,
}

impl Snapshot {
    pub(crate) fn item_at_path(&self, path: &Path) -> Option<crate::explorer::Item> {
        let entries = self.entries.as_ref()?;
        let relative = path.strip_prefix(&self.scan.folders[0].path).ok()?;
        entries.item(entries.path_key(relative)?)
    }
}

impl Snapshot {
    pub fn analyze(&self, item: crate::explorer::Item) -> crate::explorer::Explorer {
        let folder = match item {
            crate::explorer::Item::Folder(id) => id,
            crate::explorer::Item::File(id) => self.scan.files[id].parent,
        };
        let mut explorer =
            crate::explorer::Explorer::with_expanded(&self.scan, folder, Default::default());
        explorer.select(item);
        explorer
    }
}
#[derive(Default)]
pub struct Locations {
    parallelism: crate::scanner::Parallelism,
    pub entries: Vec<Location>,
    next_id: u64,
    selected: Option<u64>,
    active: Option<u64>,
    queue: VecDeque<u64>,
    replacement: Option<(Location, Vec<u64>)>,
    replacement_status: Option<ScanStatus>,
    changed: bool,
    pub epoch: u64,
    idle: ScanStatus,
}

#[derive(PartialEq, Eq)]
pub struct RootReplacement {
    pub path: PathBuf,
    pub overlaps: Vec<u64>,
    identity: PathBuf,
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

pub(crate) fn relative_folder<'a>(root: &Path, folder: &'a Path) -> io::Result<Option<&'a Path>> {
    if let Ok(relative) = folder.strip_prefix(root) {
        return Ok(Some(relative));
    }
    if !contains_path(root, folder)? {
        return Ok(None);
    }
    let mut components = folder.components();
    for _ in root.components() {
        components.next();
    }
    Ok(Some(components.as_path()))
}

impl Locations {
    /// 파일 작업 이후의 기본 스캔을 요청 당시 위치 세대에만 반영.
    pub fn accept_file_refresh(
        &mut self,
        id: u64,
        generation: u64,
        record: &crate::snapshot::SavedScan,
    ) -> io::Result<bool> {
        let Some(entry) = self.entries.iter_mut().find(|e| e.id == id) else {
            return Ok(false);
        };
        if entry.generation != generation {
            return Ok(false);
        }
        if entry.scan.is_running()
            || entry.path != record.scan.folders[0].path
            || record.tracking.is_some()
        {
            return Err(io::Error::other(
                "파일 작업 후 갱신 대상이 일치하지 않습니다",
            ));
        }
        entry.scan = ScanController::from_saved(record);
        entry.stale = false;
        entry.generation += 1;
        self.epoch += 1;
        Ok(true)
    }
    /// 검증된 추적 결과와 기준점을 함께 교체하고 화면 변경 시에만 세대 갱신.
    #[cfg(test)]
    pub fn accept_tracking(
        &mut self,
        id: u64,
        record: &crate::snapshot::SavedScan,
    ) -> io::Result<bool> {
        record
            .tracking
            .as_ref()
            .ok_or_else(|| io::Error::other("변경 추적 기준점이 없습니다"))?
            .validate(&record.scan)?;
        self.apply_tracking(id, record)
    }
    pub fn accept_published(
        &mut self,
        id: u64,
        record: &crate::tracking::PublishedRecord,
    ) -> io::Result<bool> {
        self.apply_tracking(id, record)
    }
    fn apply_tracking(&mut self, id: u64, record: &crate::snapshot::SavedScan) -> io::Result<bool> {
        let entry = self
            .entries
            .iter_mut()
            .find(|e| e.id == id)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    "추적 위치가 목록에서 제거되었습니다",
                )
            })?;
        if entry.scan.is_running()
            || record
                .scan
                .folders
                .first()
                .is_none_or(|folder| folder.path != entry.path)
        {
            return Err(io::Error::other(
                "현재 스캔과 추적 대상이 일치하지 않습니다",
            ));
        }
        let changed = entry
            .scan
            .completed()
            .is_none_or(|old| !Arc::ptr_eq(&old.result, &record.scan));
        let state_changed = entry.stale;
        entry.scan = ScanController::from_saved(record);
        entry.stale = false;
        if changed {
            entry.generation += 1;
        }
        if changed || state_changed {
            self.epoch += 1;
        }
        Ok(changed)
    }
    /// 같은 결과의 기준점만 갱신하고 화면 세대 유지.
    pub fn accept_cursor(
        &mut self,
        id: u64,
        cursor: crate::tracking::Confirmation,
    ) -> io::Result<()> {
        let entry = self
            .entries
            .iter_mut()
            .find(|e| e.id == id)
            .ok_or_else(|| io::Error::other("추적 위치가 목록에서 제거되었습니다"))?;
        if entry.scan.is_running() {
            return Err(io::Error::other("현재 스캔 중인 위치입니다"));
        }
        entry.scan.accept_cursor(cursor)?;
        if entry.stale {
            entry.stale = false;
            self.epoch += 1;
        }
        Ok(())
    }
    pub fn tracking_failed(&mut self, id: u64) {
        if let Some(entry) = self.entries.iter_mut().find(|e| e.id == id) {
            if !entry.stale {
                entry.stale = true;
                self.epoch += 1;
            }
        }
    }
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
    pub fn parallelism(&self) -> crate::scanner::Parallelism {
        self.parallelism
    }
    pub fn configure_parallelism(&mut self, value: crate::scanner::Parallelism) -> io::Result<()> {
        if self.is_running() {
            return Err(io::Error::other(
                "스캔 중에는 동시 작업 수를 변경할 수 없습니다",
            ));
        }
        self.parallelism = value;
        Ok(())
    }
    fn folder_identity(path: &Path) -> io::Result<PathBuf> {
        let metadata = std::fs::symlink_metadata(path)?;
        if !metadata.is_dir() || metadata.file_attributes() & 0x400 != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "재분석 지점이 아닌 폴더를 등록하세요",
            ));
        }
        std::fs::canonicalize(path)
    }

    pub fn add(&mut self, path: &Path) -> io::Result<u64> {
        if self.replacing() {
            return Err(io::Error::other(
                "새 최상위 폴더 스캔이 끝난 뒤 위치를 등록하세요",
            ));
        }
        let identity = Self::folder_identity(path)?;
        if self.overlapping(&identity)?.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "이미 등록됐거나 상위·하위 위치가 겹칩니다",
            ));
        }
        let location = self.new_location(std::path::absolute(path)?, identity)?;
        let id = location.id;
        self.entries.push(location);
        self.epoch += 1;
        if self.selected.is_none() {
            self.selected = Some(id);
            self.changed = true;
        }
        Ok(id)
    }

    fn new_location(&mut self, path: PathBuf, identity: PathBuf) -> io::Result<Location> {
        self.next_id = self
            .next_id
            .checked_add(1)
            .ok_or_else(|| io::Error::other("등록 위치 ID 범위 초과"))?;
        Ok(Location {
            id: self.next_id,
            path,
            identity,
            scan: ScanController::default(),
            stale: false,
            enabled: true,
            generation: 0,
        })
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
    pub fn overlapping(&self, canonical: &Path) -> io::Result<Option<&Location>> {
        for entry in &self.entries {
            if contains_path(&entry.identity, canonical)?
                || contains_path(canonical, &entry.identity)?
            {
                return Ok(Some(entry));
            }
        }
        Ok(None)
    }
    pub fn selected_id(&self) -> Option<u64> {
        self.selected
    }
    pub fn active_path(&self) -> Option<&Path> {
        self.active_location().map(|entry| entry.path.as_path())
    }
    fn active_location(&self) -> Option<&Location> {
        self.replacement
            .as_ref()
            .map(|(location, _)| location)
            .or_else(|| self.active.and_then(|id| self.get(id)))
    }
    pub fn choose(&mut self, id: u64) {
        if self.selected != Some(id) && self.get(id).is_some() {
            self.selected = Some(id);
            self.changed = true;
        }
    }
    pub fn clear_selection(&mut self) {
        self.changed |= self.selected.take().is_some();
    }
    pub fn show_completed_view(&mut self, id: u64) {
        // 같은 세대에서 복원할 화면이 있으므로 선택 변경에 따른 탐색 초기화 생략.
        self.selected = Some(id);
        self.changed = false;
    }
    pub fn completed_snapshot(&self, id: u64) -> Option<Snapshot> {
        let entry = self.get(id)?;
        let completed = entry.scan.completed()?;
        Some(Snapshot {
            entries: completed.entries.clone(),
            id,
            generation: entry.generation,
            scan: Arc::clone(&completed.result),
        })
    }
    pub fn start(&mut self, path: PathBuf) -> io::Result<()> {
        let id = self.scan_location(path)?;
        self.choose(id);
        self.start_selected(&[id])
    }

    pub fn start_background(&mut self, path: PathBuf) -> io::Result<u64> {
        let id = self.scan_location(path)?;
        self.start_selected(&[id])?;
        Ok(id)
    }

    pub fn plan_root_replacement(&self, path: &Path) -> io::Result<Option<RootReplacement>> {
        let path = std::path::absolute(path)?;
        let identity = Self::folder_identity(&path)?;
        let mut overlaps = Vec::new();
        for entry in &self.entries {
            match (
                contains_path(&entry.identity, &identity)?,
                contains_path(&identity, &entry.identity)?,
            ) {
                (true, true) => return Ok(None),
                (true, false) | (false, true) => overlaps.push(entry.id),
                (false, false) => {}
            }
        }
        Ok((!overlaps.is_empty()).then_some(RootReplacement {
            path,
            overlaps,
            identity,
        }))
    }

    pub fn start_root_replacement(&mut self, plan: RootReplacement) -> io::Result<()> {
        if self.is_running() {
            return Err(io::Error::other("이미 스캔 중입니다"));
        }
        if self.plan_root_replacement(&plan.path)?.as_ref() != Some(&plan) {
            return Err(io::Error::other(
                "분석 위치가 바뀌었습니다. 스캔을 다시 시작하세요.",
            ));
        }
        let mut location = self.new_location(plan.path, plan.identity)?;
        location
            .scan
            .configure_parallelism(self.parallelism)
            .map_err(io::Error::other)?;
        location
            .scan
            .start(location.path.clone())
            .map_err(io::Error::other)?;
        self.replacement_status = None;
        self.replacement = Some((location, plan.overlaps));
        Ok(())
    }

    pub fn replacing(&self) -> bool {
        self.replacement.is_some()
    }

    fn scan_location(&mut self, path: PathBuf) -> io::Result<u64> {
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
        match existing {
            Some(id) => Ok(id),
            None => self.add(&absolute),
        }
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
        self.replacement_status = None;
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
                .configure_parallelism(self.parallelism)
                .expect("대기 위치의 동시 작업 설정");
            entry
                .scan
                .start(entry.path.clone())
                .expect("동시에 하나의 스캔만 실행");
            self.active = Some(id);
        }
    }
    pub fn poll(&mut self) -> bool {
        let mut changed = std::mem::take(&mut self.changed);
        if let Some((location, _)) = &mut self.replacement {
            let completed = location.scan.poll();
            if location.scan.is_running() {
                return changed;
            }
            let (mut location, overlaps) =
                self.replacement.take().expect("종료된 최상위 교체 스캔");
            if completed {
                self.entries.retain(|entry| !overlaps.contains(&entry.id));
                location.generation = 1;
                self.selected = Some(location.id);
                self.entries.push(location);
                self.epoch += 1;
                return true;
            }
            self.replacement_status = Some(match location.scan.status() {
                ScanStatus::Failed(error) => ScanStatus::Failed(error.clone()),
                _ => ScanStatus::Cancelled,
            });
            return changed;
        }
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
        if let Some((location, _)) = &mut self.replacement {
            location.scan.cancel();
        }
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
        self.replacing() || self.active.is_some() || !self.queue.is_empty()
    }
    pub fn status(&self) -> &ScanStatus {
        self.active_location()
            .map(|e| e.scan.status())
            .or(self.replacement_status.as_ref())
            .or_else(|| {
                self.selected
                    .and_then(|id| self.get(id))
                    .map(|e| e.scan.status())
            })
            .unwrap_or(&self.idle)
    }
    pub fn progress(&self) -> Option<ScanProgress> {
        self.active_location().and_then(|e| e.scan.progress())
    }
    pub fn elapsed(&self) -> Option<Duration> {
        self.active_location().and_then(|e| e.scan.elapsed())
    }
    pub fn completed(&self) -> Option<&CompletedScan> {
        self.selected
            .and_then(|id| self.get(id))
            .and_then(|e| e.scan.completed())
    }
    pub fn preview(&self) -> Option<&crate::scanner::preview::Model> {
        self.selected
            .and_then(|id| self.get(id))
            .and_then(|entry| entry.scan.preview())
    }
    pub fn take_finished_preview(&mut self) -> Option<crate::scanner::preview::Model> {
        let id = self.selected?;
        self.entries
            .iter_mut()
            .find(|e| e.id == id)?
            .scan
            .take_finished_preview()
    }
    pub fn snapshots(&self) -> Vec<Snapshot> {
        self.entries
            .iter()
            .filter(|e| !e.stale)
            .filter_map(|e| {
                e.scan.completed().map(|c| Snapshot {
                    entries: c.entries.clone(),
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
    fn an_unreadable_new_root_cannot_replace_a_completed_scan() {
        let fixture = crate::duplicates::tests::Fixture::new();
        let blocked = fixture.0.join("blocked");
        std::fs::create_dir(&blocked).unwrap();
        std::fs::write(blocked.join("sample.bin"), [0; 9]).unwrap();
        let mut locations = Locations::default();
        let finish = |locations: &mut Locations| {
            let deadline = Instant::now() + Duration::from_secs(5);
            while locations.is_running() {
                locations.poll();
                assert!(Instant::now() < deadline);
                std::thread::yield_now();
            }
        };
        locations.start(fixture.0.clone()).unwrap();
        finish(&mut locations);
        let old = Arc::clone(&locations.completed().unwrap().result);
        let plan = locations.plan_root_replacement(&blocked).unwrap().unwrap();

        struct RestoreListPermission(PathBuf, String);
        impl Drop for RestoreListPermission {
            fn drop(&mut self) {
                let _ = std::process::Command::new("icacls")
                    .arg(&self.0)
                    .args(["/remove:d", &self.1])
                    .output();
            }
        }
        let account = format!(
            "{}\\{}",
            std::env::var("USERDOMAIN").unwrap(),
            std::env::var("USERNAME").unwrap()
        );
        let _restore = RestoreListPermission(blocked.clone(), account.clone());
        let denied = std::process::Command::new("icacls")
            .arg(&blocked)
            .arg("/deny")
            .arg(format!("{account}:(RD)"))
            .output()
            .unwrap();
        assert!(denied.status.success(), "{denied:?}");

        locations.start_root_replacement(plan).unwrap();
        finish(&mut locations);
        assert!(matches!(locations.status(), ScanStatus::Failed(_)));
        assert!(Arc::ptr_eq(&old, &locations.completed().unwrap().result));
    }

    #[test]
    fn parent_replacement_publishes_a_single_root_and_preserves_unrelated_results() {
        let fixture = crate::duplicates::tests::Fixture::new();
        let parent = fixture.0.join("parent");
        let first = parent.join("first");
        let second = parent.join("second");
        let outside = fixture.0.join("outside");
        for (folder, size) in [(&first, 2), (&second, 3), (&outside, 11)] {
            std::fs::create_dir_all(folder).unwrap();
            std::fs::write(folder.join("data.bin"), vec![0; size]).unwrap();
        }
        std::fs::write(parent.join("direct.bin"), [0; 7]).unwrap();

        let mut locations = Locations::default();
        let ids = [&first, &second, &outside].map(|folder| locations.add(folder).unwrap());
        let finish = |locations: &mut Locations| {
            let deadline = Instant::now() + Duration::from_secs(5);
            while locations.is_running() {
                locations.poll();
                assert!(Instant::now() < deadline);
                std::thread::yield_now();
            }
        };
        let totals = |locations: &Locations| -> u64 {
            locations
                .snapshots()
                .iter()
                .map(|item| item.scan.folders[0].logical_bytes)
                .sum()
        };
        locations.start_selected(&ids).unwrap();
        finish(&mut locations);
        let unrelated = locations.completed_snapshot(ids[2]).unwrap();

        let plan = locations.plan_root_replacement(&parent).unwrap().unwrap();
        assert_eq!(plan.overlaps, [ids[0], ids[1]]);
        locations.start_root_replacement(plan).unwrap();
        assert_eq!(totals(&locations), 16);
        locations.cancel();
        finish(&mut locations);
        assert!(matches!(locations.status(), ScanStatus::Cancelled));
        assert_eq!(totals(&locations), 16);
        assert_eq!(locations.selected_id(), Some(ids[0]));

        let plan = locations.plan_root_replacement(&parent).unwrap().unwrap();
        let moved = fixture.0.join("moved");
        std::fs::rename(&parent, &moved).unwrap();
        assert!(locations.start_root_replacement(plan).is_err());
        assert_eq!(totals(&locations), 16);
        std::fs::rename(&moved, &parent).unwrap();

        let plan = locations.plan_root_replacement(&parent).unwrap().unwrap();
        locations.start_root_replacement(plan).unwrap();
        finish(&mut locations);
        assert_eq!(locations.completed().unwrap().path, parent);
        assert_eq!(locations.snapshots().len(), 2);
        assert_eq!(totals(&locations), 23);
        assert!(Arc::ptr_eq(
            &unrelated.scan,
            &locations.completed_snapshot(ids[2]).unwrap().scan
        ));
    }

    #[test]
    fn preview_is_location_scoped_and_never_published_as_a_saved_or_searchable_result() {
        let root = std::env::temp_dir().join(format!(
            "diskweave-preview-locations-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(root.join("first/child")).unwrap();
        std::fs::create_dir_all(root.join("second")).unwrap();
        let mut locations = Locations::default();
        let first = locations.add(&root.join("first")).unwrap();
        let second = locations.add(&root.join("second")).unwrap();
        locations.choose(first);
        locations.start_selected(&[first]).unwrap();
        assert_eq!(
            locations.preview().unwrap().root,
            std::path::absolute(root.join("first")).unwrap()
        );
        assert!(locations.snapshots().is_empty());
        assert!(locations.snapshots().is_empty());
        assert_eq!(locations.get(first).unwrap().generation, 0);
        locations.choose(second);
        assert!(locations.preview().is_none());
        locations.choose(first);
        locations.cancel();
        assert!(locations.preview().is_none());
        let deadline = Instant::now() + Duration::from_secs(5);
        while locations.is_running() && Instant::now() < deadline {
            locations.poll();
            std::thread::yield_now();
        }
        assert!(!locations.is_running());
        assert!(locations.snapshots().is_empty());
        locations.start_selected(&[first]).unwrap();
        while locations.is_running() && Instant::now() < deadline {
            locations.poll();
            std::thread::yield_now();
        }
        assert_eq!(locations.snapshots().len(), 1);
        assert_eq!(locations.get(first).unwrap().generation, 1);
        assert!(locations.preview().is_none());
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn tracking_cursor_updates_preserve_visible_generation_without_losing_the_checkpoint() {
        let root =
            std::env::temp_dir().join(format!("diskweave-tracked-location-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let mut locations = Locations::default();
        let id = locations.add(&root).unwrap();
        let now = std::time::SystemTime::now();
        let mut record = crate::snapshot::SavedScan::new(
            Arc::new(crate::scanner::scan(&root, || false).unwrap()),
            now,
            now,
            Duration::ZERO,
        );
        record.tracking = Some(crate::usn::Checkpoint {
            version: 1,
            volume: "test".into(),
            root: 1,
            journal: 7,
            next: 100,
            folders: vec![1],
            files: vec![],
            excluded: vec![],
            pending: vec![],
            unavailable: vec![],
            confirmed_after: None,
        });
        assert!(locations.accept_tracking(id, &record).unwrap());
        let generation = locations.get(id).unwrap().generation;
        let epoch = locations.epoch;
        record.tracking.as_mut().unwrap().next = 108;
        record.tracking.as_mut().unwrap().confirmed_after = Some(now);
        assert!(!locations.accept_tracking(id, &record).unwrap());
        assert_eq!(locations.get(id).unwrap().generation, generation);
        assert_eq!(locations.epoch, epoch);
        assert_eq!(
            locations.entries[0]
                .scan
                .completed()
                .unwrap()
                .tracking
                .as_ref()
                .unwrap()
                .confirmed_after,
            Some(now)
        );
        locations.tracking_failed(id);
        let stale_epoch = locations.epoch;
        assert!(!locations.accept_tracking(id, &record).unwrap());
        assert!(!locations.get(id).unwrap().stale);
        assert!(locations.epoch > stale_epoch);
        assert_eq!(
            locations.entries[0]
                .scan
                .completed()
                .unwrap()
                .tracking
                .as_ref()
                .unwrap()
                .next,
            108
        );
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn saved_workspace_displays_the_recorded_engine_and_collection_options() {
        let root =
            std::env::temp_dir().join(format!("diskweave-saved-method-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let mut record = crate::snapshot::SavedScan::new(
            Arc::new(crate::scanner::scan(&root, || false).unwrap()),
            std::time::SystemTime::now(),
            std::time::SystemTime::now(),
            Duration::ZERO,
        );
        record.engine = Some(crate::engine::Report {
            requested: crate::engine::Method::Ntfs,
            actual: crate::engine::Method::Ntfs,
            fallback: None,
            ..Default::default()
        });
        let view = Locations::from_saved(&record);
        assert!(!view.completed().unwrap().result.detailed);
        assert_eq!(
            view.completed().unwrap().engine.as_ref().unwrap().actual,
            crate::engine::Method::Ntfs
        );
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn queued_locations_keep_one_metadata_mode_until_the_batch_finishes() {
        let root =
            std::env::temp_dir().join(format!("diskweave-location-options-{}", std::process::id()));
        for name in ["a", "b"] {
            std::fs::create_dir_all(root.join(name)).unwrap();
            std::fs::write(root.join(name).join("data"), [0; 3]).unwrap();
        }
        let mut locations = Locations::default();
        let ids = [
            locations.add(&root.join("a")).unwrap(),
            locations.add(&root.join("b")).unwrap(),
        ];
        locations
            .configure_parallelism(crate::scanner::Parallelism::Two)
            .unwrap();
        locations.start_selected(&ids).unwrap();
        assert!(locations
            .configure_parallelism(crate::scanner::Parallelism::Eight)
            .is_err());
        let deadline = Instant::now() + Duration::from_secs(5);
        while locations.is_running() && Instant::now() < deadline {
            locations.poll();
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(!locations.is_running());
        for id in ids {
            assert_eq!(
                locations.get(id).unwrap().scan.parallelism(),
                crate::scanner::Parallelism::Two
            );
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
