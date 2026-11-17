use serde::{Deserialize, Serialize};
use std::{
    io::{self, Read},
    path::{Path, PathBuf},
};
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Shortcut {
    pub modifiers: u32,
    pub key: u32,
}
impl Shortcut {
    pub fn validate(self) -> io::Result<()> {
        if self.modifiers == 0
            || self.modifiers & !15 != 0
            || !(self.key == 32 || (65..=90).contains(&self.key) || (112..=135).contains(&self.key))
        {
            Err(io::Error::other(
                "Ctrl/Alt/Shift/Win과 Space·영문자·F1~F24를 조합하세요",
            ))
        } else {
            Ok(())
        }
    }
    pub fn key_name(self) -> String {
        match self.key {
            32 => "Space".into(),
            112..=135 => format!("F{}", self.key - 111),
            _ => char::from_u32(self.key).unwrap_or('?').to_string(),
        }
    }
    pub fn parse_key(text: &str) -> io::Result<u32> {
        let text = text.trim().to_ascii_uppercase();
        if text == "SPACE" {
            Ok(32)
        } else if let Some(f) = text
            .strip_prefix('F')
            .and_then(|s| s.parse::<u32>().ok())
            .filter(|n| (1..=24).contains(n))
        {
            Ok(111 + f)
        } else if text.len() == 1 && text.as_bytes()[0].is_ascii_uppercase() {
            Ok(u32::from(text.as_bytes()[0]))
        } else {
            Err(io::Error::other(
                "키에는 Space, 영문자 또는 F1~F24를 입력하세요",
            ))
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Preferences {
    pub version: u32,
    pub enabled: bool,
    pub stay: bool,
    pub shortcut: Shortcut,
}
impl Default for Preferences {
    fn default() -> Self {
        Self {
            version: 1,
            enabled: false,
            stay: false,
            shortcut: Shortcut {
                modifiers: 3,
                key: 32,
            },
        }
    }
}
impl Preferences {
    pub fn validate(self) -> io::Result<()> {
        if self.version != 1 {
            return Err(io::Error::other("지원하지 않는 검색창 설정 버전입니다"));
        }
        self.shortcut.validate()
    }
}
pub fn path() -> io::Result<PathBuf> {
    std::env::var_os("LOCALAPPDATA")
        .map(|p| PathBuf::from(p).join("DiskWeave/preferences.json"))
        .ok_or_else(|| io::Error::other("사용자 설정 폴더를 찾을 수 없습니다"))
}
pub fn load(path: &Path) -> io::Result<Preferences> {
    let file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Preferences::default()),
        Err(e) => return Err(e),
    };
    let mut bytes = vec![];
    file.take(8193).read_to_end(&mut bytes)?;
    if bytes.len() > 8192 {
        return Err(io::Error::other("설정 파일이 너무 큽니다"));
    }
    let p: Preferences = serde_json::from_slice(&bytes).map_err(crate::output::json_error)?;
    p.validate()?;
    Ok(p)
}
pub fn save(path: &Path, p: &Preferences) -> io::Result<()> {
    crate::app_settings::save_quick(path, *p)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn preferences_persist_only_options_and_reject_invalid_files() {
        let root =
            std::env::temp_dir().join(format!("diskweave-quick-prefs-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("preferences.json");
        assert_eq!(load(&path).unwrap(), Preferences::default());
        let p = Preferences {
            enabled: true,
            stay: true,
            ..Default::default()
        };
        save(&path, &p).unwrap();
        assert_eq!(load(&path).unwrap(), p);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("scan"));
        std::fs::write(&path, b"{bad").unwrap();
        assert!(load(&path).is_err());
        let invalid = Preferences {
            shortcut: Shortcut {
                modifiers: 0,
                key: 0,
            },
            ..p
        };
        assert!(save(&path, &invalid).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"{bad");
        let mut unsupported = serde_json::to_value(p).unwrap();
        unsupported["version"] = 99.into();
        std::fs::write(&path, unsupported.to_string()).unwrap();
        assert!(load(&path).is_err());
        std::fs::write(&path, vec![b' '; 8193]).unwrap();
        assert!(load(&path).is_err());
        assert!(save(&root, &p).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }
}
