use std::{
    ffi::{OsStr, OsString},
    fs::{self, File},
    io,
    os::windows::{
        ffi::OsStrExt,
        fs::{MetadataExt, OpenOptionsExt},
        io::AsRawHandle,
    },
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};
use windows_sys::Win32::{Foundation::GENERIC_READ, Storage::FileSystem::*};
#[derive(Clone, Debug)]
pub enum Action {
    NewFolder { parent: PathBuf, name: OsString },
    Rename { name: OsString },
    Copy { destination: PathBuf },
    Move { destination: PathBuf },
    Recycle,
}
impl Action {
    pub fn label(&self) -> &str {
        match self {
            Self::NewFolder { .. } => "새 폴더",
            Self::Rename { .. } => "이름 변경",
            Self::Copy { .. } => "복사",
            Self::Move { .. } => "이동",
            Self::Recycle => "휴지통으로 이동",
        }
    }
}
#[derive(Clone)]
pub struct Request {
    pub action: Action,
    pub paths: Vec<PathBuf>,
    pub protected: Vec<PathBuf>,
}
#[derive(Clone, Debug)]
pub struct Target {
    pub path: PathBuf,
    pub directory: bool,
    id: (u64, [u8; 16]),
    pub bytes: u64,
    modified: u64,
}
impl Target {
    pub fn capture(path: &Path) -> io::Result<Self> {
        let path = std::path::absolute(path)?;
        for part in path.ancestors() {
            if fs::symlink_metadata(part)?.file_attributes() & 0x400 != 0 {
                return Err(io::Error::other(
                    "Junction·Symbolic Link 경로는 변경할 수 없습니다",
                ));
            }
        }
        let file = File::options()
            .access_mode(FILE_READ_ATTRIBUTES)
            .share_mode(7)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
            .open(&path)?;
        Self::from_file(path, &file)
    }
    fn from_file(path: PathBuf, file: &File) -> io::Result<Self> {
        let metadata = file.metadata()?;
        if metadata.file_attributes() & 0x400 != 0 {
            return Err(io::Error::other("재분석 지점은 변경할 수 없습니다"));
        }
        let mut info: FILE_ID_INFO = unsafe { std::mem::zeroed() };
        // 같은 열린 핸들의 종류·ID를 확인하며 파일 내용을 읽지 않는 준비 단계.
        if unsafe {
            GetFileInformationByHandleEx(
                file.as_raw_handle(),
                FileIdInfo,
                (&mut info as *mut FILE_ID_INFO).cast(),
                std::mem::size_of_val(&info) as u32,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            path,
            directory: metadata.is_dir(),
            id: (info.VolumeSerialNumber, info.FileId.Identifier),
            bytes: metadata.len(),
            modified: metadata.last_write_time(),
        })
    }
    fn unchanged(&self, now: &Self) -> bool {
        now.id == self.id
            && now.directory == self.directory
            && (self.directory || (now.bytes == self.bytes && now.modified == self.modified))
    }
    pub fn verify(&self) -> io::Result<()> {
        if !self.unchanged(&Self::capture(&self.path)?) {
            return Err(io::Error::other(
                "준비한 뒤 대상이 변경되었습니다. 다시 선택하세요",
            ));
        }
        Ok(())
    }
    fn lock_source(&self) -> io::Result<File> {
        let file = File::options()
            .access_mode(GENERIC_READ)
            .share_mode(1)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
            .open(&self.path)?;
        if !self.unchanged(&Self::from_file(self.path.clone(), &file)?) {
            return Err(io::Error::other("복사 직전 원본이 변경되었습니다"));
        }
        Ok(file)
    }
}
#[derive(Clone, Debug, Default)]
pub struct Progress {
    pub current: PathBuf,
    pub completed: u64,
    pub total: u64,
    pub bytes_done: u64,
    pub bytes_total: u64,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Collision {
    #[default]
    Skip,
    KeepBoth,
    Replace,
}
#[derive(Clone, Debug)]
pub struct PlannedItem {
    pub source: Option<Target>,
    pub path: PathBuf,
    pub destination: PathBuf,
    pub existing: Option<Target>,
    pub collision: Collision,
    pub issue: Option<String>,
    tree: Vec<Target>,
}
#[derive(Clone, Debug)]
pub struct Plan {
    pub action: Action,
    pub items: Vec<PlannedItem>,
    parent: Option<Target>,
}
impl Plan {
    pub fn resolve(&mut self, index: usize, decision: Collision) -> io::Result<()> {
        let item = self
            .items
            .get_mut(index)
            .ok_or_else(|| invalid("선택한 작업 항목이 없습니다"))?;
        if decision == Collision::Replace
            && (item.existing.as_ref().is_none_or(|e| e.directory)
                || item.source.as_ref().is_none_or(|s| s.directory))
        {
            return Err(invalid("파일끼리 충돌할 때만 바꿀 수 있습니다"));
        }
        item.collision = decision;
        Ok(())
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    Completed,
    Skipped,
    Failed(String),
    Cancelled,
    NotProcessed,
}
#[derive(Clone, Debug)]
pub struct ItemResult {
    pub source: Option<PathBuf>,
    pub destination: PathBuf,
    pub outcome: Outcome,
    pub note: Option<String>,
}
impl ItemResult {
    pub fn succeeded(&self) -> bool {
        self.outcome == Outcome::Completed
    }
}
#[derive(Clone, Debug, Default)]
pub struct Report {
    pub action: Option<Action>,
    pub items: Vec<ItemResult>,
    pub affected: Vec<PathBuf>,
    pub cancelled: bool,
}
/// 긴 경로 접두사만 통일하며 원본 경로의 비유니코드 구성요소를 보존.
pub(crate) fn comparable_path(path: &Path) -> io::Result<PathBuf> {
    use std::path::{Component, Prefix};
    let absolute = std::path::absolute(path)?;
    let mut normalized = PathBuf::new();
    for part in absolute.components() {
        match part {
            Component::Prefix(prefix) => match prefix.kind() {
                Prefix::VerbatimDisk(drive) => normalized.push(format!("{}:", drive as char)),
                Prefix::VerbatimUNC(server, share) => {
                    let mut unc = OsString::from("\\\\");
                    unc.push(server);
                    unc.push("\\");
                    unc.push(share);
                    normalized.push(unc);
                }
                _ => normalized.push(part.as_os_str()),
            },
            _ => normalized.push(part.as_os_str()),
        }
    }
    Ok(normalized)
}
pub(crate) fn contains_path(parent: &Path, child: &Path) -> io::Result<bool> {
    crate::locations::contains_path(&comparable_path(parent)?, &comparable_path(child)?)
}
fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}
pub fn validate_name(name: &OsStr) -> io::Result<()> {
    let raw: Vec<_> = name.encode_wide().collect();
    if raw.is_empty()
        || raw.len() > 255
        || raw
            .iter()
            .any(|n| *n < 32 || [34, 42, 47, 58, 60, 62, 63, 92, 124].contains(n))
        || [32, 46].contains(raw.last().unwrap())
    {
        return Err(invalid(
            "이름에 사용할 수 없는 문자·길이·끝 공백이 있습니다",
        ));
    }
    let name = name.to_string_lossy();
    let base = name.split('.').next().unwrap().to_ascii_uppercase();
    if ["CON", "PRN", "AUX", "NUL", "CONIN$", "CONOUT$"].contains(&base.as_str())
        || ((base.starts_with("COM") || base.starts_with("LPT"))
            && base.chars().count() == 4
            && matches!(base.chars().last(), Some('1'..='9' | '¹' | '²' | '³')))
    {
        return Err(invalid("Windows 예약 이름은 사용할 수 없습니다"));
    }
    Ok(())
}
fn wide(path: &Path) -> Vec<u16> {
    path.as_os_str().encode_wide().chain(Some(0)).collect()
}
fn existing(path: &Path) -> io::Result<Option<Target>> {
    match fs::symlink_metadata(path) {
        Ok(_) => Target::capture(path).map(Some),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}
fn collect(source: &Target, cancel: &impl Fn() -> bool) -> io::Result<Vec<Target>> {
    let mut entries = vec![source.clone()];
    let mut pending = vec![source.clone()];
    while let Some(parent) = pending.pop() {
        crate::output::check(cancel)?;
        if parent.directory {
            for child in fs::read_dir(&parent.path)? {
                crate::output::check(cancel)?;
                let child = Target::capture(&child?.path())?;
                if child.directory {
                    pending.push(child.clone());
                }
                entries.push(child);
            }
        }
    }
    entries.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(entries)
}
fn check_tree(item: &PlannedItem, cancel: &impl Fn() -> bool) -> io::Result<()> {
    let source = item
        .source
        .as_ref()
        .ok_or_else(|| invalid("원본 항목이 없습니다"))?;
    let now = collect(source, cancel)?;
    if now.len() != item.tree.len()
        || now
            .iter()
            .zip(&item.tree)
            .any(|(a, b)| a.path != b.path || !b.unchanged(a))
    {
        return Err(io::Error::other("준비한 뒤 폴더 내용이 변경되었습니다"));
    }
    Ok(())
}
fn selected_roots(paths: &[PathBuf]) -> io::Result<Vec<PathBuf>> {
    let mut paths: Vec<_> = paths
        .iter()
        .map(std::path::absolute)
        .collect::<io::Result<_>>()?;
    paths.sort_by_key(|p| p.components().count());
    let mut roots: Vec<PathBuf> = vec![];
    for path in paths {
        let mut covered = false;
        for root in &roots {
            covered |= contains_path(root, &path)?;
        }
        if !covered {
            roots.push(path);
        }
    }
    Ok(roots)
}
fn protect(source: &Target, roots: &[PathBuf]) -> io::Result<()> {
    if source.path.parent().is_none() {
        return Err(invalid("드라이브 자체는 변경할 수 없습니다"));
    }
    for root in roots {
        if contains_path(&source.path, &std::path::absolute(root)?)? {
            return Err(invalid("분석 위치의 루트·상위 폴더는 변경할 수 없습니다"));
        }
    }
    Ok(())
}
pub fn prepare(
    request: Request,
    cancel: &impl Fn() -> bool,
    progress: &mut impl FnMut(Progress),
) -> io::Result<Plan> {
    crate::output::check(cancel)?;
    if matches!(request.action, Action::Recycle) {
        if request.paths.is_empty() {
            return Err(invalid("휴지통으로 옮길 항목을 선택하세요"));
        }
        let mut items = vec![];
        for path in selected_roots(&request.paths)? {
            crate::output::check(cancel)?;
            let mut item = PlannedItem {
                path: path.clone(),
                source: None,
                destination: path.clone(),
                existing: None,
                collision: Collision::Skip,
                issue: None,
                tree: vec![],
            };
            let result: io::Result<()> = (|| {
                let source = Target::capture(&path)?;
                protect(&source, &request.protected)?;
                item.tree = collect(&source, cancel)?;
                item.source = Some(source);
                Ok(())
            })();
            if let Err(error) = result {
                if error.kind() == io::ErrorKind::Interrupted {
                    return Err(error);
                }
                item.issue = Some(error.to_string());
            }
            progress(Progress {
                current: path,
                completed: items.len() as u64,
                ..Default::default()
            });
            items.push(item);
        }
        return Ok(Plan {
            action: request.action,
            items,
            parent: None,
        });
    }
    if let Action::Copy { destination } | Action::Move { destination } = &request.action {
        if request.paths.is_empty() {
            return Err(invalid("복사할 항목을 선택하세요"));
        }
        let parent = Target::capture(destination)?;
        if !parent.directory {
            return Err(invalid("목적지는 폴더여야 합니다"));
        }
        let roots = selected_roots(&request.paths)?;
        let mut items = Vec::new();
        for path in roots {
            crate::output::check(cancel)?;
            let name = path
                .file_name()
                .ok_or_else(|| invalid("드라이브 전체를 파일 작업 대상으로 사용할 수 없습니다"))?;
            let destination = parent.path.join(name);
            let mut item = PlannedItem {
                source: None,
                path: path.clone(),
                destination,
                existing: None,
                collision: Collision::Skip,
                issue: None,
                tree: vec![],
            };
            let result: io::Result<()> = (|| {
                let source = Target::capture(&path)?;
                if matches!(request.action, Action::Move { .. }) {
                    protect(&source, &request.protected)?;
                }
                if contains_path(&source.path, &parent.path)? {
                    return Err(invalid("자기 자신 또는 하위 폴더로 복사할 수 없습니다"));
                }
                item.existing = existing(&item.destination)?;
                if item.existing.as_ref().is_some_and(|e| e.id == source.id) {
                    return Err(invalid("원본과 목적지가 같은 파일입니다"));
                }
                item.tree = collect(&source, cancel)?;
                item.source = Some(source);
                Ok(())
            })();
            if let Err(e) = result {
                if e.kind() == io::ErrorKind::Interrupted {
                    return Err(e);
                }
                item.issue = Some(e.to_string());
            }
            progress(Progress {
                current: path,
                completed: items.len() as u64,
                total: 0,
                ..Default::default()
            });
            items.push(item);
        }
        return Ok(Plan {
            action: request.action,
            items,
            parent: Some(parent),
        });
    }
    let (parent, source, name) = match &request.action {
        Action::NewFolder { parent, name } => {
            if !request.paths.is_empty() {
                return Err(invalid("새 폴더는 현재 폴더를 지정하세요"));
            }
            (Target::capture(parent)?, None, name)
        }
        Action::Rename { name } => {
            if request.paths.len() != 1 {
                return Err(invalid("이름을 바꿀 항목 하나를 선택하세요"));
            }
            let source = Target::capture(&request.paths[0])?;
            for root in &request.protected {
                let root = std::path::absolute(root)?;
                if contains_path(&source.path, &root)? {
                    return Err(invalid("분석 위치의 루트·상위 폴더는 변경할 수 없습니다"));
                }
            }
            let parent = source
                .path
                .parent()
                .ok_or_else(|| invalid("드라이브 자체는 변경할 수 없습니다"))?;
            (Target::capture(parent)?, Some(source), name)
        }
        Action::Copy { .. } | Action::Move { .. } | Action::Recycle => unreachable!(),
    };
    validate_name(name)?;
    if !parent.directory {
        return Err(invalid("목적지는 폴더여야 합니다"));
    }
    let destination = parent.path.join(name);
    if let Some(other) = existing(&destination)? {
        let case_only = if let Some(source) = &source {
            source.id == other.id
                && source.path != destination
                && contains_path(&source.path, &destination)?
        } else {
            false
        };
        if !case_only {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "같은 이름이 이미 있습니다",
            ));
        }
    }
    let tree = source
        .as_ref()
        .map(|s| collect(s, cancel))
        .transpose()?
        .unwrap_or_default();
    progress(Progress {
        current: destination.clone(),
        total: 1,
        ..Default::default()
    });
    Ok(Plan {
        action: request.action,
        items: vec![PlannedItem {
            path: source
                .as_ref()
                .map_or_else(|| parent.path.clone(), |s| s.path.clone()),
            source,
            destination,
            existing: None,
            collision: Collision::Skip,
            issue: None,
            tree,
        }],
        parent: Some(parent),
    })
}
fn rename(from: &Path, to: &Path) -> io::Result<()> {
    let from = wide(from);
    let to = wide(to);
    if unsafe { MoveFileExW(from.as_ptr(), to.as_ptr(), 0) } == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}
static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);
fn temp_name(parent: &Path) -> PathBuf {
    parent.join(format!(
        ".diskweave-{}-{}.partial",
        std::process::id(),
        NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
    ))
}
struct CopyProgress<'a> {
    cancel: &'a dyn Fn() -> bool,
    progress: &'a mut dyn FnMut(Progress),
    value: Progress,
    panicked: bool,
}
unsafe extern "system" fn copying(
    total: i64,
    done: i64,
    _stream_total: i64,
    _stream_done: i64,
    _stream: u32,
    _reason: u32,
    _from: windows_sys::Win32::Foundation::HANDLE,
    _to: windows_sys::Win32::Foundation::HANDLE,
    data: *const std::ffi::c_void,
) -> u32 {
    // CopyFileExW의 동기 호출 동안만 살아 있는 콜백 상태. FFI 밖으로 panic 전파 차단.
    let state = unsafe { &mut *(data as *mut CopyProgress<'_>) };
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        state.value.bytes_total = total.max(0) as u64;
        state.value.bytes_done = done.max(0) as u64;
        (state.progress)(state.value.clone());
        if (state.cancel)() {
            PROGRESS_CANCEL
        } else {
            PROGRESS_CONTINUE
        }
    })) {
        Ok(value) => value,
        Err(_) => {
            state.panicked = true;
            PROGRESS_CANCEL
        }
    }
}
fn copy_file(
    source: &Target,
    to: &Path,
    cancel: &impl Fn() -> bool,
    progress: &mut impl FnMut(Progress),
    value: Progress,
) -> io::Result<()> {
    crate::output::check(cancel)?;
    let _source = source.lock_source()?;
    let from = wide(&source.path);
    let to = wide(to);
    let mut state = CopyProgress {
        cancel,
        progress,
        value,
        panicked: false,
    };
    let result = unsafe {
        CopyFileExW(
            from.as_ptr(),
            to.as_ptr(),
            Some(copying),
            (&mut state as *mut CopyProgress<'_>).cast(),
            std::ptr::null_mut(),
            COPY_FILE_FAIL_IF_EXISTS | COPY_FILE_COPY_SYMLINK,
        )
    };
    if result == 0 {
        crate::output::check(cancel)?;
        if state.panicked {
            return Err(io::Error::other("진행 콜백 처리 실패"));
        }
        return Err(io::Error::last_os_error());
    }
    Ok(())
}
fn keep_both(path: &Path, directory: bool) -> io::Result<PathBuf> {
    for n in 2..=10000 {
        let mut name = if directory {
            path.file_name().unwrap().to_owned()
        } else {
            path.file_stem().unwrap().to_owned()
        };
        name.push(format!(" ({n})"));
        if !directory {
            if let Some(ext) = path.extension() {
                name.push(".");
                name.push(ext);
            }
        }
        let candidate = path.with_file_name(name);
        if existing(&candidate)?.is_none() {
            return Ok(candidate);
        }
    }
    Err(io::Error::other("사용할 수 있는 새 이름을 찾지 못했습니다"))
}
fn copy_item(
    item: &PlannedItem,
    parent: &Target,
    cancel: &impl Fn() -> bool,
    progress: &mut impl FnMut(Progress),
    value: Progress,
    destination: &Path,
) -> io::Result<()> {
    check_tree(item, cancel)?;
    let source = item.source.as_ref().unwrap();
    let staged = temp_name(&parent.path);
    let mut owned = None;
    let result: io::Result<()> = (|| {
        if source.directory {
            fs::create_dir(&staged)?;
            owned = Some(Target::capture(&staged)?);
            let mut entries: Vec<_> = item.tree.iter().filter(|t| t.path != source.path).collect();
            entries.sort_by_key(|t| t.path.components().count());
            for target in entries {
                crate::output::check(cancel)?;
                let to = staged.join(
                    target
                        .path
                        .strip_prefix(&source.path)
                        .map_err(|_| invalid("원본 폴더 범위 오류"))?,
                );
                if target.directory {
                    target.verify()?;
                    fs::create_dir(&to)?;
                } else {
                    copy_file(
                        target,
                        &to,
                        cancel,
                        progress,
                        Progress {
                            current: target.path.clone(),
                            ..value.clone()
                        },
                    )?;
                }
            }
        } else {
            copy_file(source, &staged, cancel, progress, value)?;
            owned = Some(Target::capture(&staged)?);
        }
        crate::output::check(cancel)?;
        parent.verify()?;
        if item.collision == Collision::Replace {
            item.existing.as_ref().unwrap().verify()?;
            let to = wide(destination);
            let temp = wide(&staged);
            if unsafe {
                ReplaceFileW(
                    to.as_ptr(),
                    temp.as_ptr(),
                    std::ptr::null(),
                    0,
                    std::ptr::null(),
                    std::ptr::null(),
                )
            } == 0
            {
                return Err(io::Error::last_os_error());
            }
        } else {
            rename(&staged, destination)?;
        }
        Ok(())
    })();
    if let Err(error) = &result {
        if let Some(owned) = owned {
            if owned.verify().is_ok() {
                let cleanup = if owned.directory {
                    fs::remove_dir_all(&staged)
                } else {
                    fs::remove_file(&staged)
                };
                if let Err(cleanup) = cleanup {
                    return Err(io::Error::new(
                        error.kind(),
                        format!(
                            "{}; 임시 출력 정리 실패 {}: {cleanup}",
                            error,
                            staged.display()
                        ),
                    ));
                }
            } else if staged.try_exists()? {
                return Err(io::Error::new(
                    error.kind(),
                    format!(
                        "{error}; 임시 출력의 식별자가 바뀌어 보존함: {}",
                        staged.display()
                    ),
                ));
            }
        } else if staged.try_exists()? {
            return Err(io::Error::new(
                error.kind(),
                format!(
                    "{error}; 확인하지 못한 임시 출력을 보존함: {}",
                    staged.display()
                ),
            ));
        }
    }
    result
}
fn remove_bound(target: &Target) -> io::Result<()> {
    let file = File::options()
        .access_mode(DELETE | FILE_READ_ATTRIBUTES)
        .share_mode(3)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(&target.path)?;
    if !target.unchanged(&Target::from_file(target.path.clone(), &file)?) {
        return Err(io::Error::other("원본 제거 직전 대상이 변경되었습니다"));
    }
    let info = FILE_DISPOSITION_INFO { DeleteFile: true };
    // 재조회한 바로 그 핸들만 제거 표시하며 경로의 다른 객체를 삭제하지 않는 이동 마무리.
    if unsafe {
        SetFileInformationByHandle(
            file.as_raw_handle(),
            FileDispositionInfo,
            (&info as *const FILE_DISPOSITION_INFO).cast(),
            std::mem::size_of_val(&info) as u32,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}
fn remove_moved_source(item: &PlannedItem, cancel: &impl Fn() -> bool) -> io::Result<()> {
    check_tree(item, cancel)?;
    let mut entries: Vec<_> = item.tree.iter().collect();
    entries.sort_by_key(|target| std::cmp::Reverse(target.path.components().count()));
    for target in entries {
        crate::output::check(cancel)?;
        remove_bound(target)?;
    }
    Ok(())
}
fn move_item(
    item: &PlannedItem,
    parent: &Target,
    cancel: &impl Fn() -> bool,
    progress: &mut impl FnMut(Progress),
    value: Progress,
    destination: &Path,
) -> io::Result<()> {
    check_tree(item, cancel)?;
    crate::output::check(cancel)?;
    // 디렉터리의 교차 볼륨 이름 교체는 ERROR_NOT_SAME_DEVICE 외 오류도 반환 가능하므로 ID로 사전 분기.
    if item.collision != Collision::Replace && item.source.as_ref().unwrap().id.0 == parent.id.0 {
        match rename(&item.source.as_ref().unwrap().path, destination) {
            Ok(()) => return Ok(()),
            Err(error) if error.raw_os_error() == Some(17) => {}
            Err(error) => return Err(error),
        }
    }
    copy_item(item, parent, cancel, progress, value, destination)?;
    remove_moved_source(item, cancel).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("목적지 복사 완료 · 원본 일부 또는 전체 유지: {error}"),
        )
    })
}
pub fn execute(
    plan: Plan,
    cancel: &impl Fn() -> bool,
    progress: &mut impl FnMut(Progress),
) -> Report {
    execute_with_recycler(plan, cancel, progress, &mut crate::recycle::send)
}
pub fn execute_with_recycler(
    plan: Plan,
    cancel: &impl Fn() -> bool,
    progress: &mut impl FnMut(Progress),
    recycle: &mut impl FnMut(&Target) -> io::Result<PathBuf>,
) -> Report {
    let mut report = Report {
        action: Some(plan.action.clone()),
        ..Default::default()
    };
    let total = plan.items.len() as u64;
    for item in plan.items {
        if report.cancelled {
            report.items.push(ItemResult {
                source: item.source.as_ref().map(|s| s.path.clone()),
                destination: item.destination,
                outcome: Outcome::NotProcessed,
                note: None,
            });
            continue;
        }
        let source = item.source.as_ref().map(|s| s.path.clone());
        let mut destination = item.destination.clone();
        let value = Progress {
            current: item.path.clone(),
            completed: report.items.len() as u64,
            total,
            ..Default::default()
        };
        let result = (|| {
            crate::output::check(cancel)?;
            if let Some(issue) = &item.issue {
                return Err(io::Error::other(issue.clone()));
            }
            if let Some(parent) = &plan.parent {
                parent.verify()?;
            }
            if let Some(source) = &item.source {
                source.verify()?;
            }
            if let Some(existing) = &item.existing {
                match item.collision {
                    Collision::Skip => return Ok(Outcome::Skipped),
                    Collision::KeepBoth => {
                        destination =
                            keep_both(&destination, item.source.as_ref().unwrap().directory)?
                    }
                    Collision::Replace => {
                        existing.verify()?;
                    }
                }
            }
            match &plan.action {
                Action::Recycle => {
                    check_tree(&item, cancel)?;
                    destination = recycle(item.source.as_ref().unwrap())?;
                }
                Action::NewFolder { .. } => fs::create_dir(&destination)?,
                Action::Rename { .. } => {
                    check_tree(&item, cancel)?;
                    rename(&item.source.as_ref().unwrap().path, &destination)?;
                }
                Action::Move { .. } => move_item(
                    &item,
                    plan.parent.as_ref().unwrap(),
                    cancel,
                    progress,
                    value.clone(),
                    &destination,
                )?,
                Action::Copy { .. } => copy_item(
                    &item,
                    plan.parent.as_ref().unwrap(),
                    cancel,
                    progress,
                    value.clone(),
                    &destination,
                )?,
            }
            Ok(Outcome::Completed)
        })();
        let mut note = None;
        let outcome = match result {
            Ok(outcome) => outcome,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {
                report.cancelled = true;
                note = Some(e.to_string());
                Outcome::Cancelled
            }
            Err(e) => Outcome::Failed(e.to_string()),
        };
        if outcome != Outcome::Skipped {
            if let Some(parent) = &plan.parent {
                report.affected.push(parent.path.clone());
            }
            if !matches!(plan.action, Action::Copy { .. }) {
                if let Some(path) = &source {
                    report.affected.push(path.clone());
                }
            }
        }
        report.items.push(ItemResult {
            source,
            destination,
            outcome,
            note,
        });
        progress(Progress {
            completed: report.items.len() as u64,
            ..value
        });
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        path::PathBuf,
        time::{SystemTime, UNIX_EPOCH},
    };
    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "diskweave-operations-{}-{}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    #[test]
    fn replacing_a_file_with_alternate_streams_preserves_source_and_replacement_content() {
        let f = Fixture::new();
        fs::create_dir(f.0.join("to")).unwrap();
        fs::write(f.0.join("source"), b"before").unwrap();
        fs::write(f.0.join("source:note"), b"ADS").unwrap();
        let request = Request {
            action: Action::Copy {
                destination: f.0.join("to"),
            },
            paths: vec![f.0.join("source")],
            protected: vec![f.0.clone()],
        };
        assert!(execute(
            prepare(request.clone(), &|| false, &mut |_| {}).unwrap(),
            &|| false,
            &mut |_| {}
        )
        .items[0]
            .succeeded());
        fs::write(f.0.join("source"), b"after").unwrap();
        let mut plan = prepare(request, &|| false, &mut |_| {}).unwrap();
        plan.resolve(0, Collision::Replace).unwrap();
        let report = execute(plan, &|| false, &mut |_| {});
        assert!(report.items[0].succeeded(), "{:?}", report.items);
        assert_eq!(fs::read(f.0.join("to/source")).unwrap(), b"after");
        assert_eq!(fs::read(f.0.join("to/source:note")).unwrap(), b"ADS");
        assert_eq!(fs::read(f.0.join("source:note")).unwrap(), b"ADS");
    }
    #[test]
    fn extended_windows_paths_cannot_bypass_root_protection_or_self_copy_checks() {
        let f = Fixture::new();
        fs::create_dir_all(f.0.join("protected/sub")).unwrap();
        fs::create_dir(f.0.join("to")).unwrap();
        let extended = fs::canonicalize(f.0.join("protected")).unwrap();
        let plan = prepare(
            Request {
                action: Action::Recycle,
                paths: vec![extended.clone()],
                protected: vec![f.0.join("protected")],
            },
            &|| false,
            &mut |_| {},
        )
        .unwrap();
        assert!(plan.items[0].issue.is_some());
        let plan = prepare(
            Request {
                action: Action::Copy {
                    destination: fs::canonicalize(f.0.join("protected/sub")).unwrap(),
                },
                paths: vec![f.0.join("protected")],
                protected: vec![],
            },
            &|| false,
            &mut |_| {},
        )
        .unwrap();
        assert!(plan.items[0].issue.is_some());
    }
    #[test]
    fn replacement_revalidates_destination_and_partial_failures_preserve_other_sources() {
        let f = Fixture::new();
        fs::create_dir(f.0.join("to")).unwrap();
        fs::write(f.0.join("a"), b"A").unwrap();
        fs::write(f.0.join("b"), b"B").unwrap();
        fs::write(f.0.join("to/a"), b"old").unwrap();
        let request = Request {
            action: Action::Copy {
                destination: f.0.join("to"),
            },
            paths: vec![f.0.join("a"), f.0.join("b")],
            protected: vec![f.0.clone()],
        };
        let mut plan = prepare(request, &|| false, &mut |_| {}).unwrap();
        plan.resolve(0, Collision::Replace).unwrap();
        fs::rename(f.0.join("to/a"), f.0.join("to/old-a")).unwrap();
        fs::write(f.0.join("to/a"), b"new target").unwrap();
        let report = execute(plan, &|| false, &mut |_| {});
        assert!(matches!(report.items[0].outcome, Outcome::Failed(_)));
        assert!(report.items[1].succeeded());
        assert_eq!(fs::read(f.0.join("to/a")).unwrap(), b"new target");
        assert_eq!(fs::read(f.0.join("to/b")).unwrap(), b"B");
        assert_eq!(fs::read(f.0.join("a")).unwrap(), b"A");
        assert_eq!(fs::read(f.0.join("b")).unwrap(), b"B");
        fs::create_dir(f.0.join("folder")).unwrap();
        fs::write(f.0.join("folder/keep"), b"tree").unwrap();
        fs::create_dir(f.0.join("to/folder")).unwrap();
        let mut plan = prepare(
            Request {
                action: Action::Copy {
                    destination: f.0.join("to"),
                },
                paths: vec![f.0.join("folder")],
                protected: vec![f.0.clone()],
            },
            &|| false,
            &mut |_| {},
        )
        .unwrap();
        assert!(plan.resolve(0, Collision::Replace).is_err());
        plan.resolve(0, Collision::KeepBoth).unwrap();
        assert!(execute(plan, &|| false, &mut |_| {}).items[0].succeeded());
        assert_eq!(fs::read(f.0.join("to/folder (2)/keep")).unwrap(), b"tree");
        assert_eq!(fs::read_dir(f.0.join("to/folder")).unwrap().count(), 0);
    }
    #[test]
    fn case_only_rename_preserves_content_and_does_not_replace_other_files() {
        let f = Fixture::new();
        let original = f.0.join("Report.txt");
        fs::write(&original, b"content").unwrap();
        let request = Request {
            action: Action::Rename {
                name: "report.TXT".into(),
            },
            paths: vec![original],
            protected: vec![f.0.clone()],
        };
        let plan = prepare(request, &|| false, &mut |_| {}).unwrap();
        assert!(execute(plan, &|| false, &mut |_| {}).items[0].succeeded());
        let names: Vec<_> = fs::read_dir(&f.0)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, vec![OsString::from("report.TXT")]);
        assert_eq!(fs::read(f.0.join("report.TXT")).unwrap(), b"content");
    }
    #[test]
    fn recycle_failures_never_fall_back_to_deletion_and_cancel_keeps_remaining_items() {
        use std::cell::Cell;
        let f = Fixture::new();
        fs::create_dir(f.0.join("fake-bin")).unwrap();
        for name in ["a", "b", "c"] {
            fs::write(f.0.join(name), b"keep").unwrap();
        }
        let request = Request {
            action: Action::Recycle,
            paths: ["a", "b", "c"].iter().map(|n| f.0.join(n)).collect(),
            protected: vec![f.0.clone()],
        };
        let plan = prepare(request.clone(), &|| false, &mut |_| {}).unwrap();
        let report = execute_with_recycler(plan, &|| false, &mut |_| {}, &mut |_| {
            Err(io::Error::new(io::ErrorKind::Unsupported, "휴지통 미지원"))
        });
        assert_eq!(report.items.len(), 3);
        assert!(report
            .items
            .iter()
            .all(|r| matches!(r.outcome, Outcome::Failed(_))));
        for name in ["a", "b", "c"] {
            assert_eq!(fs::read(f.0.join(name)).unwrap(), b"keep");
        }
        let stop = Cell::new(false);
        let plan = prepare(request, &|| false, &mut |_| {}).unwrap();
        let report = execute_with_recycler(plan, &|| stop.get(), &mut |_| {}, &mut |target| {
            let to = f.0.join("fake-bin").join(target.path.file_name().unwrap());
            fs::rename(&target.path, &to)?;
            stop.set(true);
            Ok(to)
        });
        assert!(report.cancelled);
        assert_eq!(report.items.len(), 3);
        assert_eq!(report.items[0].outcome, Outcome::Completed);
        assert_eq!(report.items[2].outcome, Outcome::NotProcessed);
        assert!(f.0.join("b").exists() && f.0.join("c").exists());
    }
    #[test]
    fn moving_uses_confirmed_destinations_and_protects_registered_roots() {
        let f = Fixture::new();
        fs::create_dir_all(f.0.join("from/sub")).unwrap();
        fs::create_dir(f.0.join("to")).unwrap();
        fs::write(f.0.join("from/sub/keep"), b"original bytes").unwrap();
        let request = Request {
            action: Action::Move {
                destination: f.0.join("to"),
            },
            paths: vec![f.0.join("from")],
            protected: vec![f.0.clone()],
        };
        let plan = prepare(request, &|| false, &mut |_| {}).unwrap();
        let report = execute(plan, &|| false, &mut |_| {});
        assert!(report.items[0].succeeded());
        assert!(!f.0.join("from").exists());
        assert_eq!(
            fs::read(f.0.join("to/from/sub/keep")).unwrap(),
            b"original bytes"
        );
        let plan = prepare(
            Request {
                action: Action::Move {
                    destination: f.0.clone(),
                },
                paths: vec![f.0.join("to/from")],
                protected: vec![f.0.join("to/from")],
            },
            &|| false,
            &mut |_| {},
        )
        .unwrap();
        assert!(plan.items[0].issue.is_some());
        assert!(matches!(
            execute(plan, &|| false, &mut |_| {}).items[0].outcome,
            Outcome::Failed(_)
        ));
    }
    #[test]
    fn copy_coalesces_parent_selection_and_refuses_linked_subtrees_and_invalid_names() {
        let f = Fixture::new();
        fs::create_dir_all(f.0.join("source/sub")).unwrap();
        fs::create_dir(f.0.join("to")).unwrap();
        fs::write(f.0.join("source/sub/a"), b"12345").unwrap();
        let request = Request {
            action: Action::Copy {
                destination: f.0.join("to"),
            },
            paths: vec![f.0.join("source/sub/a"), f.0.join("source")],
            protected: vec![f.0.clone()],
        };
        let plan = prepare(request.clone(), &|| false, &mut |_| {}).unwrap();
        assert_eq!(plan.items.len(), 1);
        assert!(execute(plan, &|| false, &mut |_| {}).items[0].succeeded());
        assert_eq!(fs::read(f.0.join("to/source/sub/a")).unwrap(), b"12345");
        let link = f.0.join("source/link");
        use std::os::windows::process::CommandExt;
        let status = std::process::Command::new("cmd.exe")
            .raw_arg(format!(
                "/C mklink /J \"{}\" \"{}\"",
                link.display(),
                f.0.join("to").display()
            ))
            .output()
            .unwrap();
        assert!(status.status.success(), "{:?}", status);
        let plan = prepare(request, &|| false, &mut |_| {}).unwrap();
        assert!(plan.items[0].issue.is_some());
        assert!(matches!(
            execute(plan, &|| false, &mut |_| {}).items[0].outcome,
            Outcome::Failed(_)
        ));
        fs::remove_dir(link).unwrap();
        for name in ["..", "CON.txt", "name.", "a/b", "x:y", "LPT¹"] {
            assert!(validate_name(OsStr::new(name)).is_err());
        }
        assert!(prepare(
            Request {
                action: Action::Rename {
                    name: "other".into()
                },
                paths: vec![f.0.clone()],
                protected: vec![f.0.clone()]
            },
            &|| false,
            &mut |_| {}
        )
        .is_err());
    }
    #[test]
    fn cancelled_replacement_keeps_original_and_changed_sources_fail_individually() {
        use std::sync::atomic::AtomicBool;
        let f = Fixture::new();
        fs::create_dir(f.0.join("to")).unwrap();
        let source = f.0.join("large.bin");
        let file = File::create(&source).unwrap();
        file.set_len(16 * 1024 * 1024).unwrap();
        drop(file);
        fs::write(f.0.join("to/large.bin"), b"keep this").unwrap();
        let request = Request {
            action: Action::Copy {
                destination: f.0.join("to"),
            },
            paths: vec![source.clone()],
            protected: vec![f.0.clone()],
        };
        let mut plan = prepare(request.clone(), &|| false, &mut |_| {}).unwrap();
        plan.resolve(0, Collision::Replace).unwrap();
        let stop = AtomicBool::new(false);
        let report = execute(plan, &|| stop.load(Ordering::Relaxed), &mut |p| {
            if p.bytes_done > 0 {
                stop.store(true, Ordering::Relaxed);
            }
        });
        assert!(report.cancelled);
        assert_eq!(fs::read(f.0.join("to/large.bin")).unwrap(), b"keep this");
        assert!(source.exists());
        assert!(!fs::read_dir(f.0.join("to")).unwrap().any(|e| e
            .unwrap()
            .file_name()
            .to_string_lossy()
            .ends_with(".partial")));
        let plan = prepare(request, &|| false, &mut |_| {}).unwrap();
        fs::rename(&source, f.0.join("moved-original")).unwrap();
        fs::write(&source, b"different object").unwrap();
        let report = execute(plan, &|| false, &mut |_| {});
        assert!(matches!(report.items[0].outcome, Outcome::Failed(_)));
        assert_eq!(fs::read(f.0.join("to/large.bin")).unwrap(), b"keep this");
    }
    #[test]
    fn copying_preserves_colliding_files_until_replacement_is_confirmed() {
        let f = Fixture::new();
        fs::create_dir(f.0.join("to")).unwrap();
        fs::write(f.0.join("data.bin"), b"new contents").unwrap();
        fs::write(f.0.join("to/data.bin"), b"old contents").unwrap();
        let request = Request {
            action: Action::Copy {
                destination: f.0.join("to"),
            },
            paths: vec![f.0.join("data.bin")],
            protected: vec![f.0.clone()],
        };
        let plan = prepare(request.clone(), &|| false, &mut |_| {}).unwrap();
        let report = execute(plan, &|| false, &mut |_| {});
        assert_eq!(report.items[0].outcome, Outcome::Skipped);
        assert_eq!(fs::read(f.0.join("to/data.bin")).unwrap(), b"old contents");
        let mut plan = prepare(request.clone(), &|| false, &mut |_| {}).unwrap();
        plan.resolve(0, Collision::KeepBoth).unwrap();
        assert!(execute(plan, &|| false, &mut |_| {}).items[0].succeeded());
        assert_eq!(
            fs::read(f.0.join("to/data (2).bin")).unwrap(),
            b"new contents"
        );
        let mut plan = prepare(request, &|| false, &mut |_| {}).unwrap();
        plan.resolve(0, Collision::Replace).unwrap();
        assert!(execute(plan, &|| false, &mut |_| {}).items[0].succeeded());
        assert_eq!(fs::read(f.0.join("to/data.bin")).unwrap(), b"new contents");
        assert_eq!(fs::read(f.0.join("data.bin")).unwrap(), b"new contents");
    }
    #[test]
    fn new_folder_and_single_rename_keep_the_existing_contents() {
        let f = Fixture::new();
        let plan = prepare(
            Request {
                action: Action::NewFolder {
                    parent: f.0.clone(),
                    name: "정리본".into(),
                },
                paths: vec![],
                protected: vec![f.0.clone()],
            },
            &|| false,
            &mut |_| {},
        )
        .unwrap();
        let report = execute(plan, &|| false, &mut |_| {});
        assert!(report.items[0].succeeded());
        let folder = f.0.join("정리본");
        fs::write(folder.join("keep.bin"), b"original contents").unwrap();
        let plan = prepare(
            Request {
                action: Action::Rename {
                    name: "완료본".into(),
                },
                paths: vec![folder.clone()],
                protected: vec![f.0.clone()],
            },
            &|| false,
            &mut |_| {},
        )
        .unwrap();
        let report = execute(plan, &|| false, &mut |_| {});
        assert!(report.items[0].succeeded());
        assert!(!folder.exists());
        assert_eq!(
            fs::read(f.0.join("완료본/keep.bin")).unwrap(),
            b"original contents"
        );
    }
}
