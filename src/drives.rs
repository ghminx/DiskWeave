use std::io;
use std::os::windows::ffi::OsStringExt;
use std::path::PathBuf;
use windows_sys::Win32::Storage::FileSystem::{
    GetDiskFreeSpaceExW, GetDriveTypeW, GetLogicalDriveStringsW,
};

pub struct Drive {
    pub path: PathBuf,
    pub available: Option<u64>,
}

pub fn read() -> io::Result<Vec<Drive>> {
    // Windows 다중 문자열의 필요 크기를 조회한 뒤 출력 버퍼 확보.
    let needed = unsafe { GetLogicalDriveStringsW(0, std::ptr::null_mut()) };
    if needed == 0 {
        return Err(io::Error::last_os_error());
    }
    let mut buffer = vec![0u16; needed as usize + 1];
    let written = unsafe { GetLogicalDriveStringsW(buffer.len() as u32, buffer.as_mut_ptr()) };
    if written == 0 {
        return Err(io::Error::last_os_error());
    }
    if written as usize >= buffer.len() {
        return Err(io::Error::other(
            "드라이브 목록이 변경됐습니다. 새로고침하세요",
        ));
    }
    Ok(local_drives(
        &buffer[..written as usize],
        |root| unsafe { GetDriveTypeW(root.as_ptr()) },
        |root| {
            let mut available = 0;
            let success = unsafe {
                GetDiskFreeSpaceExW(
                    root.as_ptr(),
                    &mut available,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                )
            };
            (success != 0).then_some(available)
        },
    ))
}

fn local_drives(
    roots: &[u16],
    drive_type: impl Fn(&[u16]) -> u32,
    available: impl Fn(&[u16]) -> Option<u64>,
) -> Vec<Drive> {
    let mut drives = Vec::new();
    for root in roots
        .split(|&unit| unit == 0)
        .filter(|root| !root.is_empty())
    {
        let terminated: Vec<u16> = root.iter().copied().chain(Some(0)).collect();
        // 고정식·이동식 드라이브만 표시하며 다른 드라이브는 용량 조회도 생략.
        if matches!(drive_type(&terminated), 2 | 3) {
            drives.push(Drive {
                path: std::ffi::OsString::from_wide(root).into(),
                available: available(&terminated),
            });
        }
    }
    drives
}

#[derive(Default)]
pub struct DriveList {
    pub entries: Vec<Drive>,
    pub error: Option<String>,
    pub started: bool,
    receiver: Option<std::sync::mpsc::Receiver<io::Result<Vec<Drive>>>>,
}
impl DriveList {
    pub fn refresh(&mut self) {
        if self.receiver.is_some() {
            return;
        }
        self.started = true;
        self.error = None;
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = sender.send(read());
        });
        self.receiver = Some(receiver);
    }
    pub fn poll(&mut self) {
        let Some(receiver) = &self.receiver else {
            return;
        };
        let result = match receiver.try_recv() {
            Ok(result) => result,
            Err(std::sync::mpsc::TryRecvError::Empty) => return,
            Err(_) => Err(io::Error::other("드라이브 조회 작업이 종료됐습니다")),
        };
        self.receiver = None;
        match result {
            Ok(entries) => self.entries = entries,
            Err(error) => self.error = Some(error.to_string()),
        }
    }
    pub fn running(&self) -> bool {
        self.receiver.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::windows::ffi::OsStrExt;

    #[test]
    #[ignore = "실제 Windows 드라이브 목록 읽기 전용 점검"]
    fn real_drive_inventory_read_only() {
        let drives = read().unwrap();
        for drive in &drives {
            let root: Vec<_> = drive
                .path
                .as_os_str()
                .encode_wide()
                .chain(Some(0))
                .collect();
            assert!(matches!(unsafe { GetDriveTypeW(root.as_ptr()) }, 2 | 3));
        }
        eprintln!(
            "DRIVES local={} ready={}",
            drives.len(),
            drives
                .iter()
                .filter(|drive| drive.available.is_some())
                .count()
        );
    }
    #[test]
    fn local_drive_probe_preserves_unknown_capacity_and_excludes_networks() {
        let roots: Vec<_> = "C:\\\0E:\\\0Z:\\\0\0".encode_utf16().collect();
        let drives = local_drives(
            &roots,
            |root| {
                assert_eq!(root.last(), Some(&0));
                match char::from_u32(root[0].into()) {
                    Some('C') => 3,
                    Some('E') => 2,
                    Some('Z') => 4,
                    _ => unreachable!(),
                }
            },
            |root| match char::from_u32(root[0].into()) {
                Some('C') => Some(42),
                Some('E') => None,
                _ => panic!("지원하지 않는 드라이브의 여유 공간 조회"),
            },
        );
        assert_eq!(drives.len(), 2);
        assert_eq!(drives[0].path, PathBuf::from(r"C:\"));
        assert_eq!(drives[0].available, Some(42));
        assert_eq!(drives[1].path, PathBuf::from(r"E:\"));
        assert_eq!(drives[1].available, None);
    }
}
