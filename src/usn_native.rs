use crate::usn::{Event, Journal, Source};
use std::{
    fs::{self, File},
    io,
    os::windows::{
        ffi::OsStrExt,
        io::{AsRawHandle, FromRawHandle},
    },
    path::{Path, PathBuf},
};
use windows_sys::Win32::{
    Foundation::*,
    Security::*,
    Storage::FileSystem::*,
    System::Threading::{GetCurrentProcess, OpenProcessToken},
    System::{Ioctl::*, IO::DeviceIoControl},
};
fn wide(path: impl AsRef<std::ffi::OsStr>) -> Vec<u16> {
    path.as_ref().encode_wide().chain(Some(0)).collect()
}
pub fn target(root: &Path) -> io::Result<crate::ntfs_native::Target> {
    let p = wide(root);
    let mut mount = vec![0u16; 32768];
    if unsafe { GetVolumePathNameW(p.as_ptr(), mount.as_mut_ptr(), mount.len() as u32) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let len = mount.iter().position(|n| *n == 0).unwrap();
    use std::os::windows::ffi::OsStringExt;
    crate::ntfs_native::target(Path::new(&std::ffi::OsString::from_wide(&mount[..len])))
}
pub struct Native {
    _backup: Backup,
    root: PathBuf,
    resolved: PathBuf,
    volume: String,
    device: File,
    journal: u64,
    ancestors: Vec<u64>,
}
impl Native {
    pub fn open(root: &Path) -> io::Result<Self> {
        let backup = Backup::enable()?;
        let root = std::path::absolute(root)?;
        let t = target(&root)?;
        let info = crate::ntfs_native::inspect_file(&root, false, &t.volume)?;
        if info.attributes & 0x410 != 0x10 {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "일반 NTFS 폴더만 변경 추적할 수 있습니다",
            ));
        }
        // 최종 경로가 원래대로 돌아와도 상위 폴더 이동 기록을 놓치지 않도록 ID 고정.
        let mut ancestors = Vec::new();
        for parent in root.ancestors().skip(1) {
            if !crate::locations::contains_path(Path::new(&t.root), parent)? {
                break;
            }
            let parent = crate::ntfs_native::inspect_file(parent, false, &t.volume)?;
            let Some(crate::ntfs::Id::V2(id)) = parent.reference else {
                return Err(io::Error::other(
                    "추적 경로의 상위 폴더 ID를 확인할 수 없습니다",
                ));
            };
            ancestors.push(id);
        }
        let device = wide(t.volume.trim_end_matches('\\'));
        // 볼륨 GUID에 읽기 권한만 요청하며 저널 생성·변경 작업은 미포함.
        let raw = unsafe {
            CreateFileW(
                device.as_ptr(),
                GENERIC_READ,
                7,
                std::ptr::null(),
                OPEN_EXISTING,
                0,
                std::ptr::null_mut(),
            )
        };
        if raw == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        let mut native = Self {
            _backup: backup,
            root,
            resolved: info.resolved.unwrap(),
            volume: t.volume,
            device: unsafe { File::from_raw_handle(raw) },
            journal: 0,
            ancestors,
        };
        native.journal()?;
        Ok(native)
    }
}
impl Source for Native {
    fn journal(&mut self) -> io::Result<Journal> {
        if target(&self.root)?.volume != self.volume {
            return Err(io::Error::other("추적 드라이브가 교체되었습니다"));
        }
        let mut data: USN_JOURNAL_DATA_V0 = unsafe { std::mem::zeroed() };
        let mut bytes = 0;
        if unsafe {
            DeviceIoControl(
                self.device.as_raw_handle(),
                FSCTL_QUERY_USN_JOURNAL,
                std::ptr::null(),
                0,
                (&mut data as *mut USN_JOURNAL_DATA_V0).cast(),
                std::mem::size_of_val(&data) as u32,
                &mut bytes,
                std::ptr::null_mut(),
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        if bytes < std::mem::size_of_val(&data) as u32 {
            return Err(io::Error::other("잘린 변경 기록 정보"));
        }
        if self.journal != 0 && self.journal != data.UsnJournalID {
            return Err(io::Error::other(
                "변경 기록이 재생성되었습니다. 다시 스캔해 주세요",
            ));
        }
        self.journal = data.UsnJournalID;
        Ok(Journal {
            volume: self.volume.clone(),
            id: data.UsnJournalID,
            first: data.FirstUsn,
            lowest: data.LowestValidUsn,
            next: data.NextUsn,
        })
    }
    fn read(&mut self, start: i64, end: i64) -> io::Result<Vec<Event>> {
        let mut events = Vec::new();
        let mut cursor = start;
        while cursor < end {
            let input = READ_USN_JOURNAL_DATA_V0 {
                StartUsn: cursor,
                ReasonMask: u32::MAX,
                ReturnOnlyOnClose: 0,
                Timeout: 0,
                BytesToWaitFor: 0,
                UsnJournalID: self.journal,
            };
            let mut page = vec![0u8; 1024 * 1024];
            let mut bytes = 0;
            if unsafe {
                DeviceIoControl(
                    self.device.as_raw_handle(),
                    FSCTL_READ_USN_JOURNAL,
                    (&input as *const READ_USN_JOURNAL_DATA_V0).cast(),
                    std::mem::size_of_val(&input) as u32,
                    page.as_mut_ptr().cast(),
                    page.len() as u32,
                    &mut bytes,
                    std::ptr::null_mut(),
                )
            } == 0
            {
                return Err(io::Error::last_os_error());
            }
            page.truncate(bytes as usize);
            let (next, found) = crate::usn::decode(&page, cursor)?;
            if found.iter().any(|event| {
                event.usn < end && event.reason & 0x3200 != 0 && self.ancestors.contains(&event.id)
            }) {
                return Err(io::Error::other(
                    "추적 경로의 상위 폴더가 변경되었습니다. 폴더를 다시 지정해 주세요",
                ));
            }
            if next <= cursor {
                return Err(io::Error::other("변경 기록을 끝까지 읽지 못했습니다"));
            }
            events.extend(found.into_iter().filter(|e| e.usn < end));
            if events.len() > 200_000 {
                return Err(io::Error::other(
                    "대기 중인 변경이 너무 많습니다. 다시 스캔해 주세요",
                ));
            }
            cursor = next.min(end);
        }
        Ok(events)
    }
    fn inspect(&mut self, path: &Path, detailed: bool) -> io::Result<crate::ntfs::Info> {
        if !crate::locations::contains_path(&self.root, path)? {
            return Err(io::Error::other("요청한 경로가 추적 폴더 밖입니다"));
        }
        let info = crate::ntfs_native::inspect_file(path, detailed, &self.volume)?;
        if !crate::locations::contains_path(&self.resolved, info.resolved.as_ref().unwrap())? {
            return Err(io::Error::other(
                "파일 핸들이 추적 폴더 밖으로 변경되었습니다",
            ));
        }
        Ok(info)
    }
    fn list(&mut self, path: &Path) -> io::Result<Vec<PathBuf>> {
        let info = self.inspect(path, false)?;
        if info.attributes & 0x410 != 0x10 {
            return Err(io::Error::other(
                "수집 중 폴더가 재분석 지점으로 변경되었습니다",
            ));
        }
        match fs::read_dir(path) {
            Ok(entries) => entries.map(|e| e.map(|e| e.path())).collect(),
            Err(error) if error.kind() == io::ErrorKind::PermissionDenied => self.backup_list(path),
            Err(error) => Err(error),
        }
    }
}
struct Backup {
    token: HANDLE,
    previous: TOKEN_PRIVILEGES,
}
impl Backup {
    fn enable() -> io::Result<Self> {
        let mut token = std::ptr::null_mut();
        if unsafe {
            OpenProcessToken(
                GetCurrentProcess(),
                TOKEN_QUERY | TOKEN_ADJUST_PRIVILEGES,
                &mut token,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        let mut old: TOKEN_PRIVILEGES = unsafe { std::mem::zeroed() };
        let mut privilege: LUID = unsafe { std::mem::zeroed() };
        let name = wide("SeBackupPrivilege");
        if unsafe { LookupPrivilegeValueW(std::ptr::null(), name.as_ptr(), &mut privilege) } == 0 {
            let error = io::Error::last_os_error();
            unsafe {
                CloseHandle(token);
            }
            return Err(error);
        }
        let new = TOKEN_PRIVILEGES {
            PrivilegeCount: 1,
            Privileges: [LUID_AND_ATTRIBUTES {
                Luid: privilege,
                Attributes: SE_PRIVILEGE_ENABLED,
            }],
        };
        let mut size = 0;
        // 승인된 수집 프로세스의 읽기용 backup intent. 원래 토큰 상태는 Drop에서 복구.
        if unsafe {
            AdjustTokenPrivileges(
                token,
                0,
                &new,
                std::mem::size_of_val(&old) as u32,
                &mut old,
                &mut size,
            )
        } == 0
        {
            let error = io::Error::last_os_error();
            unsafe {
                CloseHandle(token);
            }
            return Err(error);
        }
        Ok(Self {
            token,
            previous: old,
        })
    }
}
impl Drop for Backup {
    fn drop(&mut self) {
        unsafe {
            AdjustTokenPrivileges(
                self.token,
                0,
                &self.previous,
                0,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            );
            CloseHandle(self.token);
        }
    }
}
impl Native {
    fn backup_list(&self, path: &Path) -> io::Result<Vec<PathBuf>> {
        use std::os::windows::fs::OpenOptionsExt;
        let file = File::options()
            .access_mode(FILE_LIST_DIRECTORY)
            .share_mode(7)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
            .open(path)?;
        let resolved = crate::ntfs_native::check_volume(&file, &self.volume)?;
        if !crate::locations::contains_path(&self.resolved, &resolved)? {
            return Err(io::Error::other("폴더 핸들이 추적 범위 밖입니다"));
        }
        let mut result = Vec::new();
        let mut first = true;
        let mut buffer = vec![0u64; 4096];
        loop {
            buffer.fill(0);
            let class = if first {
                FileIdBothDirectoryRestartInfo
            } else {
                FileIdBothDirectoryInfo
            };
            first = false;
            if unsafe {
                GetFileInformationByHandleEx(
                    file.as_raw_handle(),
                    class,
                    buffer.as_mut_ptr().cast(),
                    (buffer.len() * 8) as u32,
                )
            } == 0
            {
                let error = io::Error::last_os_error();
                if error.raw_os_error() == Some(ERROR_NO_MORE_FILES as i32) {
                    break;
                }
                return Err(error);
            }
            let bytes = unsafe {
                std::slice::from_raw_parts(buffer.as_ptr().cast::<u8>(), buffer.len() * 8)
            };
            let mut offset = 0;
            loop {
                if offset + std::mem::size_of::<FILE_ID_BOTH_DIR_INFO>() > bytes.len() {
                    return Err(io::Error::other("잘린 폴더 열거 정보"));
                }
                // 8바이트 정렬 버퍼와 검증한 레코드 경계 안에서 고정 헤더만 참조.
                let entry = unsafe { &*bytes.as_ptr().add(offset).cast::<FILE_ID_BOTH_DIR_INFO>() };
                let head = std::mem::offset_of!(FILE_ID_BOTH_DIR_INFO, FileName);
                let len = entry.FileNameLength as usize;
                if len == 0 || !len.is_multiple_of(2) || offset + head + len > bytes.len() {
                    return Err(io::Error::other("폴더 이름 범위 오류"));
                }
                let name = unsafe {
                    std::slice::from_raw_parts(
                        bytes.as_ptr().add(offset + head).cast::<u16>(),
                        len / 2,
                    )
                };
                if name != [46] && name != [46, 46] {
                    if name.iter().any(|n| [0, 47, 58, 92].contains(n)) {
                        return Err(io::Error::other("폴더 이름 형식 오류"));
                    }
                    use std::os::windows::ffi::OsStringExt;
                    result.push(path.join(std::ffi::OsString::from_wide(name)));
                }
                let step = entry.NextEntryOffset as usize;
                if step == 0 {
                    break;
                }
                if step < head + len || !step.is_multiple_of(8) || offset + step >= bytes.len() {
                    return Err(io::Error::other("폴더 열거 레코드 길이 오류"));
                }
                offset += step;
            }
        }
        Ok(result)
    }
}
