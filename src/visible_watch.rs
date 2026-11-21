use std::{
    collections::BTreeMap,
    ffi::{OsStr, OsString},
    fs::{self, File},
    io,
    os::windows::{
        ffi::OsStringExt,
        fs::{MetadataExt, OpenOptionsExt},
        io::{AsRawHandle, FromRawHandle, OwnedHandle},
    },
    path::{Component, Path, PathBuf, Prefix},
    ptr::{null, null_mut},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{self, Receiver, TryRecvError},
        Arc,
    },
    time::{Duration, Instant, SystemTime},
};
use windows_sys::Win32::{
    Foundation::{ERROR_IO_PENDING, ERROR_NOTIFY_ENUM_DIR, WAIT_FAILED, WAIT_OBJECT_0},
    Storage::FileSystem::{
        GetDriveTypeW, GetFileInformationByHandle, ReadDirectoryChangesW,
        BY_HANDLE_FILE_INFORMATION, FILE_ACTION_ADDED, FILE_ACTION_MODIFIED, FILE_ACTION_REMOVED,
        FILE_ACTION_RENAMED_NEW_NAME, FILE_ACTION_RENAMED_OLD_NAME, FILE_FLAG_BACKUP_SEMANTICS,
        FILE_FLAG_OPEN_REPARSE_POINT, FILE_FLAG_OVERLAPPED, FILE_LIST_DIRECTORY,
        FILE_NOTIFY_CHANGE_ATTRIBUTES, FILE_NOTIFY_CHANGE_DIR_NAME, FILE_NOTIFY_CHANGE_FILE_NAME,
        FILE_NOTIFY_CHANGE_LAST_WRITE, FILE_NOTIFY_CHANGE_SIZE,
    },
    System::{
        Threading::{CreateEventW, ResetEvent, WaitForSingleObject},
        IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED},
    },
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub folder: bool,
    pub bytes: u64,
    pub modified: Option<SystemTime>,
    pub created: Option<SystemTime>,
}

type Entries = BTreeMap<OsString, Entry>;
type Changes = Vec<(OsString, Option<Entry>)>;

enum Update {
    Listing(Entries),
    Changes(Changes),
    Problem(String),
}

pub struct FolderWatch {
    receiver: Receiver<Update>,
    stopped: Arc<AtomicBool>,
    entries: Option<Entries>,
    error: Option<String>,
    revision: u64,
    instance: u64,
    #[cfg(test)]
    listings_received: usize,
}

impl FolderWatch {
    pub fn start(folder: PathBuf, wake: impl Fn() + Send + 'static) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        let (sender, receiver) = mpsc::sync_channel(8);
        let stopped = Arc::new(AtomicBool::new(false));
        let cancel = Arc::clone(&stopped);
        std::thread::spawn(move || {
            let mut publish = |update| {
                sender
                    .send(update)
                    .map_err(|_| io::Error::from(io::ErrorKind::Interrupted))?;
                wake();
                Ok(())
            };
            let cancelled = || cancel.load(Ordering::Relaxed);
            if let Err(error) = watch_folder(&folder, &cancelled, &mut publish) {
                if !cancelled() {
                    let _ = publish(Update::Problem(error.to_string()));
                }
            }
        });
        Self {
            receiver,
            stopped,
            entries: None,
            error: None,
            revision: 0,
            instance: NEXT.fetch_add(1, Ordering::Relaxed),
            #[cfg(test)]
            listings_received: 0,
        }
    }

    pub fn entries(&self) -> Option<&BTreeMap<OsString, Entry>> {
        self.entries.as_ref()
    }

    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    pub fn version(&self) -> (u64, u64) {
        (self.instance, self.revision)
    }

    pub fn poll(&mut self) -> bool {
        for _ in 0..8 {
            let update = match self.receiver.try_recv() {
                Ok(update) => update,
                Err(TryRecvError::Empty) => return false,
                Err(TryRecvError::Disconnected) => {
                    self.error
                        .get_or_insert_with(|| "폴더 자동 갱신 연결이 종료됐습니다".into());
                    return false;
                }
            };
            match update {
                Update::Listing(rows) => {
                    #[cfg(test)]
                    {
                        self.listings_received += 1;
                    }
                    if self.entries.as_ref() != Some(&rows) {
                        self.entries = Some(rows);
                        self.revision += 1;
                    }
                    self.error = None;
                }
                Update::Changes(changes) => {
                    let Some(entries) = &mut self.entries else {
                        continue;
                    };
                    let mut changed = false;
                    for (name, entry) in changes {
                        match entry {
                            Some(entry) => {
                                if entries.get(&name) != Some(&entry) {
                                    entries.insert(name, entry);
                                    changed = true;
                                }
                            }
                            None => changed |= entries.remove(&name).is_some(),
                        }
                    }
                    self.revision += u64::from(changed);
                }
                Update::Problem(error) => self.error = Some(error),
            }
        }
        true
    }
}

impl Drop for FolderWatch {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Relaxed);
    }
}

pub fn is_local_folder(path: &Path) -> bool {
    let Some(Component::Prefix(prefix)) = path.components().next() else {
        return false;
    };
    let (Prefix::Disk(letter) | Prefix::VerbatimDisk(letter)) = prefix.kind() else {
        return false;
    };
    let root = [u16::from(letter), b':' as u16, b'\\' as u16, 0];
    path.is_absolute() && matches!(unsafe { GetDriveTypeW(root.as_ptr()) }, 2 | 3)
}

#[derive(Default)]
pub struct VisibleWatches {
    folders: BTreeMap<PathBuf, FolderWatch>,
}

impl VisibleWatches {
    pub fn sync(&mut self, visible: &[PathBuf], ctx: &eframe::egui::Context) {
        self.folders.retain(|path, _| visible.contains(path));
        for path in visible {
            if !self.folders.contains_key(path) && is_local_folder(path) {
                let ctx = ctx.clone();
                self.folders.insert(
                    path.clone(),
                    FolderWatch::start(path.clone(), move || {
                        ctx.request_repaint_of(eframe::egui::ViewportId::ROOT)
                    }),
                );
            }
        }
        for watch in self.folders.values_mut() {
            if watch.poll() {
                ctx.request_repaint();
            }
        }
    }

    pub fn get(&self, path: &Path) -> Option<&FolderWatch> {
        self.folders.get(path)
    }

    pub fn clear(&mut self) {
        self.folders.clear();
    }
}

fn open_folder(path: &Path, flags: u32) -> io::Result<File> {
    let file = File::options()
        .access_mode(FILE_LIST_DIRECTORY)
        .share_mode(7)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT | flags)
        .open(path)?;
    let info = file.metadata()?;
    if !info.is_dir() || info.file_attributes() & 0x400 != 0 {
        return Err(io::Error::other("일반 폴더만 자동 갱신할 수 있습니다"));
    }
    Ok(file)
}

fn identity(file: &File) -> io::Result<(u32, u32, u32)> {
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok((
        info.dwVolumeSerialNumber,
        info.nFileIndexHigh,
        info.nFileIndexLow,
    ))
}

struct Notice<'a> {
    file: &'a File,
    event: OwnedHandle,
    overlap: Box<OVERLAPPED>,
    data: Vec<u32>,
    armed: bool,
}

impl<'a> Notice<'a> {
    fn new(file: &'a File) -> io::Result<Self> {
        let event = unsafe { CreateEventW(null(), 1, 0, null()) };
        if event.is_null() {
            return Err(io::Error::last_os_error());
        }
        // 취소 완료까지 OVERLAPPED와 버퍼의 주소 고정.
        Ok(Self {
            file,
            event: unsafe { OwnedHandle::from_raw_handle(event) },
            overlap: Box::default(),
            data: vec![0; 16 * 1024],
            armed: false,
        })
    }

    fn arm(&mut self) -> io::Result<()> {
        if unsafe { ResetEvent(self.event.as_raw_handle()) } == 0 {
            return Err(io::Error::last_os_error());
        }
        *self.overlap = OVERLAPPED {
            hEvent: self.event.as_raw_handle(),
            ..OVERLAPPED::default()
        };
        let ok = unsafe {
            ReadDirectoryChangesW(
                self.file.as_raw_handle(),
                self.data.as_mut_ptr().cast(),
                (self.data.len() * 4) as u32,
                0,
                FILE_NOTIFY_CHANGE_FILE_NAME
                    | FILE_NOTIFY_CHANGE_DIR_NAME
                    | FILE_NOTIFY_CHANGE_SIZE
                    | FILE_NOTIFY_CHANGE_LAST_WRITE
                    | FILE_NOTIFY_CHANGE_ATTRIBUTES,
                null_mut(),
                self.overlap.as_mut(),
                None,
            )
        };
        if ok == 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() != Some(ERROR_IO_PENDING as i32) {
                return Err(error);
            }
        }
        self.armed = true;
        Ok(())
    }

    fn wait(&mut self) -> io::Result<Option<u32>> {
        match unsafe { WaitForSingleObject(self.event.as_raw_handle(), 50) } {
            WAIT_OBJECT_0 => {
                let mut bytes = 0;
                let ok = unsafe {
                    GetOverlappedResult(
                        self.file.as_raw_handle(),
                        self.overlap.as_mut(),
                        &mut bytes,
                        0,
                    )
                };
                self.armed = false;
                if ok == 0 {
                    let error = io::Error::last_os_error();
                    if error.raw_os_error() != Some(ERROR_NOTIFY_ENUM_DIR as i32) {
                        return Err(error);
                    }
                    bytes = 0;
                }
                Ok(Some(bytes))
            }
            WAIT_FAILED => Err(io::Error::last_os_error()),
            _ => Ok(None),
        }
    }

    fn names(&self, bytes: u32) -> Option<BTreeMap<OsString, bool>> {
        if bytes == 0 || bytes as usize > self.data.len() * 4 {
            return None;
        }
        // 완료 후 Windows가 기록한 범위만 읽는 DWORD 정렬 버퍼.
        let data =
            unsafe { std::slice::from_raw_parts(self.data.as_ptr().cast::<u8>(), bytes as usize) };
        parse_names(data)
    }
}

impl Drop for Notice<'_> {
    fn drop(&mut self) {
        if self.armed {
            let mut ignored = 0;
            unsafe {
                CancelIoEx(self.file.as_raw_handle(), self.overlap.as_mut());
                GetOverlappedResult(
                    self.file.as_raw_handle(),
                    self.overlap.as_mut(),
                    &mut ignored,
                    1,
                );
            }
        }
    }
}

fn parse_names(bytes: &[u8]) -> Option<BTreeMap<OsString, bool>> {
    let mut names = BTreeMap::new();
    let mut at = 0usize;
    loop {
        let record = bytes.get(at..)?;
        let uint = |offset| {
            Some(u32::from_le_bytes(record.get(offset..offset + 4)?.try_into().ok()?) as usize)
        };
        let next = uint(0)?;
        let exists = match uint(4)? as u32 {
            FILE_ACTION_ADDED | FILE_ACTION_MODIFIED | FILE_ACTION_RENAMED_NEW_NAME => true,
            FILE_ACTION_REMOVED | FILE_ACTION_RENAMED_OLD_NAME => false,
            _ => return None,
        };
        let size = uint(8)?;
        let end = 12usize.checked_add(size)?;
        if size == 0 || size % 2 != 0 || (next != 0 && (next < end || next % 4 != 0)) {
            return None;
        }
        let units: Vec<_> = record
            .get(12..end)?
            .as_chunks::<2>()
            .0
            .iter()
            .copied()
            .map(u16::from_le_bytes)
            .collect();
        let name = OsString::from_wide(&units);
        if units.iter().any(|&unit| matches!(unit, 0 | 47 | 58 | 92)) || name == "." || name == ".."
        {
            return None;
        }
        names.insert(name, exists);
        if next == 0 {
            return Some(names);
        }
        at = at.checked_add(next)?;
    }
}

fn listing(folder: &Path, cancel: &impl Fn() -> bool) -> io::Result<Entries> {
    use crate::scanner::directory::{collect, Strategy};
    let mut result = BTreeMap::new();
    collect(folder, Strategy::LargeFetch, cancel, &mut |rows| {
        for row in rows {
            let row = row.map_err(|(_, error)| error)?;
            if row.attributes & 0x400 == 0 {
                result.insert(
                    row.name,
                    Entry {
                        folder: row.attributes & 0x10 != 0,
                        bytes: row.logical_bytes,
                        modified: row.modified,
                        created: row.created,
                    },
                );
            }
        }
        Ok(())
    })?;
    Ok(result)
}

fn read_entry(folder: &Path, name: &OsStr) -> io::Result<Option<Entry>> {
    match fs::symlink_metadata(folder.join(name)) {
        Ok(meta) if meta.file_attributes() & 0x400 == 0 => Ok(Some(Entry {
            folder: meta.is_dir(),
            bytes: meta.len(),
            modified: meta.modified().ok(),
            created: meta.created().ok(),
        })),
        Ok(_) => Ok(None),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn watch_folder(
    folder: &Path,
    cancel: &impl Fn() -> bool,
    publish: &mut impl FnMut(Update) -> io::Result<()>,
) -> io::Result<()> {
    let file = open_folder(folder, FILE_FLAG_OVERLAPPED)?;
    let initial_id = identity(&file)?;
    let verify = || {
        if identity(&open_folder(folder, 0)?)? == initial_id {
            return Ok(());
        }
        Err(io::Error::other(
            "보고 있는 폴더가 다른 폴더로 교체됐습니다",
        ))
    };
    let mut notice = Notice::new(&file)?;
    notice.arm()?;
    verify()?;
    let current = listing(folder, cancel)?;
    verify()?;
    publish(Update::Listing(current))?;
    let mut root_check = Instant::now();
    let mut retry = None;
    loop {
        crate::output::check(cancel)?;
        let received = notice.wait()?;
        let count = if let Some(bytes) = received {
            bytes
        } else {
            if root_check.elapsed() >= Duration::from_secs(1) {
                verify()?;
                root_check = Instant::now();
            }
            if retry.is_none_or(|when| Instant::now() < when) {
                continue;
            }
            0
        };
        let names = if received.is_some() {
            let names = notice.names(count);
            notice.arm()?;
            names
        } else {
            None
        };
        if retry.is_some_and(|when| Instant::now() < when) {
            continue;
        }
        verify()?;
        root_check = Instant::now();
        let update = match names.filter(|_| retry.is_none()) {
            Some(names) => names
                .into_iter()
                .map(|(name, exists)| {
                    crate::output::check(cancel)?;
                    // 이름의 대소문자만 바뀐 경우 기존 철자로 다시 조회하면 동일 파일을 읽으므로 제거 기록 우선.
                    let value = if exists {
                        read_entry(folder, &name)?
                    } else {
                        None
                    };
                    Ok((name, value))
                })
                .collect::<io::Result<Changes>>()
                .map(Update::Changes),
            None => listing(folder, cancel).map(Update::Listing),
        };
        verify()?;
        match update {
            Ok(update) => {
                publish(update)?;
                retry = None;
            }
            Err(error) => {
                crate::output::check(cancel)?;
                if retry.is_none() {
                    publish(Update::Problem(error.to_string()))?;
                }
                retry = Some(Instant::now() + Duration::from_millis(750));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        ffi::OsStr,
        fs,
        time::{Duration, Instant},
    };

    fn await_update(watch: &mut FolderWatch, predicate: impl Fn(&FolderWatch) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            watch.poll();
            if predicate(watch) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "폴더 갱신 대기 시간 초과: {:?}",
                watch.error()
            );
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    #[test]
    fn overflowing_the_windows_buffer_reloads_only_the_direct_children() {
        let root = crate::duplicates::tests::Fixture::new();
        fs::create_dir(root.0.join("nested")).unwrap();
        fs::write(root.0.join("nested/ignored.txt"), b"ignored").unwrap();
        let (release, gate) = std::sync::mpsc::channel();
        let first = AtomicBool::new(true);
        let mut observer = FolderWatch::start(root.0.clone(), move || {
            if first.swap(false, Ordering::Relaxed) {
                gate.recv_timeout(Duration::from_secs(10)).unwrap();
            }
        });
        await_update(&mut observer, |watch| watch.listings_received == 1);
        for number in 0..350 {
            let name = format!("{}-{number:04}.txt", "long".repeat(35));
            fs::write(root.0.join(&name), b"fixture").unwrap();
        }
        release.send(()).unwrap();
        await_update(&mut observer, |watch| {
            watch.listings_received > 1 && watch.entries().unwrap().len() == 351
        });
        assert!(!observer
            .entries()
            .unwrap()
            .contains_key(OsStr::new("ignored.txt")));
        assert!(observer.error().is_none());
    }

    #[test]
    fn replacing_the_watched_folder_keeps_its_last_listing_and_reports_an_error() {
        let root = crate::duplicates::tests::Fixture::new();
        let folder = root.0.join("folder");
        fs::create_dir(&folder).unwrap();
        fs::write(folder.join("original.txt"), b"old").unwrap();
        let mut observer = FolderWatch::start(folder.clone(), || {});
        await_update(&mut observer, |watch| watch.entries().is_some());
        fs::rename(&folder, root.0.join("moved")).unwrap();
        fs::create_dir(&folder).unwrap();
        fs::write(folder.join("new.txt"), b"new").unwrap();
        await_update(&mut observer, |watch| watch.error().is_some());
        assert!(observer
            .entries()
            .unwrap()
            .contains_key(OsStr::new("original.txt")));
        assert!(!observer
            .entries()
            .unwrap()
            .contains_key(OsStr::new("new.txt")));
    }

    #[test]
    fn watches_real_direct_creates_writes_renames_and_deletes() {
        let root = crate::duplicates::tests::Fixture::new();
        fs::create_dir(root.0.join("nested")).unwrap();
        let (wake, pulse) = std::sync::mpsc::channel();
        let mut observer = FolderWatch::start(root.0.clone(), move || {
            let _ = wake.send(());
        });
        let mut observe = |check: &dyn Fn(&FolderWatch) -> bool| {
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                observer.poll();
                assert!(observer.error().is_none(), "{:?}", observer.error());
                if check(&observer) {
                    break;
                }
                assert!(Instant::now() < deadline, "폴더 변경 응답 시간 초과");
                let _ = pulse.recv_timeout(Duration::from_millis(50));
            }
        };
        let original = OsStr::new("한글 report.txt");
        let renamed = OsStr::new("renamed.txt");
        observe(&|folder| folder.entries().is_some());
        fs::write(root.0.join(original), b"abc").unwrap();
        observe(&|folder| {
            folder
                .entries()
                .unwrap()
                .get(original)
                .is_some_and(|row| row.bytes == 3)
        });
        fs::write(root.0.join(original), b"larger data").unwrap();
        observe(&|folder| {
            folder
                .entries()
                .unwrap()
                .get(original)
                .is_some_and(|row| row.bytes == 11)
        });
        fs::rename(root.0.join(original), root.0.join(renamed)).unwrap();
        observe(&|folder| {
            !folder.entries().unwrap().contains_key(original)
                && folder.entries().unwrap().contains_key(renamed)
        });
        let uppercase = OsStr::new("Renamed.txt");
        fs::rename(root.0.join(renamed), root.0.join(uppercase)).unwrap();
        observe(&|folder| {
            !folder.entries().unwrap().contains_key(renamed)
                && folder.entries().unwrap().contains_key(uppercase)
        });
        fs::remove_file(root.0.join(uppercase)).unwrap();
        observe(&|folder| !folder.entries().unwrap().contains_key(uppercase));
        fs::write(root.0.join("nested/hidden.txt"), b"hidden").unwrap();
        observer.poll();
        assert_eq!(observer.entries().unwrap().len(), 1);
    }
}
