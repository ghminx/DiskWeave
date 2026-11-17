use crate::{explorer::Item, locations::Snapshot, search::SearchInput};
use std::{io, path::PathBuf};

#[derive(Clone)]
pub struct ViewState {
    pub location: u64,
    pub folder: PathBuf,
    pub query: SearchInput,
    pub whole_scan: bool,
    pub global: bool,
    pub selected: Vec<PathBuf>,
    pub scroll: [f32; 2],
    pub tab: usize,
}
pub struct Resolved {
    pub source: Snapshot,
    pub folder: usize,
    pub selected: Vec<Item>,
    pub fallback: bool,
}
impl ViewState {
    pub fn resolve(&self, sources: &[Snapshot]) -> io::Result<Resolved> {
        let source = sources
            .iter()
            .find(|s| s.id == self.location)
            .ok_or_else(|| io::Error::other("이력의 분석 위치가 제거됐거나 갱신이 필요합니다"))?;
        let mut path = self.folder.as_path();
        let folder = loop {
            if let Some(id) = path
                .strip_prefix(&source.scan.folders[0].path)
                .ok()
                .and_then(|relative| source.scan.folder_index(relative).ok())
            {
                break id;
            }
            path = path
                .parent()
                .ok_or_else(|| io::Error::other("이력의 폴더로 이동할 수 없습니다"))?;
        };
        let mut selected = Vec::new();
        for target in &self.selected {
            let Some(relative) = target.strip_prefix(&source.scan.folders[0].path).ok() else {
                continue;
            };
            if let Ok(id) = source.scan.folder_index(relative) {
                selected.push(Item::Folder(id));
                continue;
            }
            if let Some(parent) = relative
                .parent()
                .and_then(|p| source.scan.folder_index(p).ok())
            {
                if let Some(&id) = source.scan.file_children[parent]
                    .iter()
                    .find(|&&i| Some(source.scan.files[i].name.as_os_str()) == target.file_name())
                {
                    selected.push(Item::File(id));
                }
            }
        }
        Ok(Resolved {
            source: source.clone(),
            folder,
            selected,
            fallback: path != self.folder,
        })
    }
    fn same_place(&self, other: &Self) -> bool {
        self.location == other.location
            && self.folder == other.folder
            && self.query == other.query
            && self.whole_scan == other.whole_scan
            && self.global == other.global
            && self.tab == other.tab
    }
}
#[derive(Default)]
pub struct History {
    back: Vec<ViewState>,
    forward: Vec<ViewState>,
}
impl History {
    pub fn visit(&mut self, from: ViewState, to: &ViewState) {
        if from.same_place(to) {
            return;
        }
        self.back.push(from);
        if self.back.len() > 64 {
            self.back.remove(0);
        }
        self.forward.clear();
    }
    pub fn can_back(&self) -> bool {
        !self.back.is_empty()
    }
    pub fn can_forward(&self) -> bool {
        !self.forward.is_empty()
    }
    pub fn back(&mut self, current: ViewState) -> Option<ViewState> {
        let previous = self.back.pop()?;
        self.forward.push(current);
        Some(previous)
    }
    pub fn forward(&mut self, current: ViewState) -> Option<ViewState> {
        let next = self.forward.pop()?;
        self.back.push(current);
        Some(next)
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Search,
    Path,
    Rescan,
    Open,
    CopyPath,
}
pub fn shortcut(
    key: eframe::egui::Key,
    m: eframe::egui::Modifiers,
    editing: bool,
    popup: bool,
    busy: bool,
) -> Option<Action> {
    use eframe::egui::Key;
    if popup {
        return None;
    }
    if m.ctrl && !m.alt && !m.shift {
        if key == Key::F {
            return Some(Action::Search);
        }
        if key == Key::L {
            return Some(Action::Path);
        }
    }
    if editing || busy {
        return None;
    }
    if m.ctrl && m.shift && !m.alt && key == Key::C {
        return Some(Action::CopyPath);
    }
    if m.ctrl || m.alt || m.shift || m.mac_cmd {
        return None;
    }
    match key {
        Key::F5 => Some(Action::Rescan),
        Key::Enter => Some(Action::Open),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn removed_navigation_keys_are_not_application_shortcuts() {
        use eframe::egui::{Key, Modifiers};
        for (key, modifiers) in [
            (Key::ArrowLeft, Modifiers::ALT),
            (Key::ArrowRight, Modifiers::ALT),
            (Key::ArrowUp, Modifiers::ALT),
            (Key::ArrowUp, Modifiers::NONE),
            (Key::ArrowDown, Modifiers::NONE),
        ] {
            assert_eq!(
                shortcut(key, modifiers, false, false, false),
                None,
                "{key:?} {modifiers:?}"
            );
        }
    }
    #[test]
    fn history_restores_search_selection_scroll_and_drops_forward_branch() {
        let fixture = crate::duplicates::tests::Fixture::new();
        std::fs::create_dir(fixture.0.join("nested")).unwrap();
        std::fs::write(fixture.0.join("nested/file"), b"data").unwrap();
        let source = fixture.snapshot();
        let a = ViewState {
            location: 1,
            folder: fixture.0.clone(),
            query: crate::search::SearchInput {
                text: "file".into(),
                ..Default::default()
            },
            whole_scan: true,
            global: false,
            selected: vec![fixture.0.join("nested/file")],
            scroll: [3.0, 48.0],
            tab: 3,
        };
        let b = ViewState {
            folder: fixture.0.join("nested"),
            query: Default::default(),
            whole_scan: false,
            scroll: [0.0, 0.0],
            tab: 0,
            ..a.clone()
        };
        let mut history = History::default();
        history.visit(a.clone(), &b);
        let restored = history.back(b.clone()).unwrap();
        assert_eq!(restored.query.text, "file");
        assert_eq!(restored.scroll, [3.0, 48.0]);
        assert_eq!(
            restored
                .resolve(std::slice::from_ref(&source))
                .unwrap()
                .selected
                .len(),
            1
        );
        assert_eq!(history.forward(restored.clone()).unwrap().folder, b.folder);
        history.back(b.clone()).unwrap();
        let c = ViewState {
            query: Default::default(),
            tab: 0,
            ..a.clone()
        };
        history.visit(a, &c);
        assert!(!history.can_forward());
        std::fs::remove_dir_all(fixture.0.join("nested")).unwrap();
        let next = fixture.snapshot();
        let resolved = b.resolve(&[next]).unwrap();
        assert!(resolved.fallback);
        assert_eq!(resolved.folder, 0);
        assert!(resolved.selected.is_empty());
        assert!(b.resolve(&[]).is_err());
    }
    #[test]
    fn keyboard_policy_preserves_text_popup_and_workflow_ownership() {
        use eframe::egui::{Key, Modifiers};
        for (key, modifiers, action) in [
            (Key::F, Modifiers::CTRL, Action::Search),
            (Key::L, Modifiers::CTRL, Action::Path),
            (Key::F5, Modifiers::NONE, Action::Rescan),
            (Key::Enter, Modifiers::NONE, Action::Open),
            (Key::C, Modifiers::CTRL | Modifiers::SHIFT, Action::CopyPath),
        ] {
            assert_eq!(shortcut(key, modifiers, false, false, false), Some(action));
            assert_eq!(shortcut(key, modifiers, false, true, false), None);
        }
        assert_eq!(
            shortcut(Key::F, Modifiers::CTRL, true, false, false),
            Some(Action::Search)
        );
        for (editing, popup, busy) in [
            (true, false, false),
            (false, true, false),
            (false, false, true),
        ] {
            assert_eq!(
                shortcut(Key::ArrowDown, Modifiers::NONE, editing, popup, busy),
                None
            );
            assert_eq!(
                shortcut(Key::Enter, Modifiers::NONE, editing, popup, busy),
                None
            );
            assert_eq!(
                shortcut(Key::F2, Modifiers::NONE, editing, popup, busy),
                None
            );
            assert_eq!(
                shortcut(Key::Delete, Modifiers::NONE, editing, popup, busy),
                None
            );
        }
    }
}
