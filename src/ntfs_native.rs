use crate::ntfs::{Id, Info};
use serde::{Deserialize, Serialize};
use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io;
use std::os::windows::{
    ffi::{OsStrExt, OsStringExt},
    fs::{MetadataExt, OpenOptionsExt},
    io::AsRawHandle,
};
use std::path::{Path, PathBuf};
use windows_sys::Win32::Storage::FileSystem::*;

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
    let buffer = resolved_units(file, VOLUME_NAME_GUID)?;
    let close = buffer
        .iter()
        .position(|n| *n == 125)
        .filter(|n| *n + 1 < buffer.len() && buffer[*n + 1] == 92)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "볼륨 GUID 경로 확인 실패"))?;
    if !String::from_utf16_lossy(&buffer[..close + 2]).eq_ignore_ascii_case(expected) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "선택한 볼륨 밖으로 경로가 변경됐습니다",
        ));
    }
    Ok(PathBuf::from(OsString::from_wide(&buffer)))
}

pub(crate) fn resolved_path(file: &File, format: u32) -> io::Result<PathBuf> {
    Ok(PathBuf::from(OsString::from_wide(&resolved_units(
        file, format,
    )?)))
}

fn resolved_units(file: &File, format: u32) -> io::Result<Vec<u16>> {
    let mut buffer = vec![0u16; 256];
    let query = |buffer: &mut Vec<u16>| unsafe {
        GetFinalPathNameByHandleW(
            file.as_raw_handle(),
            buffer.as_mut_ptr(),
            buffer.len() as u32,
            format,
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
    buffer.truncate(size);
    Ok(buffer)
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
