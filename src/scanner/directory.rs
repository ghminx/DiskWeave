use std::os::windows::{
    ffi::{OsStrExt, OsStringExt},
    fs::{MetadataExt, OpenOptionsExt},
    io::AsRawHandle,
};
use std::{
    ffi::OsString,
    fs, io,
    path::{Path, PathBuf},
    time::SystemTime,
};
use windows_sys::Win32::{
    Foundation::{FILETIME, HANDLE, INVALID_HANDLE_VALUE},
    Storage::FileSystem::*,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Strategy {
    Reference,
    LargeFetch,
    #[allow(
        dead_code,
        reason = "기본 적용하지 않는 개발용 일괄 열거 비교 경로 유지"
    )]
    Bulk,
    #[allow(dead_code, reason = "제품 기본값과 분리한 개발용 단계별 비교")]
    Experimental(Experiment),
    #[default]
    Combined,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
/// 개발용 일괄 열거 설정. 버퍼 크기는 64 KiB/1 MiB/4 MiB로 제한.
pub struct Experiment {
    pub shared_handle: bool,
    pub reuse_buffer: bool,
    pub buffer_bytes: usize,
}
impl Default for Experiment {
    fn default() -> Self {
        Self {
            shared_handle: false,
            reuse_buffer: false,
            buffer_bytes: 65536,
        }
    }
}
// worker별 동기 열거 저장소. 전달하는 항목에는 이 버퍼의 참조를 보관하지 않음.
thread_local! {static PAGE_BUFFER:std::cell::RefCell<Vec<u64>>=const{std::cell::RefCell::new(Vec::new())};}
pub struct Entry {
    pub path: PathBuf,
    pub name: OsString,
    pub logical_bytes: u64,
    pub attributes: u32,
    pub created: Option<SystemTime>,
    pub modified: Option<SystemTime>,
    pub extra: crate::metadata::Extra,
}
pub type Item = Result<Entry, (PathBuf, io::Error)>;
/// 실제 방문 시 폴더 관찰값과 자식 항목 전달. 열거 실패에도 이미 확인한 폴더 값 유지.
pub fn collect_folder(
    path: &Path,
    strategy: Strategy,
    cancel: &impl Fn() -> bool,
    emit: &mut impl FnMut(Vec<Item>) -> io::Result<()>,
    observe: &mut impl FnMut(Entry),
    trace: Option<&crate::scanner::profile::Trace>,
) -> io::Result<()> {
    use crate::scanner::profile::{scope, Stage};
    crate::output::check(cancel)?;
    let checking = scope(trace, Stage::FolderCheck);
    let experiment = match strategy {
        Strategy::Experimental(value) => value,
        _ => Experiment::default(),
    };
    if experiment.shared_handle {
        let source = Native::open_observed(path, &mut |metadata| {
            observe(Entry::from_metadata(path.to_owned(), metadata))
        });
        drop(checking);
        return match source {
            Ok(mut source) => {
                let _enumeration = scope(trace, Stage::Enumerate);
                collect_native(path, &mut source, cancel, emit, experiment)
            }
            Err(e) if e.kind() == io::ErrorKind::Unsupported => {
                collect_folder(path, Strategy::LargeFetch, cancel, emit, observe, trace)
            }
            Err(e) => Err(e),
        };
    }
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_dir() || metadata.file_attributes() & 0x400 != 0 {
        return Err(io::Error::other("열거 대상의 종류가 변경되었습니다"));
    }
    observe(Entry::from_metadata(path.to_owned(), metadata));
    drop(checking);
    let _enumeration = scope(trace, Stage::Enumerate);
    collect(path, strategy, cancel, emit)
}
impl Entry {
    pub(super) fn from_metadata(path: PathBuf, m: fs::Metadata) -> Self {
        Self {
            // 드라이브 루트에는 파일명이 없으며 폴더 관찰 값에는 이름을 사용하지 않음.
            name: path.file_name().unwrap_or_default().to_owned(),
            path,
            logical_bytes: m.len(),
            attributes: m.file_attributes(),
            created: m.created().ok(),
            modified: m.modified().ok(),
            extra: Default::default(),
        }
    }
}
fn time(value: FILETIME) -> Option<SystemTime> {
    let ticks = ((value.dwHighDateTime as u64) << 32) | value.dwLowDateTime as u64;
    let epoch = 116444736000000000;
    let span = |v: u64| std::time::Duration::new(v / 10000000, ((v % 10000000) * 100) as u32);
    if ticks >= epoch {
        std::time::UNIX_EPOCH.checked_add(span(ticks - epoch))
    } else {
        std::time::UNIX_EPOCH.checked_sub(span(epoch - ticks))
    }
}
struct Find(HANDLE);
impl Drop for Find {
    fn drop(&mut self) {
        // 열거 핸들만 FindClose로 반환.
        unsafe {
            FindClose(self.0);
        }
    }
}
/// 폴더 항목을 제한된 묶음으로 전달. 파일 내용은 읽지 않으며 Reparse 대상 추적 생략.
pub fn collect(
    path: &Path,
    strategy: Strategy,
    cancel: &impl Fn() -> bool,
    emit: &mut impl FnMut(Vec<Item>) -> io::Result<()>,
) -> io::Result<()> {
    crate::output::check(cancel)?;
    if matches!(strategy, Strategy::Bulk | Strategy::Experimental(_)) {
        return match Native::open(path) {
            Ok(mut source) => collect_native(
                path,
                &mut source,
                cancel,
                emit,
                match strategy {
                    Strategy::Experimental(value) => value,
                    _ => Experiment::default(),
                },
            ),
            Err(e) if e.kind() == io::ErrorKind::Unsupported => {
                collect(path, Strategy::LargeFetch, cancel, emit)
            }
            Err(e) => Err(e),
        };
    }
    let mut batch = Vec::with_capacity(128);
    let mut push = |entry: Item| -> io::Result<()> {
        batch.push(entry);
        if batch.len() == 128 {
            emit(std::mem::replace(&mut batch, Vec::with_capacity(128)))?;
        }
        Ok(())
    };
    match strategy {
        Strategy::Reference => {
            for entry in fs::read_dir(path)? {
                crate::output::check(cancel)?;
                match entry {
                    Ok(e) => {
                        let path = e.path();
                        push(
                            e.metadata()
                                .map(|m| Entry::from_metadata(path.clone(), m))
                                .map_err(|error| (path, error)),
                        )?;
                    }
                    Err(e) => {
                        push(Err((path.to_owned(), e)))?;
                        break;
                    }
                }
            }
        }
        Strategy::LargeFetch | Strategy::Combined => {
            let absolute = std::path::absolute(path)?;
            let mut units: Vec<u16> = absolute.as_os_str().encode_wide().collect();
            if !units.starts_with(&[92, 92, 63, 92]) {
                if units.starts_with(&[92, 92]) {
                    let mut prefix: Vec<u16> = "\\\\?\\UNC\\".encode_utf16().collect();
                    prefix.extend_from_slice(&units[2..]);
                    units = prefix;
                } else {
                    let mut prefix: Vec<u16> = "\\\\?\\".encode_utf16().collect();
                    prefix.extend(units);
                    units = prefix;
                }
            }
            if units.contains(&0) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "경로에 NUL이 포함되어 있습니다",
                ));
            }
            if units.last() != Some(&92) {
                units.push(92);
            }
            units.extend([42, 0]);
            let mut data = WIN32_FIND_DATAW::default();
            // UTF-16 NUL 종료 경로와 고정 Win32 출력 버퍼는 호출 동안 유효.
            let handle = unsafe {
                FindFirstFileExW(
                    units.as_ptr(),
                    FindExInfoBasic,
                    (&mut data as *mut WIN32_FIND_DATAW).cast(),
                    FindExSearchNameMatch,
                    std::ptr::null(),
                    FIND_FIRST_EX_LARGE_FETCH,
                )
            };
            if handle == INVALID_HANDLE_VALUE {
                let e = io::Error::last_os_error();
                if matches!(e.raw_os_error(), Some(1 | 50 | 87 | 120 | 124)) {
                    return collect(path, Strategy::Reference, cancel, emit);
                }
                if e.raw_os_error() == Some(2) && fs::metadata(path)?.is_dir() {
                    return Ok(());
                }
                return Err(e);
            }
            let handle = Find(handle);
            loop {
                crate::output::check(cancel)?;
                let length = data.cFileName.iter().position(|v| *v == 0).ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidData, "파일명 종료값이 없습니다")
                })?;
                let name = &data.cFileName[..length];
                if name.is_empty() {
                    return Err(io::Error::new(io::ErrorKind::InvalidData, "빈 열거 파일명"));
                }
                if name != [46] && name != [46, 46] {
                    let name = OsString::from_wide(name);
                    push(Ok(Entry {
                        path: path.join(&name),
                        name,
                        logical_bytes: ((data.nFileSizeHigh as u64) << 32)
                            | data.nFileSizeLow as u64,
                        attributes: data.dwFileAttributes,
                        created: time(data.ftCreationTime),
                        modified: time(data.ftLastWriteTime),
                        extra: Default::default(),
                    }))?;
                }
                // 성공 시에만 갱신된 고정 구조체 참조. 종료/실제 오류 구분.
                if unsafe { FindNextFileW(handle.0, &mut data) } == 0 {
                    let e = io::Error::last_os_error();
                    if e.raw_os_error() == Some(18) {
                        break;
                    }
                    push(Err((path.to_owned(), e)))?;
                    break;
                }
            }
        }
        Strategy::Bulk | Strategy::Experimental(_) => unreachable!(),
    }
    if !batch.is_empty() {
        emit(batch)?;
    }
    Ok(())
}

/// Windows가 반환하는 페이지를 받는 OS 경계. false는 정상 열거 종료.
pub trait Source {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<bool>;
    fn volume(&self) -> u64;
}
fn unsupported(e: io::Error) -> io::Error {
    if matches!(e.raw_os_error(), Some(1 | 50 | 87 | 120 | 124)) {
        io::Error::new(io::ErrorKind::Unsupported, e)
    } else {
        e
    }
}
struct Native {
    file: fs::File,
    volume: u64,
    first: bool,
    returned_page: bool,
}
impl Native {
    fn open(path: &Path) -> io::Result<Self> {
        Self::open_observed(path, &mut |_| {})
    }
    fn open_observed(path: &Path, observe: &mut impl FnMut(fs::Metadata)) -> io::Result<Self> {
        let file = fs::File::options()
            .access_mode(1)
            .share_mode(7)
            .custom_flags(0x02200000)
            .open(path)?;
        let m = file.metadata()?;
        if !m.is_dir() || m.file_attributes() & 0x400 != 0 {
            return Err(io::Error::other("열거할 폴더의 종류가 변경되었습니다"));
        }
        observe(m);
        let mut id = FILE_ID_INFO::default();
        // 동일 디렉터리 핸들에서 64-bit 볼륨 ID 확인. 출력 구조체의 크기/수명 유지.
        if unsafe {
            GetFileInformationByHandleEx(
                file.as_raw_handle(),
                FileIdInfo,
                (&mut id as *mut FILE_ID_INFO).cast(),
                std::mem::size_of_val(&id) as u32,
            )
        } == 0
        {
            return Err(unsupported(io::Error::last_os_error()));
        }
        Ok(Self {
            file,
            volume: id.VolumeSerialNumber,
            first: true,
            returned_page: false,
        })
    }
}
impl Source for Native {
    fn volume(&self) -> u64 {
        self.volume
    }
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<bool> {
        let class = if self.first {
            FileIdExtdDirectoryRestartInfo
        } else {
            FileIdExtdDirectoryInfo
        };
        self.first = false;
        // collect_from의 8-byte 정렬 버퍼에 한정한 동기 Win32 호출. false 뒤 출력 미사용.
        if unsafe {
            GetFileInformationByHandleEx(
                self.file.as_raw_handle(),
                class,
                buffer.as_mut_ptr().cast(),
                buffer.len() as u32,
            )
        } != 0
        {
            self.returned_page = true;
            Ok(true)
        } else {
            let e = io::Error::last_os_error();
            if e.raw_os_error() == Some(18) {
                Ok(false)
            } else {
                Err(e)
            }
        }
    }
}
fn collect_native(
    path: &Path,
    source: &mut Native,
    cancel: &impl Fn() -> bool,
    emit: &mut impl FnMut(Vec<Item>) -> io::Result<()>,
    experiment: Experiment,
) -> io::Result<()> {
    let result = if !experiment.reuse_buffer && experiment.buffer_bytes == 65536 {
        collect_from(path, source, cancel, emit)
    } else {
        collect_from_with_options(path, source, cancel, emit, experiment)
    };
    match result {
        // 중간 미지원도 Unsupported로 분류될 수 있어 성공 페이지 이후 재열거 금지.
        Err(e) if e.kind() == io::ErrorKind::Unsupported && !source.returned_page => {
            collect(path, Strategy::LargeFetch, cancel, emit)
        }
        other => other,
    }
}
/// 길이·정렬·파일명을 검증한 일괄 열거. 첫 페이지 미지원만 안전한 대체 처리로 구분.
pub fn collect_from(
    path: &Path,
    source: &mut impl Source,
    cancel: &impl Fn() -> bool,
    emit: &mut impl FnMut(Vec<Item>) -> io::Result<()>,
) -> io::Result<()> {
    collect_from_with_options(path, source, cancel, emit, Experiment::default())
}
pub fn collect_from_with_options(
    path: &Path,
    source: &mut impl Source,
    cancel: &impl Fn() -> bool,
    emit: &mut impl FnMut(Vec<Item>) -> io::Result<()>,
    experiment: Experiment,
) -> io::Result<()> {
    if !matches!(experiment.buffer_bytes, 65536 | 1048576 | 4194304) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "열거 버퍼는 64 KiB/1 MiB/4 MiB만 지원합니다",
        ));
    }
    let words = experiment.buffer_bytes / 8;
    if experiment.reuse_buffer {
        PAGE_BUFFER.with(|buffer| {
            let mut storage = buffer.borrow_mut();
            storage.resize(words, 0);
            read_pages(path, source, cancel, emit, &mut storage)
        })
    } else {
        read_pages(path, source, cancel, emit, &mut vec![0u64; words])
    }
}
fn read_pages(
    path: &Path,
    source: &mut impl Source,
    cancel: &impl Fn() -> bool,
    emit: &mut impl FnMut(Vec<Item>) -> io::Result<()>,
    storage: &mut [u64],
) -> io::Result<()> {
    let mut first = true;
    loop {
        crate::output::check(cancel)?;
        // u64 저장소의 초기화된 바이트 범위만 사용하여 Win32 요구 정렬 충족.
        let bytes = unsafe {
            std::slice::from_raw_parts_mut(storage.as_mut_ptr().cast::<u8>(), storage.len() * 8)
        };
        // 유효 Byte 수가 없는 API에서 짧은 손상 응답이 이전 파일명을 읽지 않도록 초기화 유지.
        bytes.fill(0);
        let present = source
            .read(bytes)
            .map_err(|e| if first { unsupported(e) } else { e })?;
        if !present {
            return Ok(());
        }
        first = false;
        let mut at = 0usize;
        let mut batch = Vec::with_capacity(128);
        loop {
            crate::output::check(cancel)?;
            let bad = || {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Windows 폴더 열거 응답의 길이/값이 잘못되었습니다",
                )
            };
            let header = bytes.get(at..at + 88).ok_or_else(bad)?;
            let u32_at =
                |offset| u32::from_le_bytes(header[offset..offset + 4].try_into().unwrap());
            let i64_at =
                |offset| i64::from_le_bytes(header[offset..offset + 8].try_into().unwrap());
            let next = u32_at(0) as usize;
            let length = u32_at(60) as usize;
            if length == 0 || !length.is_multiple_of(2) || length > 65534 {
                return Err(bad());
            }
            let end = at.checked_add(88 + length).ok_or_else(bad)?;
            if next != 0
                && (!next.is_multiple_of(8) || next < 88 + length || at + next > bytes.len())
            {
                return Err(bad());
            }
            let name: Vec<u16> = bytes
                .get(at + 88..end)
                .ok_or_else(bad)?
                .as_chunks::<2>()
                .0
                .iter()
                .map(|s| u16::from_le_bytes([s[0], s[1]]))
                .collect();
            if name.iter().any(|v| matches!(v, 0 | 47 | 92 | 58)) {
                return Err(bad());
            }
            let size = u64::try_from(i64_at(40)).map_err(|_| bad())?;
            let allocation = u64::try_from(i64_at(48)).map_err(|_| bad())?;
            if name != [46] && name != [46, 46] {
                let name = OsString::from_wide(&name);
                let entry_path = path.join(&name);
                let attributes = u32_at(56);
                let ticks = |v: i64| {
                    time(FILETIME {
                        dwLowDateTime: v as u32,
                        dwHighDateTime: (v as u64 >> 32) as u32,
                    })
                };
                let extra = crate::metadata::Extra {
                    allocation: crate::metadata::Field::Value(allocation),
                    identity: crate::metadata::Field::Value(crate::metadata::Identity {
                        volume: source.volume(),
                        file: header[72..88].try_into().unwrap(),
                    }),
                    links: Default::default(),
                };
                let entry = Ok(Entry {
                    path: entry_path,
                    name,
                    logical_bytes: size,
                    attributes,
                    created: ticks(i64_at(8)),
                    modified: ticks(i64_at(24)),
                    extra,
                });
                batch.push(entry);
                if batch.len() == 128 {
                    emit(std::mem::replace(&mut batch, Vec::with_capacity(128)))?;
                }
            }
            if next == 0 {
                break;
            }
            at += next;
        }
        if !batch.is_empty() {
            emit(batch)?;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn configured_pages_bound_buffer_sizes_and_reject_short_dirty_responses() {
        struct Pages {
            pages: std::collections::VecDeque<Vec<u8>>,
            bytes: usize,
        }
        impl Source for Pages {
            fn read(&mut self, buffer: &mut [u8]) -> io::Result<bool> {
                assert_eq!(buffer.len(), self.bytes);
                if let Some(page) = self.pages.pop_front() {
                    buffer[..page.len()].copy_from_slice(&page);
                    Ok(true)
                } else {
                    Ok(false)
                }
            }
            fn volume(&self) -> u64 {
                17
            }
        }
        let page = |name: &[u16]| {
            let mut bytes = vec![0u8; 88 + name.len() * 2];
            bytes[40..48].copy_from_slice(&3i64.to_le_bytes());
            bytes[48..56].copy_from_slice(&4096i64.to_le_bytes());
            bytes[60..64].copy_from_slice(&((name.len() * 2) as u32).to_le_bytes());
            bytes[72] = 5;
            bytes[87] = 0x81;
            for (i, value) in name.iter().enumerate() {
                bytes[88 + i * 2..90 + i * 2].copy_from_slice(&value.to_le_bytes());
            }
            bytes
        };
        for buffer_bytes in [65536, 1048576, 4194304] {
            let options = Experiment {
                reuse_buffer: true,
                buffer_bytes,
                ..Default::default()
            };
            let mut entries = vec![];
            collect_from_with_options(
                Path::new("scope"),
                &mut Pages {
                    pages: [page(&[97, 98])].into(),
                    bytes: buffer_bytes,
                },
                &|| false,
                &mut |batch| {
                    entries.extend(batch);
                    Ok(())
                },
                options,
            )
            .unwrap();
            let entry = entries.pop().unwrap().unwrap();
            assert_eq!((entry.name, entry.logical_bytes), (OsString::from("ab"), 3));
            let id = entry.extra.identity.value().unwrap();
            assert_eq!(id.volume, 17);
            assert_eq!(id.file[15], 0x81);
            let mut short = page(&[99]);
            short[60..64].copy_from_slice(&4u32.to_le_bytes());
            assert_eq!(
                collect_from_with_options(
                    Path::new("scope"),
                    &mut Pages {
                        pages: [short].into(),
                        bytes: buffer_bytes
                    },
                    &|| false,
                    &mut |_| Ok(()),
                    options
                )
                .unwrap_err()
                .kind(),
                io::ErrorKind::InvalidData
            );
        }
        for buffer_bytes in [0, 1, 65535, usize::MAX] {
            assert_eq!(
                collect_from_with_options(
                    Path::new("scope"),
                    &mut Pages {
                        pages: Default::default(),
                        bytes: 0
                    },
                    &|| false,
                    &mut |_| Ok(()),
                    Experiment {
                        buffer_bytes,
                        ..Default::default()
                    }
                )
                .unwrap_err()
                .kind(),
                io::ErrorKind::InvalidInput
            );
        }
    }
    #[test]
    fn configured_page_failures_keep_permission_errors_and_cancellation_distinct() {
        struct Failure(i32);
        impl Source for Failure {
            fn read(&mut self, _buffer: &mut [u8]) -> io::Result<bool> {
                Err(io::Error::from_raw_os_error(self.0))
            }
            fn volume(&self) -> u64 {
                0
            }
        }
        for buffer_bytes in [65536, 1048576, 4194304] {
            let options = Experiment {
                reuse_buffer: true,
                buffer_bytes,
                ..Default::default()
            };
            for (code, kind) in [
                (5, io::ErrorKind::PermissionDenied),
                (2, io::ErrorKind::NotFound),
                (50, io::ErrorKind::Unsupported),
            ] {
                assert_eq!(
                    collect_from_with_options(
                        Path::new("scope"),
                        &mut Failure(code),
                        &|| false,
                        &mut |_| Ok(()),
                        options
                    )
                    .unwrap_err()
                    .kind(),
                    kind
                );
            }
            assert_eq!(
                collect_from_with_options(
                    Path::new("scope"),
                    &mut Failure(5),
                    &|| true,
                    &mut |_| Ok(()),
                    options
                )
                .unwrap_err()
                .kind(),
                io::ErrorKind::Interrupted
            );
        }
    }
    #[test]
    fn shared_folder_collection_publishes_current_folder_and_all_children() {
        let root =
            std::env::temp_dir().join(format!("diskweave-shared-folder-{}", std::process::id()));
        fs::create_dir_all(root.join("child")).unwrap();
        fs::write(root.join("data"), b"123").unwrap();
        for buffer_bytes in [65536, 1048576, 4194304] {
            let mut observed = None;
            let mut entries = vec![];
            collect_folder(
                &root,
                Strategy::Experimental(Experiment {
                    shared_handle: true,
                    buffer_bytes,
                    ..Default::default()
                }),
                &|| false,
                &mut |batch| {
                    entries.extend(batch);
                    Ok(())
                },
                &mut |entry| observed = Some(entry),
                None,
            )
            .unwrap();
            let folder = observed.unwrap();
            assert_eq!(folder.path, root);
            assert_eq!(
                folder.modified,
                fs::symlink_metadata(&root).unwrap().modified().ok()
            );
            assert_eq!(entries.len(), 2);
            let data = entries
                .iter()
                .map(|e| e.as_ref().unwrap())
                .find(|e| e.name == "data")
                .unwrap();
            assert_eq!(data.logical_bytes, 3);
            assert!(
                data.extra.identity.value().is_some(),
                "현재 NTFS에서 실제 일괄 수집 경로 확인"
            );
            assert!(entries
                .iter()
                .map(|e| e.as_ref().unwrap())
                .any(|e| e.name == "child" && e.attributes & 0x10 != 0));
        }
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn drive_root_metadata_has_no_file_name_and_remains_a_directory_entry() {
        let current = std::env::current_dir().unwrap();
        let root = current.ancestors().last().unwrap();
        let entry = Entry::from_metadata(root.to_owned(), fs::symlink_metadata(root).unwrap());
        assert_eq!(entry.path, root);
        assert_ne!(entry.attributes & 0x10, 0);
        assert!(entry.name.is_empty());
    }
    #[test]
    fn directory_pages_reject_bad_lengths_offsets_and_midstream_failures() {
        struct Pages {
            page: Option<Vec<u8>>,
            error: Option<i32>,
        }
        impl Source for Pages {
            fn read(&mut self, buffer: &mut [u8]) -> io::Result<bool> {
                if let Some(page) = self.page.take() {
                    buffer[..page.len()].copy_from_slice(&page);
                    Ok(true)
                } else if let Some(code) = self.error.take() {
                    Err(io::Error::from_raw_os_error(code))
                } else {
                    Ok(false)
                }
            }
            fn volume(&self) -> u64 {
                7
            }
        }
        let page = || {
            let mut b = vec![0u8; 90];
            b[40..48].copy_from_slice(&3i64.to_le_bytes());
            b[48..56].copy_from_slice(&4096i64.to_le_bytes());
            b[60..64].copy_from_slice(&2u32.to_le_bytes());
            b[72] = 9;
            b[88] = b'x';
            b
        };
        let mut entries = vec![];
        collect_from(
            Path::new("test"),
            &mut Pages {
                page: Some(page()),
                error: None,
            },
            &|| false,
            &mut |items| {
                entries.extend(items);
                Ok(())
            },
        )
        .unwrap();
        let e = entries.pop().unwrap().unwrap();
        assert_eq!(e.logical_bytes, 3);
        assert_eq!(e.extra.allocation.value(), Some(4096));
        assert_eq!(e.extra.identity.value().unwrap().volume, 7);
        let mut folder_page = page();
        folder_page[56..60].copy_from_slice(&16u32.to_le_bytes());
        let mut folders = vec![];
        collect_from(
            Path::new("not-opened"),
            &mut Pages {
                page: Some(folder_page),
                error: None,
            },
            &|| false,
            &mut |items| {
                folders.extend(items);
                Ok(())
            },
        )
        .unwrap();
        assert!(
            folders[0].is_ok(),
            "수집 경계의 페이지 해석은 경로를 다시 열지 않음"
        );
        for offset in [0, 60] {
            let mut bad = page();
            bad[offset..offset + 4].copy_from_slice(&1u32.to_le_bytes());
            assert_eq!(
                collect_from(
                    Path::new("test"),
                    &mut Pages {
                        page: Some(bad),
                        error: None
                    },
                    &|| false,
                    &mut |_| Ok(())
                )
                .unwrap_err()
                .kind(),
                io::ErrorKind::InvalidData
            );
        }
        assert_eq!(
            collect_from(
                Path::new("test"),
                &mut Pages {
                    page: None,
                    error: Some(50)
                },
                &|| false,
                &mut |_| Ok(())
            )
            .unwrap_err()
            .kind(),
            io::ErrorKind::Unsupported
        );
        assert_ne!(
            collect_from(
                Path::new("test"),
                &mut Pages {
                    page: Some(page()),
                    error: Some(50)
                },
                &|| false,
                &mut |_| Ok(())
            )
            .unwrap_err()
            .kind(),
            io::ErrorKind::Unsupported
        );
        assert_eq!(
            collect_from(
                Path::new("test"),
                &mut Pages {
                    page: Some(page()),
                    error: None
                },
                &|| true,
                &mut |_| Ok(())
            )
            .unwrap_err()
            .kind(),
            io::ErrorKind::Interrupted
        );
    }
    #[test]
    fn large_fetch_keeps_empty_unicode_and_many_directory_entries() {
        use std::os::windows::ffi::{OsStrExt, OsStringExt};
        let root = std::env::temp_dir().join(format!("diskweave-fetch-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let mut empty = Vec::new();
        collect(&root, Strategy::LargeFetch, &|| false, &mut |page| {
            empty.extend(page);
            Ok(())
        })
        .unwrap();
        assert!(empty.is_empty());
        let odd = std::ffi::OsString::from_wide(&[0xD800, b'x' as u16]);
        std::fs::write(root.join(&odd), b"123").unwrap();
        let mut expected = std::collections::HashSet::from([odd.clone()]);
        for index in 0..900 {
            let name = OsString::from(format!("한글-{index:04}"));
            std::fs::write(root.join(&name), b"x").unwrap();
            expected.insert(name);
        }
        let mut found = Vec::new();
        let mut strategies = vec![Strategy::LargeFetch, Strategy::Bulk];
        for reuse_buffer in [false, true] {
            for buffer_bytes in [65536, 1048576, 4194304] {
                strategies.push(Strategy::Experimental(Experiment {
                    reuse_buffer,
                    buffer_bytes,
                    ..Default::default()
                }));
            }
        }
        for strategy in strategies {
            found.clear();
            collect(&root, strategy, &|| false, &mut |page| {
                found.extend(page.into_iter().map(Result::unwrap));
                Ok(())
            })
            .unwrap();
            assert_eq!(found.len(), 901);
            assert_eq!(
                found
                    .iter()
                    .map(|e| e.name.clone())
                    .collect::<std::collections::HashSet<_>>(),
                expected
            );
            assert_eq!(found.iter().map(|e| e.logical_bytes).sum::<u64>(), 903);
            assert!(found.iter().any(|e| e
                .path
                .file_name()
                .unwrap()
                .encode_wide()
                .eq(odd.encode_wide())));
            if strategy == Strategy::Bulk {
                let entry = found
                    .iter()
                    .find(|e| e.path.file_name().unwrap() == odd)
                    .unwrap();
                if entry.extra.identity.value().is_some() {
                    let actual = crate::metadata::read(&entry.path);
                    assert_eq!(entry.extra.identity, actual.values.identity);
                    assert_eq!(entry.extra.allocation, actual.values.allocation);
                }
            }
        }
        assert_eq!(
            collect(&root, Strategy::LargeFetch, &|| true, &mut |_| Ok(()))
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::Interrupted
        );
        std::fs::remove_dir_all(root).unwrap();
    }
}
