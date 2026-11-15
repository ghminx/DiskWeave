use crate::ntfs::{Id, Info, Source};
use serde::{Deserialize, Serialize};
use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io;
use std::os::windows::{
    ffi::{OsStrExt, OsStringExt},
    fs::{MetadataExt, OpenOptionsExt},
    io::{AsRawHandle, FromRawHandle},
};
use std::path::{Path, PathBuf};
use windows_sys::Win32::{
    Foundation::*,
    Storage::FileSystem::*,
    System::{Ioctl::*, IO::DeviceIoControl},
};

fn wide(s: impl AsRef<OsStr>) -> Vec<u16> {
    s.as_ref().encode_wide().chain(Some(0)).collect()
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Target {
    pub root: String,
    pub volume: String,
}
pub fn target(path: &Path) -> io::Result<Target> {
    let s = path.as_os_str().encode_wide().collect::<Vec<_>>();
    if s.len() != 3
        || !((65..=90).contains(&s[0]) || (97..=122).contains(&s[0]))
        || s[1] != 58
        || s[2] != 92
    {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "NTFS 고속 스캔은 로컬 드라이브 전체에서 사용할 수 있습니다",
        ));
    }
    let root = format!(
        "{}:\\",
        char::from_u32(s[0] as u32).unwrap().to_ascii_uppercase()
    );
    let w = wide(&root);
    let mut fs = [0u16; 32];
    let mut guid = [0u16; 64];
    // NUL 종료된 드라이브 루트와 고정 크기 출력 버퍼만 전달
    unsafe {
        if ![3, 2].contains(&GetDriveTypeW(w.as_ptr())) {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "로컬 고정·이동식 드라이브만 지원합니다",
            ));
        }
        if GetVolumeInformationW(
            w.as_ptr(),
            std::ptr::null_mut(),
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            fs.as_mut_ptr(),
            fs.len() as u32,
        ) == 0
        {
            return Err(io::Error::last_os_error());
        }
        if String::from_utf16_lossy(&fs[..fs.iter().position(|n| *n == 0).unwrap_or(fs.len())])
            != "NTFS"
        {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "NTFS가 아니므로 기본 스캔을 사용합니다",
            ));
        }
        if GetVolumeNameForVolumeMountPointW(w.as_ptr(), guid.as_mut_ptr(), guid.len() as u32) == 0
        {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(Target {
        root,
        volume: String::from_utf16_lossy(
            &guid[..guid.iter().position(|n| *n == 0).unwrap_or(guid.len())],
        ),
    })
}
pub struct Native {
    guid: String,
    root: PathBuf,
    volume: File,
    cursor: u64,
    done: bool,
}
impl Native {
    pub fn open(request: &Target) -> io::Result<Self> {
        let current = target(Path::new(&request.root))?;
        if current.volume != request.volume {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "선택한 드라이브가 변경됐습니다",
            ));
        }
        let device = wide(format!("\\\\.\\{}", &request.root[..2]));
        // 볼륨 핸들은 읽기 전용이며 저널 생성·변경 IOCTL을 호출하지 않음
        let raw = unsafe {
            CreateFileW(
                device.as_ptr(),
                GENERIC_READ,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                std::ptr::null(),
                OPEN_EXISTING,
                0,
                std::ptr::null_mut(),
            )
        };
        if raw == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            guid: request.volume.clone(),
            root: PathBuf::from(&request.root),
            volume: unsafe { File::from_raw_handle(raw) },
            cursor: 0,
            done: false,
        })
    }
}
impl Source for Native {
    fn root_id(&mut self) -> io::Result<Id> {
        let f = File::options()
            .access_mode(FILE_READ_ATTRIBUTES)
            .share_mode(7)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
            .open(&self.root)?;
        let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
        // 실제 루트 핸들의 64비트 참조 사용. segment 번호와 sequence의 임의 변환 방지
        if unsafe { GetFileInformationByHandle(f.as_raw_handle(), &mut info) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Id::V2(
            (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow),
        ))
    }
    fn page(&mut self) -> io::Result<Option<Vec<u8>>> {
        if self.done {
            return Ok(None);
        }
        let request = MFT_ENUM_DATA_V0 {
            StartFileReferenceNumber: self.cursor,
            LowUsn: 0,
            HighUsn: i64::MAX,
        };
        let mut data = vec![0u8; 1024 * 1024];
        let mut bytes = 0;
        let ok = unsafe {
            DeviceIoControl(
                self.volume.as_raw_handle(),
                FSCTL_ENUM_USN_DATA,
                (&request as *const MFT_ENUM_DATA_V0).cast(),
                std::mem::size_of_val(&request) as u32,
                data.as_mut_ptr().cast(),
                data.len() as u32,
                &mut bytes,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            let e = io::Error::last_os_error();
            if e.raw_os_error() == Some(ERROR_HANDLE_EOF as i32) {
                self.done = true;
                return Ok(None);
            }
            return Err(e);
        }
        data.truncate(bytes as usize);
        if data.len() < 8 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "NTFS 열거 응답이 잘렸습니다",
            ));
        }
        let next = u64::from_le_bytes(data[..8].try_into().unwrap());
        if next <= self.cursor {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "NTFS 열거 위치가 진행하지 않습니다",
            ));
        }
        self.cursor = next;
        Ok(Some(data))
    }
    fn inspect(&mut self, path: &Path, detailed: bool) -> io::Result<Info> {
        inspect_file(path, detailed, &self.guid)
    }
    fn links(&mut self, path: &Path) -> io::Result<Vec<PathBuf>> {
        let name = wide(path);
        let mut size = 32768u32;
        let mut buffer = vec![0u16; size as usize];
        let handle =
            unsafe { FindFirstFileNameW(name.as_ptr(), 0, &mut size, buffer.as_mut_ptr()) };
        if handle == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        struct Find(HANDLE);
        impl Drop for Find {
            fn drop(&mut self) {
                unsafe {
                    FindClose(self.0);
                }
            }
        }
        let _find = Find(handle);
        let mut links = Vec::new();
        loop {
            let len = buffer.iter().position(|n| *n == 0).ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "Hard Link 경로가 잘렸습니다")
            })?;
            if len < 2 || buffer[0] != 92 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "잘못된 볼륨 상대 경로",
                ));
            }
            links.push(self.root.join(OsString::from_wide(&buffer[1..len])));
            size = buffer.len() as u32;
            if unsafe { FindNextFileNameW(handle, &mut size, buffer.as_mut_ptr()) } == 0 {
                let e = io::Error::last_os_error();
                if e.raw_os_error() == Some(ERROR_HANDLE_EOF as i32) {
                    break;
                }
                return Err(e);
            }
        }
        Ok(links)
    }
}

/// 선택한 볼륨의 핸들에서만 메타데이터 조회.
pub fn inspect_file(path: &Path, detailed: bool, expected_volume: &str) -> io::Result<Info> {
    let f = File::options()
        .access_mode(FILE_READ_ATTRIBUTES)
        .share_mode(7)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?;
    let resolved = check_volume(&f, expected_volume)?;
    let metadata = f.metadata()?;
    if metadata.is_dir() && metadata.file_attributes() & 0x400 == 0 {
        let _access = File::options()
            .access_mode(FILE_LIST_DIRECTORY)
            .share_mode(7)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
            .open(path)?;
    }
    let mut id: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    if unsafe { GetFileInformationByHandle(f.as_raw_handle(), &mut id) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(Info {
        resolved: Some(resolved),
        link_count: Some(id.nNumberOfLinks),
        reference: Some(Id::V2(
            (u64::from(id.nFileIndexHigh) << 32) | u64::from(id.nFileIndexLow),
        )),
        bytes: metadata.len(),
        attributes: metadata.file_attributes(),
        created: metadata.created().ok(),
        modified: metadata.modified().ok(),
        extra: if detailed && metadata.is_file() {
            crate::metadata::read_file(&f)
        } else {
            crate::metadata::Report {
                values: Default::default(),
                issue: None,
            }
        },
    })
}
pub(crate) fn check_volume(file: &File, expected: &str) -> io::Result<PathBuf> {
    let mut buffer = vec![0u16; 256];
    let query = |buffer: &mut Vec<u16>| unsafe {
        GetFinalPathNameByHandleW(
            file.as_raw_handle(),
            buffer.as_mut_ptr(),
            buffer.len() as u32,
            VOLUME_NAME_GUID,
        )
    };
    let mut size = query(&mut buffer) as usize;
    if size >= buffer.len() && size <= 32768 {
        buffer.resize(size + 1, 0);
        size = query(&mut buffer) as usize;
    }
    if size == 0 {
        return Err(io::Error::last_os_error());
    }
    if size >= buffer.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "볼륨 경로 길이 초과",
        ));
    }
    let close = buffer[..size]
        .iter()
        .position(|n| *n == 125)
        .filter(|n| *n + 1 < size && buffer[*n + 1] == 92)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "볼륨 GUID 경로 확인 실패"))?;
    if !String::from_utf16_lossy(&buffer[..close + 2]).eq_ignore_ascii_case(expected) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "선택한 볼륨 밖으로 경로가 변경됐습니다",
        ));
    }
    Ok(PathBuf::from(OsString::from_wide(&buffer[..size])))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn metadata_handles_are_bound_to_the_selected_volume() {
        let root =
            std::env::temp_dir().join(format!("diskweave-volume-boundary-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let file = root.join("local.bin");
        std::fs::write(&file, b"1234567890123").unwrap();
        let volume = target(root.ancestors().last().unwrap()).unwrap().volume;
        let info = inspect_file(&file, true, &volume).unwrap();
        assert_eq!(info.bytes, 13);
        assert!(info.reference.is_some());
        assert_eq!(
            inspect_file(
                &file,
                true,
                "\\\\?\\Volume{00000000-0000-0000-0000-000000000000}\\"
            )
            .err()
            .unwrap()
            .kind(),
            io::ErrorKind::InvalidData
        );
        std::fs::remove_dir_all(root).unwrap();
    }
}
