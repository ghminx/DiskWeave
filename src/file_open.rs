use std::io;
use std::os::windows::fs::MetadataExt;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, TryRecvError};

#[derive(Default)]
pub enum OpenStatus {
    #[default]
    Idle,
    Running,
    Submitted,
    Failed(String),
}

#[derive(Default)]
pub struct OpenController {
    receiver: Option<Receiver<io::Result<()>>>,
    status: OpenStatus,
}

impl OpenController {
    pub fn status(&self) -> &OpenStatus {
        &self.status
    }

    pub fn start(&mut self, request: OpenRequest) -> io::Result<()> {
        self.start_with(request, windows_shell)
    }

    pub fn start_with(
        &mut self,
        request: OpenRequest,
        shell: impl FnOnce(&OpenRequest) -> io::Result<()> + Send + 'static,
    ) -> io::Result<()> {
        if self.receiver.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "이전 열기 요청을 처리 중입니다",
            ));
        }
        let (sender, receiver) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = sender.send(open_with(request, shell));
        });
        self.receiver = Some(receiver);
        self.status = OpenStatus::Running;
        Ok(())
    }

    pub fn poll(&mut self) {
        let Some(receiver) = &self.receiver else {
            return;
        };
        let result = match receiver.try_recv() {
            Ok(result) => result,
            Err(TryRecvError::Empty) => return,
            Err(TryRecvError::Disconnected) => {
                Err(io::Error::other("열기 작업이 예기치 않게 종료됐습니다"))
            }
        };
        self.receiver = None;
        self.status = match result {
            Ok(()) => OpenStatus::Submitted,
            Err(error) => OpenStatus::Failed(error.to_string()),
        };
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TargetKind {
    Folder,
    File,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OpenAction {
    OpenFolder,
    OpenFile,
    RevealFile,
}

pub struct OpenRequest {
    path: PathBuf,
    kind: TargetKind,
    action: OpenAction,
}

impl OpenRequest {
    pub fn new(path: PathBuf, kind: TargetKind, action: OpenAction) -> Self {
        Self { path, kind, action }
    }

    pub fn for_item(
        result: &crate::scanner::ScanResult,
        item: crate::explorer::Item,
        action: OpenAction,
    ) -> Self {
        match item {
            crate::explorer::Item::Folder(id) => {
                Self::new(result.folders[id].path.clone(), TargetKind::Folder, action)
            }
            crate::explorer::Item::File(id) => {
                let file = &result.files[id];
                Self::new(
                    result.folders[file.parent].path.join(&file.name),
                    TargetKind::File,
                    action,
                )
            }
        }
    }
}

pub fn open_with(
    mut request: OpenRequest,
    shell: impl FnOnce(&OpenRequest) -> io::Result<()>,
) -> io::Result<()> {
    request.path = std::path::absolute(&request.path)?;
    let metadata = std::fs::symlink_metadata(&request.path)?;
    let valid_kind = match request.kind {
        TargetKind::Folder => metadata.is_dir() && request.action == OpenAction::OpenFolder,
        TargetKind::File => {
            metadata.is_file()
                && matches!(
                    request.action,
                    OpenAction::OpenFile | OpenAction::RevealFile
                )
        }
    };
    if !valid_kind || metadata.file_attributes() & 0x400 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "대상 종류가 변경됐거나 지원하지 않는 열기 요청입니다",
        ));
    }
    shell(&request)
}

// Windows ABI와 동일한 C 필드 순서·포인터 크기 유지.
#[repr(C)]
struct ShellExecuteInfo {
    size: u32,
    mask: u32,
    window: *mut std::ffi::c_void,
    verb: *const u16,
    file: *const u16,
    parameters: *const u16,
    directory: *const u16,
    show: i32,
    instance: *mut std::ffi::c_void,
    id_list: *mut std::ffi::c_void,
    class: *const u16,
    class_key: *mut std::ffi::c_void,
    hot_key: u32,
    icon_or_monitor: *mut std::ffi::c_void,
    process: *mut std::ffi::c_void,
}

#[link(name = "shell32")]
extern "system" {
    fn ShellExecuteExW(info: *mut ShellExecuteInfo) -> i32;
    fn SHParseDisplayName(
        name: *const u16,
        context: *mut std::ffi::c_void,
        pidl: *mut *mut std::ffi::c_void,
        attributes: u32,
        result_attributes: *mut u32,
    ) -> i32;
    fn SHOpenFolderAndSelectItems(
        pidl: *const std::ffi::c_void,
        count: u32,
        items: *const *const std::ffi::c_void,
        flags: u32,
    ) -> i32;
}

#[link(name = "ole32")]
extern "system" {
    fn CoInitializeEx(reserved: *mut std::ffi::c_void, flags: u32) -> i32;
    fn CoUninitialize();
    fn CoTaskMemFree(memory: *mut std::ffi::c_void);
}

fn check_hresult(code: i32) -> io::Result<()> {
    if code < 0 {
        Err(io::Error::other(format!(
            "Windows Shell 오류: 0x{:08X}",
            code as u32
        )))
    } else {
        Ok(())
    }
}

fn windows_shell(request: &OpenRequest) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use std::ptr::{null, null_mut};
    let path: Vec<u16> = request
        .path
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect();
    // 작업 스레드의 STA 초기화 및 성공한 초기화마다 동일 스레드에서 해제.
    unsafe {
        check_hresult(CoInitializeEx(null_mut(), 0x2 | 0x4))?;
        let result = {
            if request.action == OpenAction::RevealFile {
                let mut pidl = null_mut();
                let parsed =
                    SHParseDisplayName(path.as_ptr(), null_mut(), &mut pidl, 0, null_mut());
                let result = if parsed < 0 {
                    check_hresult(parsed)
                } else {
                    // cidl=0일 때 절대 파일 PIDL의 부모 폴더를 열고 파일 선택.
                    check_hresult(SHOpenFolderAndSelectItems(pidl, 0, null(), 0))
                };
                CoTaskMemFree(pidl);
                result
            } else {
                // 숫자·raw pointer로만 구성된 ABI 구조체의 예약 필드 0 초기화.
                let mut info: ShellExecuteInfo = std::mem::zeroed();
                let verb = [b'o' as u16, b'p' as u16, b'e' as u16, b'n' as u16, 0];
                info.size = std::mem::size_of::<ShellExecuteInfo>() as u32;
                // 메시지 루프 없는 worker의 요청 완료 대기 및 자체 오류 안내 사용.
                info.mask = 0x100 | 0x400;
                info.verb = verb.as_ptr();
                info.file = path.as_ptr();
                info.show = 1;
                if ShellExecuteExW(&mut info) == 0 {
                    Err(io::Error::last_os_error())
                } else {
                    Ok(())
                }
            }
        };
        CoUninitialize();
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn controller_blocks_duplicate_requests_and_reports_shell_success_and_failure() {
        use std::sync::mpsc;
        use std::time::{Duration, Instant};
        let request = || {
            OpenRequest::new(
                std::env::temp_dir(),
                TargetKind::Folder,
                OpenAction::OpenFolder,
            )
        };
        let mut controller = OpenController::default();
        let (release, gate) = mpsc::channel();
        controller
            .start_with(request(), move |_| {
                gate.recv().unwrap();
                Ok(())
            })
            .unwrap();
        assert!(matches!(controller.status(), OpenStatus::Running));
        assert_eq!(
            controller
                .start_with(request(), |_| Ok(()))
                .unwrap_err()
                .kind(),
            io::ErrorKind::AlreadyExists
        );
        release.send(()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while matches!(controller.status(), OpenStatus::Running) && Instant::now() < deadline {
            controller.poll();
            std::thread::yield_now();
        }
        assert!(matches!(controller.status(), OpenStatus::Submitted));
        controller
            .start_with(request(), |_| Err(io::Error::other("Shell failure")))
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while matches!(controller.status(), OpenStatus::Running) && Instant::now() < deadline {
            controller.poll();
            std::thread::yield_now();
        }
        assert!(
            matches!(controller.status(), OpenStatus::Failed(message) if message.contains("Shell failure"))
        );
    }

    #[test]
    fn invalid_or_changed_targets_are_rejected_before_shell_dispatch() {
        let root = std::env::temp_dir().join(format!(
            "diskweave-open-invalid-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&root).unwrap();
        let file = root.join("file.txt");
        fs::write(&file, "sample").unwrap();
        for (path, kind, action, expected) in [
            (
                root.clone(),
                TargetKind::File,
                OpenAction::OpenFile,
                io::ErrorKind::InvalidInput,
            ),
            (
                file.clone(),
                TargetKind::Folder,
                OpenAction::OpenFolder,
                io::ErrorKind::InvalidInput,
            ),
            (
                root.clone(),
                TargetKind::Folder,
                OpenAction::RevealFile,
                io::ErrorKind::InvalidInput,
            ),
            (
                file.clone(),
                TargetKind::File,
                OpenAction::OpenFolder,
                io::ErrorKind::InvalidInput,
            ),
            (
                root.join("missing.txt"),
                TargetKind::File,
                OpenAction::OpenFile,
                io::ErrorKind::NotFound,
            ),
        ] {
            let error = open_with(OpenRequest::new(path, kind, action), |_| {
                panic!("유효하지 않은 요청의 Shell 호출")
            })
            .unwrap_err();
            assert_eq!(error.kind(), expected);
        }
        fs::remove_file(&file).unwrap();
        let error = open_with(
            OpenRequest::new(file, TargetKind::File, OpenAction::RevealFile),
            |_| panic!("삭제된 파일의 Shell 호출"),
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn opening_uses_the_validated_target_and_action() {
        let root = std::env::temp_dir().join(format!(
            "diskweave-open-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&root).unwrap();
        let file = root.join("한글 file.txt");
        fs::write(&file, "sample").unwrap();
        let scan = crate::scanner::scan(&root, || false).unwrap();
        for (path, kind, action) in [
            (&root, TargetKind::Folder, OpenAction::OpenFolder),
            (&file, TargetKind::File, OpenAction::OpenFile),
            (&file, TargetKind::File, OpenAction::RevealFile),
        ] {
            let item = match kind {
                TargetKind::Folder => crate::explorer::Item::Folder(0),
                TargetKind::File => crate::explorer::Item::File(scan.file_children[0][0]),
            };
            let request = OpenRequest::for_item(&scan, item, action);
            open_with(request, |validated| {
                assert_eq!((&validated.path, validated.action), (path, action));
                Ok(())
            })
            .unwrap();
        }
        fs::remove_dir_all(root).unwrap();
    }
}
