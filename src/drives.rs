use std::io;
use std::os::windows::ffi::OsStringExt;
use std::path::PathBuf;

pub struct Drive {
    pub path: PathBuf,
    pub kind: u32,
    pub available: Option<u64>,
}

pub fn read_with(probe: impl FnOnce() -> io::Result<Vec<Drive>>) -> io::Result<Vec<Drive>> {
    Ok(probe()?
        .into_iter()
        .filter(|drive| matches!(drive.kind, 2 | 3))
        .collect())
}

#[link(name = "kernel32")]
extern "system" {
    fn GetLogicalDriveStringsW(length: u32, buffer: *mut u16) -> u32;
    fn GetDriveTypeW(root: *const u16) -> u32;
    fn GetDiskFreeSpaceExW(
        root: *const u16,
        available: *mut u64,
        total: *mut u64,
        free: *mut u64,
    ) -> i32;
}

pub fn read() -> io::Result<Vec<Drive>> {
    read_with(|| {
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
        let mut result = Vec::new();
        for root in buffer[..written as usize]
            .split(|&c| c == 0)
            .filter(|part| !part.is_empty())
        {
            let terminated: Vec<u16> = root.iter().copied().chain(Some(0)).collect();
            // NUL 종료 UTF-16 루트와 64비트 출력 버퍼를 호출 동안 유지.
            let kind = unsafe { GetDriveTypeW(terminated.as_ptr()) };
            if !matches!(kind, 2 | 3) {
                continue;
            }
            let mut available = 0;
            let success = unsafe {
                GetDiskFreeSpaceExW(
                    terminated.as_ptr(),
                    &mut available,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                )
            };
            result.push(Drive {
                path: std::ffi::OsString::from_wide(root).into(),
                kind,
                available: (success != 0).then_some(available),
            });
        }
        Ok(result)
    })
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
    #[test]
    #[ignore = "실제 Windows 드라이브 목록 읽기 전용 점검"]
    fn real_drive_inventory_read_only() {
        let drives = read().unwrap();
        assert!(drives.iter().all(|drive| matches!(drive.kind, 2 | 3)));
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
        let drives = read_with(|| {
            Ok(vec![
                Drive {
                    path: "C:\\".into(),
                    kind: 3,
                    available: Some(42),
                },
                Drive {
                    path: "E:\\".into(),
                    kind: 2,
                    available: None,
                },
                Drive {
                    path: "Z:\\".into(),
                    kind: 4,
                    available: Some(99),
                },
            ])
        })
        .unwrap();
        assert_eq!(drives.len(), 2);
        assert_eq!(drives[1].available, None);
        assert!(read_with(|| Err(std::io::Error::other("probe failed"))).is_err());
    }
}
