use std::fs;
use std::path::{Path, PathBuf};

pub struct TargetSelection {
    path: Option<PathBuf>,
    text: String,
}
impl Default for TargetSelection {
    fn default() -> Self {
        Self {
            path: Some(PathBuf::from(r"C:\")),
            text: r"C:\".into(),
        }
    }
}

impl TargetSelection {
    pub fn accept_drop(&mut self, paths: &[PathBuf]) -> Result<(), String> {
        if paths.len() != 1 {
            return Err("폴더 하나만 놓을 수 있습니다".to_owned());
        }
        self.choose_folder(paths[0].clone())
    }

    pub fn choose_folder(&mut self, path: PathBuf) -> Result<(), String> {
        match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.is_dir() => {}
            Ok(_) => return Err("폴더를 선택해야 합니다".to_owned()),
            Err(error) => return Err(format!("경로를 확인할 수 없습니다: {error}")),
        }
        self.path = Some(path.clone());
        self.text = path.to_string_lossy().into_owned();
        Ok(())
    }

    pub fn set_text(&mut self, text: String) {
        self.path = if text.trim().is_empty() {
            None
        } else {
            Some(PathBuf::from(&text))
        };
        self.text = text;
    }
    /// 등록된 경로를 오프라인에서도 선택하고 실재 검증은 재스캔에 위임.
    pub fn choose_registered(&mut self, path: PathBuf) {
        self.text = path.to_string_lossy().into_owned();
        self.path = Some(path);
    }
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    pub fn text(&self) -> &str {
        &self.text
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};
    #[test]
    fn choosing_an_offline_registered_location_keeps_the_original_scan_target() {
        let path = PathBuf::from(r"Z:\diskweave-offline-location");
        let mut target = TargetSelection::default();
        target.choose_registered(path.clone());
        assert_eq!(target.path(), Some(path.as_path()));
        assert_eq!(target.text(), path.to_string_lossy());
    }
    #[test]
    fn startup_target_is_c_drive_and_explicit_clear_still_clears_it() {
        let mut selection = TargetSelection::default();
        assert_eq!(selection.text(), r"C:\");
        assert_eq!(selection.path(), Some(Path::new(r"C:\")));
        selection.set_text(String::new());
        assert_eq!(selection.path(), None);
    }

    #[test]
    fn dropping_one_folder_selects_it() {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root =
            std::env::temp_dir().join(format!("diskweave-drop-{}-{stamp}", std::process::id()));
        fs::create_dir(&root).unwrap();

        let mut selection = TargetSelection::default();
        selection.accept_drop(std::slice::from_ref(&root)).unwrap();
        assert_eq!(selection.path(), Some(root.as_path()));
        assert_eq!(selection.text(), root.to_string_lossy());

        fs::remove_dir(root).unwrap();
    }

    #[test]
    fn dropping_file_keeps_previous_folder() {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "diskweave-drop-file-{}-{stamp}",
            std::process::id()
        ));
        fs::create_dir(&root).unwrap();
        let file = root.join("data.bin");
        fs::write(&file, [1, 2, 3]).unwrap();

        let mut selection = TargetSelection::default();
        selection.accept_drop(std::slice::from_ref(&root)).unwrap();
        assert!(selection.accept_drop(&[file]).is_err());
        assert_eq!(selection.path(), Some(root.as_path()));

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn dropping_multiple_folders_keeps_previous_folder() {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let base = std::env::temp_dir().join(format!(
            "diskweave-drop-many-{}-{stamp}",
            std::process::id()
        ));
        let first = base.join("first");
        let second = base.join("second");
        fs::create_dir_all(&first).unwrap();
        fs::create_dir(&second).unwrap();

        let mut selection = TargetSelection::default();
        selection.accept_drop(std::slice::from_ref(&first)).unwrap();
        assert!(selection.accept_drop(&[first.clone(), second]).is_err());
        assert_eq!(selection.path(), Some(first.as_path()));

        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn chosen_folder_becomes_scan_target() {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root =
            std::env::temp_dir().join(format!("diskweave-picker-{}-{stamp}", std::process::id()));
        fs::create_dir(&root).unwrap();

        let mut selection = TargetSelection::default();
        selection.choose_folder(root.clone()).unwrap();
        assert_eq!(selection.path(), Some(root.as_path()));

        fs::remove_dir(root).unwrap();
    }

    #[test]
    fn typed_path_becomes_scan_target() {
        let path = PathBuf::from(r"C:\SampleData\Projects");
        let mut selection = TargetSelection::default();
        selection.set_text(path.display().to_string());
        assert_eq!(selection.path(), Some(path.as_path()));
    }

    #[test]
    fn clearing_typed_path_removes_scan_target() {
        let mut selection = TargetSelection::default();
        selection.set_text(r"C:\SampleData\Projects".to_owned());
        selection.set_text(String::new());
        assert_eq!(selection.path(), None);
    }
}
