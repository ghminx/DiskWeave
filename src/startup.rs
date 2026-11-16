use std::{
    io,
    path::{Path, PathBuf},
    sync::mpsc,
};
#[derive(Clone, Copy, Default, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Preferences {
    pub always_admin: bool,
}
impl Preferences {
    pub fn load(path: &Path) -> io::Result<Self> {
        match std::fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(crate::output::json_error),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e),
        }
    }
    pub fn save(&self, path: &Path) -> io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        crate::output::write(
            path,
            true,
            || false,
            |writer| serde_json::to_writer(writer, self).map_err(crate::output::json_error),
        )
    }
}
#[derive(Default)]
pub enum Status {
    #[default]
    Prompt,
    Authorizing,
    Ready,
    Basic,
    Failed(String),
}
#[derive(Default)]
pub struct Controller {
    pub preferences: Preferences,
    pub status: Status,
    pub elevated: bool,
    pub exit_requested: bool,
    attempted_relaunch: bool,
    receiver: Option<mpsc::Receiver<(bool, io::Result<()>)>>,
}
impl Controller {
    pub fn new(elevated: bool, preferences: Preferences) -> Self {
        Self {
            elevated,
            preferences,
            status: if elevated {
                Status::Ready
            } else {
                Status::Prompt
            },
            ..Self::default()
        }
    }
    pub fn needs_relaunch(&self) -> bool {
        self.preferences.always_admin && !self.elevated && !self.attempted_relaunch && !self.busy()
    }
    pub fn busy(&self) -> bool {
        self.receiver.is_some()
    }
    pub fn suppress_relaunch(&mut self) {
        self.attempted_relaunch = true;
    }
    pub fn skip(&mut self) {
        if !self.busy() {
            self.status = Status::Basic;
        }
    }
    pub fn request(
        &mut self,
        relaunch: bool,
        operation: impl FnOnce() -> io::Result<()> + Send + 'static,
    ) -> io::Result<()> {
        if self.busy() {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "권한 요청이 진행 중입니다",
            ));
        }
        self.attempted_relaunch |= relaunch;
        let (sender, receiver) = mpsc::channel();
        self.receiver = Some(receiver);
        self.status = Status::Authorizing;
        std::thread::spawn(move || {
            let _ = sender.send((relaunch, operation()));
        });
        Ok(())
    }
    pub fn poll(&mut self) {
        let Some(receiver) = &self.receiver else {
            return;
        };
        let reply = match receiver.try_recv() {
            Ok(reply) => reply,
            Err(mpsc::TryRecvError::Empty) => return,
            Err(mpsc::TryRecvError::Disconnected) => (
                false,
                Err(io::Error::other("권한 요청 작업이 중단됐습니다")),
            ),
        };
        self.receiver = None;
        match reply {
            (relaunch, Ok(())) => {
                self.exit_requested = relaunch;
                self.status = Status::Ready;
            }
            (_, Err(e)) => {
                self.status = Status::Failed(format!(
                    "권한 승인을 받지 못했습니다. 기본 스캔을 사용할 수 있습니다. {e}"
                ))
            }
        }
    }
}
pub fn preference_path() -> io::Result<PathBuf> {
    crate::index_job::default_path().map(|p| p.with_file_name("startup.json"))
}
pub fn is_elevated() -> io::Result<bool> {
    use windows_sys::Win32::{Foundation::*, Security::*, System::Threading::*};
    let mut token = std::ptr::null_mut();
    // 현재 프로세스의 토큰만 읽고 사용 후 핸들 해제.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let mut value = TOKEN_ELEVATION::default();
    let mut bytes = 0;
    let ok = unsafe {
        GetTokenInformation(
            token,
            TokenElevation,
            (&mut value as *mut TOKEN_ELEVATION).cast(),
            std::mem::size_of_val(&value) as u32,
            &mut bytes,
        )
    };
    unsafe {
        CloseHandle(token);
    }
    if ok == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(value.TokenIsElevated != 0)
    }
}
pub fn relaunch() -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::System::Com::*;
    use windows_sys::Win32::UI::Shell::*;
    // 권한 요청 작업 스레드의 COM 수명을 Shell 호출에 한정.
    let initialized = unsafe {
        CoInitializeEx(
            std::ptr::null(),
            COINIT_APARTMENTTHREADED as u32 | COINIT_DISABLE_OLE1DDE as u32,
        )
    };
    if initialized < 0 {
        return Err(io::Error::other(format!(
            "권한 요청 초기화 실패: {initialized:#x}"
        )));
    }
    struct Com;
    impl Drop for Com {
        fn drop(&mut self) {
            unsafe {
                CoUninitialize();
            }
        }
    }
    let _com = Com;
    let exe: Vec<_> = std::env::current_exe()?
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect();
    let verb: Vec<_> = "runas".encode_utf16().chain(Some(0)).collect();
    let args: Vec<_> = "--gui-elevated".encode_utf16().chain(Some(0)).collect();
    let mut info: SHELLEXECUTEINFOW = unsafe { std::mem::zeroed() };
    info.cbSize = std::mem::size_of_val(&info) as u32;
    info.fMask = SEE_MASK_NOASYNC | SEE_MASK_FLAG_NO_UI;
    info.lpFile = exe.as_ptr();
    info.lpVerb = verb.as_ptr();
    info.lpParameters = args.as_ptr();
    info.nShow = 1;
    // 고정된 자체 GUI 실행 인자만 승격하며 사용자 경로·명령을 포함하지 않음.
    if unsafe { ShellExecuteExW(&mut info) } == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn saved_admin_preference_applies_on_next_start_and_a_failed_relaunch_keeps_the_app_open() {
        let root = std::env::temp_dir().join(format!("diskweave-startup-{}", std::process::id()));
        let path = root.join("startup.json");
        assert!(!Preferences::load(&path).unwrap().always_admin);
        Preferences { always_admin: true }.save(&path).unwrap();
        let mut controller = Controller::new(false, Preferences::load(&path).unwrap());
        assert!(controller.needs_relaunch());
        controller
            .request(true, || Err(io::Error::other("시험용 재실행 실패")))
            .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while controller.busy() && std::time::Instant::now() < deadline {
            controller.poll();
            std::thread::yield_now();
        }
        assert!(matches!(controller.status, Status::Failed(_)));
        assert!(!controller.needs_relaunch());
        assert!(!controller.exit_requested);
        Preferences::default().save(&path).unwrap();
        assert!(!Preferences::load(&path).unwrap().always_admin);
        std::fs::write(&path, b"invalid").unwrap();
        assert!(Preferences::load(&path).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn startup_permission_can_be_skipped_denied_and_retried_without_duplicate_requests() {
        let mut controller = Controller::new(false, Preferences::default());
        assert!(matches!(controller.status, Status::Prompt));
        controller.skip();
        assert!(matches!(controller.status, Status::Basic));
        controller
            .request(false, || Err(std::io::Error::from_raw_os_error(1223)))
            .unwrap();
        assert!(controller.request(false, || Ok(())).is_err());
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while controller.busy() && std::time::Instant::now() < deadline {
            controller.poll();
            std::thread::yield_now();
        }
        assert!(matches!(controller.status, Status::Failed(_)));
        assert!(!controller.exit_requested);
        controller.request(false, || Ok(())).unwrap();
        while controller.busy() && std::time::Instant::now() < deadline {
            controller.poll();
            std::thread::yield_now();
        }
        assert!(matches!(controller.status, Status::Ready));
        let elevated = Controller::new(true, Preferences { always_admin: true });
        assert!(matches!(elevated.status, Status::Ready));
        assert!(!elevated.needs_relaunch());
    }
}
