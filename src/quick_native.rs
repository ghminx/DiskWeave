use crate::quick_settings::{Preferences, Shortcut};
use std::{io, sync::mpsc};
pub trait Bindings {
    fn register(&mut self, id: i32, key: Shortcut) -> io::Result<()>;
    fn unregister(&mut self, id: i32) -> io::Result<()>;
    fn tray(&mut self, on: bool) -> io::Result<()>;
}
#[derive(Default)]
pub struct Registration {
    pub applied: Preferences,
    pub tray_ready: bool,
    id: i32,
}
impl Registration {
    pub fn apply(&mut self, next: Preferences, os: &mut impl Bindings) -> io::Result<()> {
        next.validate()?;
        let old = self.applied;
        let changed = old.enabled != next.enabled || old.shortcut != next.shortcut;
        let id = if self.id == 1 { 2 } else { 1 };
        if changed && next.enabled {
            os.register(id, next.shortcut)?;
        }
        if next.stay != self.tray_ready {
            if let Err(e) = os.tray(next.stay) {
                self.tray_ready = false;
                if changed && next.enabled {
                    if let Err(rollback) = os.unregister(id) {
                        return Err(io::Error::other(format!(
                            "{e}; 새 단축키 해제 실패: {rollback}"
                        )));
                    }
                }
                return Err(e);
            }
        }
        if changed && old.enabled {
            if let Err(e) = os.unregister(self.id) {
                let key_rollback = if next.enabled {
                    os.unregister(id).err()
                } else {
                    None
                };
                let tray_rollback = if next.stay != old.stay {
                    os.tray(old.stay).err()
                } else {
                    None
                };
                self.tray_ready = old.stay && tray_rollback.is_none();
                return Err(io::Error::other(format!(
                    "이전 단축키 해제 실패: {e}; 복구: {key_rollback:?} {tray_rollback:?}"
                )));
            }
        }
        if changed {
            self.id = id;
        }
        self.applied = next;
        self.tray_ready = next.stay;
        Ok(())
    }
}
#[derive(Default)]
pub struct Lifecycle {
    pub hidden: bool,
    pub exit: bool,
}
impl Lifecycle {
    pub fn close(&mut self, stay: bool, tray_ready: bool) -> bool {
        if !self.exit && stay && tray_ready {
            self.hidden = true;
            true
        } else {
            self.exit = true;
            false
        }
    }
    pub fn show(&mut self) {
        self.hidden = false;
    }
}
pub enum Event {
    Toggle,
    Show,
    Settings,
    Exit,
    Notice(String),
    Tray {
        ready: bool,
        error: Option<String>,
    },
    Applied {
        preferences: Preferences,
        tray_ready: bool,
        error: Option<String>,
    },
    Failed(String),
}
enum Command {
    Apply(Preferences),
    Heartbeat(bool),
    Quit,
}
pub struct Host {
    commands: mpsc::Sender<Command>,
    pub events: mpsc::Receiver<Event>,
    hwnd: isize,
    thread: Option<std::thread::JoinHandle<()>>,
}
impl Host {
    pub fn start(ctx: eframe::egui::Context) -> io::Result<Self> {
        let (tx, rx) = mpsc::channel();
        let (events, receive) = mpsc::channel();
        let (ready, wait) = mpsc::sync_channel(1);
        let thread = std::thread::spawn(move || {
            let wake = ctx.clone();
            if let Err(e) = native::run(rx, events.clone(), ctx, ready) {
                let _ = events.send(Event::Failed(e.to_string()));
                wake.request_repaint();
            }
        });
        let hwnd = wait
            .recv()
            .map_err(|_| io::Error::other("Windows 검색창 제어를 시작하지 못했습니다"))??;
        Ok(Self {
            commands: tx,
            events: receive,
            hwnd,
            thread: Some(thread),
        })
    }
    pub fn apply(&self, p: Preferences) -> io::Result<()> {
        self.send(Command::Apply(p))
    }
    pub fn heartbeat(&self, on: bool) -> io::Result<()> {
        self.send(Command::Heartbeat(on))
    }
    fn send(&self, cmd: Command) -> io::Result<()> {
        self.commands
            .send(cmd)
            .map_err(|_| io::Error::other("Windows 제어 작업이 종료되었습니다"))?;
        native::wake(self.hwnd)
    }
}
impl Drop for Host {
    fn drop(&mut self) {
        let _ = self.send(Command::Quit);
        native::cancel_menu(self.hwnd);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[derive(Default)]
pub struct Desktop {
    host: Option<Host>,
    pub applied: Preferences,
    pub tray_ready: bool,
    pub life: Lifecycle,
    pub initialized: bool,
    pub busy: bool,
    save_pending: bool,
    heartbeat: bool,
}
impl Desktop {
    pub fn poll(&mut self, ctx: &eframe::egui::Context) -> Vec<Event> {
        let mut events = vec![];
        if !self.initialized {
            self.initialized = true;
            let preferences = match crate::quick_settings::path()
                .and_then(|p| crate::app_settings::load_quick(&p))
            {
                Ok(p) => p,
                Err(e) => {
                    events.push(Event::Notice(format!(
                        "검색창 설정을 읽지 못해 기본값을 사용합니다: {e}"
                    )));
                    Preferences::default()
                }
            };
            match Host::start(ctx.clone()) {
                Ok(host) => {
                    if let Err(e) = host.apply(preferences) {
                        events.push(Event::Failed(e.to_string()));
                    } else {
                        self.busy = true;
                    }
                    self.host = Some(host);
                }
                Err(e) => events.push(Event::Failed(e.to_string())),
            }
        }
        if let Some(host) = &self.host {
            events.extend(host.events.try_iter());
        }
        let mut notices = vec![];
        for event in &events {
            match event {
                Event::Applied {
                    preferences,
                    tray_ready,
                    error,
                } => {
                    self.applied = *preferences;
                    self.tray_ready = *tray_ready;
                    self.busy = false;
                    if self.save_pending && error.is_none() {
                        if let Err(e) = crate::quick_settings::path()
                            .and_then(|p| crate::app_settings::save_quick(&p, *preferences))
                        {
                            notices.push(Event::Notice(format!("현재 설정은 적용됐지만 저장하지 못했습니다. 다음 실행에서 다시 설정해 주세요: {e}")));
                        }
                    }
                    self.save_pending = false;
                }
                Event::Tray { ready, .. } => self.tray_ready = *ready,
                Event::Failed(_) => {
                    self.host = None;
                    self.tray_ready = false;
                    self.applied.enabled = false;
                    self.applied.stay = false;
                    self.busy = false;
                }
                _ => {}
            }
        }
        events.extend(notices);
        events
    }
    pub fn apply(&mut self, ctx: &eframe::egui::Context, p: Preferences) -> io::Result<()> {
        if self.busy {
            return Err(io::Error::other("이전 설정을 적용 중입니다"));
        }
        if self.host.is_none() {
            self.host = Some(Host::start(ctx.clone())?);
        }
        self.host.as_ref().unwrap().apply(p)?;
        self.busy = true;
        self.save_pending = true;
        Ok(())
    }
    pub fn heartbeat(&mut self, on: bool) -> io::Result<()> {
        if on != self.heartbeat {
            if let Some(host) = &self.host {
                host.heartbeat(on)?;
            }
            self.heartbeat = on;
        }
        Ok(())
    }
}
mod native {
    use super::*;
    use std::ptr::{null, null_mut};
    use windows_sys::Win32::{
        Foundation::*,
        UI::{Input::KeyboardAndMouse::*, Shell::*, WindowsAndMessaging::*},
    };
    const CONTROL: u32 = WM_APP + 31;
    pub(super) const TRAY: u32 = WM_APP + 32;
    const QUEUED_TRAY: u32 = WM_APP + 33;
    const RECOVER_TRAY: u32 = WM_APP + 34;
    struct Wake {
        events: mpsc::Sender<Event>,
        ctx: eframe::egui::Context,
        taskbar: u32,
    }
    unsafe extern "system" fn procedure(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
        let pointer = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *const Wake;
        if msg == WM_CANCELMODE {
            unsafe {
                EndMenu();
            }
            return 0;
        }
        if !pointer.is_null() {
            let state = unsafe { &*pointer };
            // 직접 전달된 Shell 알림도 메시지 루프에서 한 번만 처리하도록 큐에 전달
            if msg == TRAY || msg == state.taskbar {
                let queued = if msg == TRAY {
                    QUEUED_TRAY
                } else {
                    RECOVER_TRAY
                };
                if unsafe { PostMessageW(hwnd, queued, w, l) } == 0 {
                    let _ = state
                        .events
                        .send(Event::Failed(io::Error::last_os_error().to_string()));
                    state.ctx.request_repaint();
                }
                return 0;
            }
            if msg == WM_HOTKEY {
                let _ = state.events.send(Event::Toggle);
                state.ctx.request_repaint();
                return 0;
            }
            if msg == WM_TIMER {
                state.ctx.request_repaint();
                return 0;
            }
        }
        unsafe { DefWindowProcW(hwnd, msg, w, l) }
    }
    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(Some(0)).collect()
    }
    pub(super) fn wake(hwnd: isize) -> io::Result<()> {
        if unsafe { PostMessageW(hwnd as HWND, CONTROL, 0, 0) } == 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }
    pub(super) fn cancel_menu(hwnd: isize) {
        unsafe {
            PostMessageW(hwnd as HWND, WM_CANCELMODE, 0, 0);
        }
    }
    struct Windows {
        hwnd: HWND,
        icon: HICON,
    }
    impl Windows {
        fn icon(&mut self) -> io::Result<HICON> {
            if !self.icon.is_null() {
                return Ok(self.icon);
            }
            let bytes = include_bytes!("../assets/brand/diskweave-icon.ico");
            let offset = u32::from_le_bytes(bytes[18..22].try_into().unwrap()) as usize;
            let size = u32::from_le_bytes(bytes[14..18].try_into().unwrap()) as usize;
            let image = bytes
                .get(offset..offset + size)
                .ok_or_else(|| io::Error::other("프로젝트 아이콘 형식 오류"))?;
            self.icon = unsafe {
                CreateIconFromResourceEx(image.as_ptr(), image.len() as u32, 1, 0x30000, 32, 32, 0)
            };
            if self.icon.is_null() {
                Err(io::Error::last_os_error())
            } else {
                Ok(self.icon)
            }
        }
    }
    impl Bindings for Windows {
        fn register(&mut self, id: i32, key: Shortcut) -> io::Result<()> {
            if unsafe { RegisterHotKey(self.hwnd, id, key.modifiers | MOD_NOREPEAT, key.key) } == 0
            {
                Err(io::Error::other(format!(
                    "단축키를 등록하지 못했습니다. 다른 조합을 선택하세요 ({})",
                    io::Error::last_os_error()
                )))
            } else {
                Ok(())
            }
        }
        fn unregister(&mut self, id: i32) -> io::Result<()> {
            if unsafe { UnregisterHotKey(self.hwnd, id) } == 0 {
                Err(io::Error::last_os_error())
            } else {
                Ok(())
            }
        }
        fn tray(&mut self, on: bool) -> io::Result<()> {
            let mut data = NOTIFYICONDATAW {
                cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
                hWnd: self.hwnd,
                uID: 1,
                ..Default::default()
            };
            if on {
                data.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
                data.uCallbackMessage = TRAY;
                data.hIcon = self.icon()?;
                let title = wide("DiskWeave · 검색·분석 작업 실행 중");
                data.szTip[..title.len()].copy_from_slice(&title);
            }
            if unsafe { Shell_NotifyIconW(if on { NIM_ADD } else { NIM_DELETE }, &data) } == 0 {
                Err(io::Error::other(
                    "트레이 아이콘을 준비하지 못했습니다. 창을 숨기지 않습니다",
                ))
            } else {
                Ok(())
            }
        }
    }
    impl Drop for Windows {
        fn drop(&mut self) {
            unsafe {
                UnregisterHotKey(self.hwnd, 1);
                UnregisterHotKey(self.hwnd, 2);
                let data = NOTIFYICONDATAW {
                    cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
                    hWnd: self.hwnd,
                    uID: 1,
                    ..Default::default()
                };
                Shell_NotifyIconW(NIM_DELETE, &data);
                if !self.icon.is_null() {
                    DestroyIcon(self.icon);
                }
                DestroyWindow(self.hwnd);
            }
        }
    }
    fn menu(hwnd: HWND) -> Option<Event> {
        unsafe {
            let menu = CreatePopupMenu();
            if menu.is_null() {
                return Some(Event::Failed("트레이 메뉴를 만들지 못했습니다".into()));
            }
            for (id, label) in [
                (1, "빠른 검색"),
                (2, "메인 창 열기"),
                (3, "검색창 설정"),
                (4, "완전 종료"),
            ] {
                AppendMenuW(menu, MF_STRING, id, wide(label).as_ptr());
            }
            let mut point = POINT::default();
            GetCursorPos(&mut point);
            SetForegroundWindow(hwnd);
            let choice = TrackPopupMenu(
                menu,
                TPM_RETURNCMD | TPM_RIGHTBUTTON,
                point.x,
                point.y,
                0,
                hwnd,
                null(),
            );
            DestroyMenu(menu);
            PostMessageW(hwnd, WM_NULL, 0, 0);
            match choice {
                1 => Some(Event::Toggle),
                2 => Some(Event::Show),
                3 => Some(Event::Settings),
                4 => Some(Event::Exit),
                _ => None,
            }
        }
    }
    pub(super) fn run(
        commands: mpsc::Receiver<Command>,
        events: mpsc::Sender<Event>,
        ctx: eframe::egui::Context,
        ready: mpsc::SyncSender<io::Result<isize>>,
    ) -> io::Result<()> {
        unsafe {
            let taskbar = RegisterWindowMessageW(wide("TaskbarCreated").as_ptr());
            if taskbar == 0 {
                return Err(io::Error::last_os_error());
            }
            let class = wide("DiskWeaveQuickSearchHost");
            let wc = WNDCLASSW {
                lpfnWndProc: Some(procedure),
                lpszClassName: class.as_ptr(),
                ..Default::default()
            };
            let atom = RegisterClassW(&wc);
            if atom == 0 && GetLastError() != ERROR_CLASS_ALREADY_EXISTS {
                let e = io::Error::last_os_error();
                let _ = ready.send(Err(io::Error::other(e.to_string())));
                return Err(e);
            }
            let hwnd = CreateWindowExW(
                0,
                class.as_ptr(),
                class.as_ptr(),
                0,
                0,
                0,
                0,
                0,
                null_mut(),
                null_mut(),
                null_mut(),
                null(),
            );
            if hwnd.is_null() {
                let e = io::Error::last_os_error();
                let _ = ready.send(Err(io::Error::other(e.to_string())));
                return Err(e);
            }
            let wake = Box::new(Wake {
                events: events.clone(),
                ctx: ctx.clone(),
                taskbar,
            });
            // 콜백 포인터는 Windows 소유 창을 파괴할 때까지 같은 주소로 유지
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, (&*wake as *const Wake) as isize);
            let mut os = Windows {
                hwnd,
                icon: null_mut(),
            };
            let mut registration = Registration::default();
            let _ = ready.send(Ok(hwnd as isize));
            loop {
                for command in commands.try_iter() {
                    match command {
                        Command::Quit => return Ok(()),
                        Command::Heartbeat(on) => {
                            if on {
                                if SetTimer(hwnd, 1, 50, None) == 0 {
                                    return Err(io::Error::last_os_error());
                                }
                            } else {
                                KillTimer(hwnd, 1);
                            }
                        }
                        Command::Apply(p) => {
                            let error = registration.apply(p, &mut os).err().map(|e| e.to_string());
                            let _ = events.send(Event::Applied {
                                preferences: registration.applied,
                                tray_ready: registration.tray_ready,
                                error,
                            });
                            ctx.request_repaint();
                        }
                    }
                }
                let mut message = MSG::default();
                let result = GetMessageW(&mut message, null_mut(), 0, 0);
                if result <= 0 {
                    return if result == 0 {
                        Ok(())
                    } else {
                        Err(io::Error::last_os_error())
                    };
                }
                if message.message == RECOVER_TRAY && registration.applied.stay {
                    registration.tray_ready = false;
                    let error = registration
                        .apply(registration.applied, &mut os)
                        .err()
                        .map(|e| e.to_string());
                    let _ = events.send(Event::Tray {
                        ready: registration.tray_ready,
                        error,
                    });
                    ctx.request_repaint();
                }
                if message.message == QUEUED_TRAY {
                    let event = match message.lParam as u32 {
                        WM_RBUTTONUP => menu(hwnd),
                        WM_LBUTTONDBLCLK => Some(Event::Show),
                        _ => None,
                    };
                    if let Some(event) = event {
                        let _ = events.send(event);
                        ctx.request_repaint();
                    }
                }
                TranslateMessage(&message);
                DispatchMessageW(&message);
            }
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sent_tray_double_click_reaches_the_main_window_event() {
        use windows_sys::Win32::UI::WindowsAndMessaging::{SendMessageW, WM_LBUTTONDBLCLK};
        let host = Host::start(eframe::egui::Context::default()).unwrap();
        unsafe {
            SendMessageW(host.hwnd as _, native::TRAY, 1, WM_LBUTTONDBLCLK as isize);
        }
        assert!(matches!(
            host.events.recv_timeout(std::time::Duration::from_secs(2)),
            Ok(Event::Show)
        ));
        use windows_sys::Win32::UI::WindowsAndMessaging::PostMessageW;
        unsafe {
            PostMessageW(host.hwnd as _, native::TRAY, 1, WM_LBUTTONDBLCLK as isize);
        }
        assert!(matches!(
            host.events.recv_timeout(std::time::Duration::from_secs(2)),
            Ok(Event::Show)
        ));
        assert!(host.events.try_recv().is_err());
    }

    #[test]
    fn sent_taskbar_recreation_restores_only_the_owned_tray_icon() {
        use windows_sys::Win32::UI::{Shell::*, WindowsAndMessaging::*};
        let host = Host::start(eframe::egui::Context::default()).unwrap();
        host.apply(Preferences {
            stay: true,
            ..Default::default()
        })
        .unwrap();
        assert!(matches!(
            host.events.recv_timeout(std::time::Duration::from_secs(2)),
            Ok(Event::Applied {
                tray_ready: true,
                error: None,
                ..
            })
        ));
        let icon = NOTIFYICONDATAW {
            cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: host.hwnd as _,
            uID: 1,
            ..Default::default()
        };
        unsafe {
            assert_ne!(Shell_NotifyIconW(NIM_DELETE, &icon), 0);
            let name: Vec<u16> = "TaskbarCreated".encode_utf16().chain(Some(0)).collect();
            let message = RegisterWindowMessageW(name.as_ptr());
            assert_ne!(message, 0);
            SendMessageW(host.hwnd as _, message, 0, 0);
        }
        assert!(matches!(
            host.events.recv_timeout(std::time::Duration::from_secs(2)),
            Ok(Event::Tray {
                ready: true,
                error: None
            })
        ));
    }
    #[derive(Default)]
    struct Adapter {
        keys: std::collections::HashMap<i32, Shortcut>,
        tray: bool,
        fail_key: bool,
        fail_tray: bool,
    }
    impl Bindings for Adapter {
        fn register(&mut self, id: i32, key: Shortcut) -> io::Result<()> {
            if self.fail_key {
                Err(io::Error::other("충돌"))
            } else {
                self.keys.insert(id, key);
                Ok(())
            }
        }
        fn unregister(&mut self, id: i32) -> io::Result<()> {
            self.keys.remove(&id);
            Ok(())
        }
        fn tray(&mut self, on: bool) -> io::Result<()> {
            if self.fail_tray {
                Err(io::Error::other("트레이 실패"))
            } else {
                self.tray = on;
                Ok(())
            }
        }
    }
    #[test]
    fn failed_registration_keeps_old_shortcut_and_tray_failure_never_hides() {
        let mut a = Adapter::default();
        let mut state = Registration::default();
        let p = Preferences {
            enabled: true,
            ..Default::default()
        };
        state.apply(p, &mut a).unwrap();
        let old = a.keys.clone();
        a.fail_key = true;
        let new = Preferences {
            shortcut: Shortcut {
                key: 65,
                ..p.shortcut
            },
            ..p
        };
        assert!(state.apply(new, &mut a).is_err());
        assert_eq!(state.applied, p);
        assert_eq!(a.keys, old);
        a.fail_key = false;
        a.fail_tray = true;
        assert!(state
            .apply(Preferences { stay: true, ..new }, &mut a)
            .is_err());
        assert!(!state.tray_ready);
        assert_eq!(a.keys, old);
        let mut life = Lifecycle::default();
        assert!(!life.close(true, false));
        assert!(!life.hidden);
        life = Lifecycle::default();
        assert!(life.close(true, true));
        assert!(life.hidden);
        life.exit = true;
        assert!(!life.close(true, true));
        state.apply(Preferences::default(), &mut a).unwrap();
        assert!(a.keys.is_empty());
    }

    #[test]
    fn lost_tray_can_be_disabled_without_deleting_a_missing_icon() {
        let mut os = Adapter::default();
        let mut state = Registration::default();
        state
            .apply(
                Preferences {
                    stay: true,
                    ..Default::default()
                },
                &mut os,
            )
            .unwrap();
        state.tray_ready = false;
        os.fail_tray = true;
        state.apply(Preferences::default(), &mut os).unwrap();
        assert!(!state.applied.stay);
        let mut life = Lifecycle::default();
        assert!(life.close(true, true));
        life.show();
        assert!(!life.hidden);
        assert!(!life.close(false, false));
        assert!(life.exit);
    }

    #[test]
    fn owned_windows_register_conflict_disable_and_drop_release_hotkeys() {
        fn applied(host: &Host, p: Preferences) -> (Preferences, Option<String>) {
            host.apply(p).unwrap();
            match host
                .events
                .recv_timeout(std::time::Duration::from_secs(5))
                .unwrap()
            {
                Event::Applied {
                    preferences, error, ..
                } => (preferences, error),
                _ => panic!("설정 응답 필요"),
            }
        }
        let first = Host::start(eframe::egui::Context::default()).unwrap();
        let mut chosen = None;
        for key in 124..=135 {
            let p = Preferences {
                enabled: true,
                shortcut: Shortcut { modifiers: 7, key },
                ..Default::default()
            };
            if applied(&first, p).1.is_none() {
                chosen = Some(p);
                break;
            }
        }
        let p = chosen.expect("시험용 단축키 등록 필요");
        let second = Host::start(eframe::egui::Context::default()).unwrap();
        let (actual, error) = applied(&second, p);
        assert!(error.is_some());
        assert!(!actual.enabled);
        assert!(applied(&first, Preferences::default()).1.is_none());
        assert!(applied(&second, p).1.is_none());
        drop(second);
        assert!(applied(&first, p).1.is_none());
        first.heartbeat(true).unwrap();
        first.heartbeat(false).unwrap();
    }
}
