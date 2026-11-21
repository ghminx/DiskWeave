use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::{
    io::{self, Read},
    path::Path,
};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WindowState {
    pub position: [f32; 2],
    pub size: [f32; 2],
    pub maximized: bool,
}
pub struct Screen {
    pub work: eframe::egui::Rect,
    pub scale: f32,
}
impl WindowState {
    pub fn fit(&self, screens: &[Screen]) -> io::Result<Self> {
        let position = eframe::egui::pos2(self.position[0], self.position[1]);
        let screen = screens
            .iter()
            .min_by(|a, b| {
                let distance = |s: &Screen| {
                    (position.x - position.x.clamp(s.work.left(), s.work.right())).powi(2)
                        + (position.y - position.y.clamp(s.work.top(), s.work.bottom())).powi(2)
                };
                distance(a).total_cmp(&distance(b))
            })
            .ok_or_else(|| io::Error::other("사용 가능한 화면을 확인하지 못했습니다"))?;
        let size = [
            self.size[0].min(screen.work.width() / screen.scale),
            self.size[1].min(screen.work.height() / screen.scale),
        ];
        Ok(Self {
            position: [
                position.x.clamp(
                    screen.work.left(),
                    screen.work.right() - size[0] * screen.scale,
                ),
                position.y.clamp(
                    screen.work.top(),
                    screen.work.bottom() - size[1] * screen.scale,
                ),
            ],
            size,
            maximized: self.maximized,
        })
    }
}
pub fn screens() -> io::Result<Vec<Screen>> {
    use windows_sys::Win32::{
        Foundation::{LPARAM, RECT},
        Graphics::Gdi::*,
        UI::HiDpi::*,
    };
    struct Probe {
        screens: Vec<Screen>,
        error: Option<String>,
    }
    unsafe extern "system" fn collect(
        monitor: HMONITOR,
        _: HDC,
        _: *mut RECT,
        context: LPARAM,
    ) -> i32 {
        let probe = unsafe { &mut *(context as *mut Probe) };
        let mut info = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if unsafe { GetMonitorInfoW(monitor, &mut info) } == 0 {
            probe.error = Some(io::Error::last_os_error().to_string());
            return 0;
        }
        let (mut x, mut y) = (0, 0);
        let status = unsafe { GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut x, &mut y) };
        if status < 0 || x == 0 {
            probe.error = Some(format!("모니터 배율 조회 실패 ({status:#X})"));
            return 0;
        }
        probe.screens.push(Screen {
            work: eframe::egui::Rect::from_min_max(
                eframe::egui::pos2(info.rcWork.left as f32, info.rcWork.top as f32),
                eframe::egui::pos2(info.rcWork.right as f32, info.rcWork.bottom as f32),
            ),
            scale: x as f32 / 96.0,
        });
        1
    }
    let mut probe = Probe {
        screens: vec![],
        error: None,
    };
    // 화면 위치는 물리 픽셀, 크기는 논리 단위로 분리하여 배율 변경 후에도 다시 배치
    let ok = unsafe {
        EnumDisplayMonitors(
            std::ptr::null_mut(),
            std::ptr::null(),
            Some(collect),
            (&mut probe as *mut Probe) as LPARAM,
        )
    };
    if let Some(error) = probe.error {
        return Err(io::Error::other(error));
    }
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(probe.screens)
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct UiSettings {
    pub window: Option<WindowState>,
    pub columns: Option<[f32; 8]>,
    pub sort: crate::search::SortField,
    pub descending: bool,
    pub map_height: f32,
    pub map_collapsed: bool,
    pub workers: u8,
    pub folder_auto_refresh: bool,
}
impl Default for UiSettings {
    fn default() -> Self {
        Self {
            window: None,
            columns: None,
            sort: Default::default(),
            descending: true,
            map_height: 240.0,
            map_collapsed: false,
            workers: 0,
            folder_auto_refresh: true,
        }
    }
}
#[derive(Default, Serialize, Deserialize)]
pub struct Settings {
    #[serde(flatten)]
    pub quick: crate::quick_settings::Preferences,
    #[serde(default)]
    pub ui: UiSettings,
}
impl UiSettings {
    pub fn validate(&self) -> io::Result<()> {
        let finite = |v: f32, min: f32, max: f32| v.is_finite() && (min..=max).contains(&v);
        if !finite(self.map_height, 64.0, 2000.0)
            || ![0, 1, 2, 4, 8].contains(&self.workers)
            || self
                .columns
                .is_some_and(|c| c.into_iter().any(|v| !finite(v, 80.0, 800.0)))
            || self.window.as_ref().is_some_and(|w| {
                w.position
                    .iter()
                    .any(|&v| !finite(v, -1_000_000.0, 1_000_000.0))
                    || w.size.iter().any(|&v| !finite(v, 100.0, 32768.0))
            })
        {
            return Err(io::Error::other(
                "화면 설정 값이 올바른 범위를 벗어났습니다",
            ));
        }
        Ok(())
    }
}
fn read<T: DeserializeOwned + Default>(path: &Path) -> io::Result<T> {
    let file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(T::default()),
        Err(e) => return Err(e),
    };
    let mut bytes = Vec::new();
    file.take(8193).read_to_end(&mut bytes)?;
    if bytes.len() > 8192 {
        return Err(io::Error::other("설정 파일이 너무 큽니다"));
    }
    serde_json::from_slice(&bytes).map_err(crate::output::json_error)
}
pub fn load(path: &Path) -> io::Result<Settings> {
    let settings: Settings = read(path)?;
    settings.quick.validate()?;
    settings.ui.validate()?;
    Ok(settings)
}
pub fn load_quick(path: &Path) -> io::Result<crate::quick_settings::Preferences> {
    let preferences: crate::quick_settings::Preferences = read(path)?;
    preferences.validate()?;
    Ok(preferences)
}
fn save(path: &Path, settings: &Settings) -> io::Result<()> {
    settings.quick.validate()?;
    settings.ui.validate()?;
    std::fs::create_dir_all(
        path.parent()
            .ok_or_else(|| io::Error::other("설정 폴더가 없습니다"))?,
    )?;
    crate::output::write(
        path,
        true,
        || false,
        |writer| serde_json::to_writer(writer, settings).map_err(crate::output::json_error),
    )
}
pub fn save_ui(path: &Path, ui: UiSettings) -> io::Result<()> {
    let mut s = load(path)?;
    s.ui = ui;
    save(path, &s)
}
pub fn save_quick(path: &Path, quick: crate::quick_settings::Preferences) -> io::Result<()> {
    let mut s = load(path)?;
    s.quick = quick;
    save(path, &s)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn legacy_quick_options_survive_ui_updates_without_storing_session_data() {
        let fixture = crate::duplicates::tests::Fixture::new();
        let path = fixture.0.join("preferences.json");
        let quick = crate::quick_settings::Preferences {
            enabled: true,
            stay: true,
            ..Default::default()
        };
        save_quick(&path, quick).unwrap();
        let mut settings = load(&path).unwrap();
        assert_eq!(settings.quick, quick);
        assert!(settings.ui.folder_auto_refresh);
        settings.ui.folder_auto_refresh = false;
        settings.ui.map_collapsed = true;
        settings.ui.columns = Some([160.0; 8]);
        settings.ui.workers = 4;
        save_ui(&path, settings.ui.clone()).unwrap();
        assert_eq!(load(&path).unwrap().ui, settings.ui);
        let changed = crate::quick_settings::Preferences {
            enabled: false,
            ..quick
        };
        save_quick(&path, changed).unwrap();
        let restored = load(&path).unwrap();
        assert_eq!(restored.quick, changed);
        assert_eq!(restored.ui, settings.ui);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains(&fixture.0.display().to_string()));
        for key in ["search", "history", "location", "files", "path"] {
            assert!(!text.contains(&format!("\"{key}\"")));
        }
        let mut invalid_ui: serde_json::Value = serde_json::from_str(&text).unwrap();
        invalid_ui["ui"]["workers"] = 3.into();
        std::fs::write(&path, invalid_ui.to_string()).unwrap();
        assert!(load(&path).is_err());
        assert_eq!(load_quick(&path).unwrap(), changed);
        std::fs::write(&path, b"{broken").unwrap();
        assert!(save_ui(&path, UiSettings::default()).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"{broken");
    }
    #[test]
    fn invalid_versions_ranges_and_write_failure_preserve_existing_settings() {
        let fixture = crate::duplicates::tests::Fixture::new();
        let path = fixture.0.join("preferences.json");
        save_ui(&path, UiSettings::default()).unwrap();
        let original = std::fs::read(&path).unwrap();
        let invalid = UiSettings {
            map_height: f32::INFINITY,
            ..Default::default()
        };
        assert!(save_ui(&path, invalid).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), original);
        let mut permissions = std::fs::metadata(&path).unwrap().permissions();
        let original_permissions = permissions.clone();
        permissions.set_readonly(true);
        std::fs::set_permissions(&path, permissions.clone()).unwrap();
        let result = save_ui(
            &path,
            UiSettings {
                map_collapsed: true,
                ..Default::default()
            },
        );
        std::fs::set_permissions(&path, original_permissions).unwrap();
        assert!(result.is_err());
        assert_eq!(std::fs::read(&path).unwrap(), original);
        let mut json: serde_json::Value = serde_json::from_slice(&original).unwrap();
        json["version"] = 2.into();
        let unsupported = serde_json::to_vec(&json).unwrap();
        std::fs::write(&path, &unsupported).unwrap();
        assert!(load(&path).is_err());
        assert!(save_ui(&path, UiSettings::default()).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), unsupported);
        std::fs::write(&path, vec![b' '; 8193]).unwrap();
        assert!(load(&path).is_err());
    }
    #[test]
    fn changed_monitor_layout_and_scale_keep_restored_windows_visible() {
        let screens = [
            Screen {
                work: eframe::egui::Rect::from_min_size(
                    eframe::egui::pos2(-1920.0, 0.0),
                    eframe::egui::vec2(1920.0, 1080.0),
                ),
                scale: 1.0,
            },
            Screen {
                work: eframe::egui::Rect::from_min_size(
                    eframe::egui::Pos2::ZERO,
                    eframe::egui::vec2(3840.0, 2160.0),
                ),
                scale: 2.0,
            },
        ];
        let window = WindowState {
            position: [9000.0, 9000.0],
            size: [3000.0, 2000.0],
            maximized: false,
        };
        let fit = window.fit(&screens).unwrap();
        assert!(fit.size[0] <= 1920.0 && fit.size[1] <= 1080.0);
        assert!(fit.position[0] >= 0.0 && fit.position[1] >= 0.0);
        let removed = fit.fit(&screens[..1]).unwrap();
        assert!(removed.position[0] < 0.0 && removed.position[0] >= -1920.0);
        assert!(window.fit(&[]).is_err());
    }
}
