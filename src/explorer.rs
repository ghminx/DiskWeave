use crate::scanner::ScanResult;
use std::collections::{HashMap, HashSet};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Item {
    Folder(usize),
    File(usize),
}

pub struct Explorer {
    folder: usize,
    entries: Vec<Item>,
    selected: Item,
    expanded: HashSet<usize>,
    visible_tree: Vec<(usize, usize)>,
}

impl Explorer {
    /// 고정 ID가 남은 항목만 재연결하고 교체된 경로로 선택 승계 방지.
    pub fn rebase_entries(
        &self,
        old: &ScanResult,
        new: &ScanResult,
        before: &crate::usn::entries::Projection,
        after: &crate::usn::entries::Projection,
    ) -> Self {
        if before
            .key(Item::Folder(0))
            .is_none_or(|key| !after.same_scope(key))
        {
            return self.rebase(old, new);
        }
        let remap = |item| before.key(item).and_then(|key| after.item(key));
        let remap_folder = |folder| match remap(Item::Folder(folder)) {
            Some(Item::Folder(id)) => Some(id),
            _ => None,
        };
        let folder = std::iter::successors(Some(self.folder), |&id| old.folders[id].parent)
            .find_map(remap_folder)
            .unwrap_or(0);
        let expanded = self
            .expanded
            .iter()
            .copied()
            .filter_map(remap_folder)
            .collect();
        let mut next = Self::with_expanded(new, folder, expanded);
        if let Some(item) = remap(self.selected) {
            next.select(item);
        }
        next
    }
    /// 변경 세대의 인덱스를 경로로 다시 연결하고 사라진 폴더는 남은 상위로 복귀.
    pub fn rebase(&self, old: &ScanResult, new: &ScanResult) -> Self {
        let folders: HashMap<_, _> = new
            .folders
            .iter()
            .enumerate()
            .map(|(id, folder)| (folder.path.as_path(), id))
            .collect();
        let mut path = old.folders[self.folder].path.as_path();
        let folder = loop {
            if let Some(&id) = folders.get(path) {
                break id;
            }
            let Some(parent) = path.parent() else {
                break 0;
            };
            path = parent;
        };
        let expanded = self
            .expanded
            .iter()
            .filter_map(|&id| folders.get(old.folders[id].path.as_path()).copied())
            .collect();
        let mut next = Self::with_expanded(new, folder, expanded);
        let selected = crate::search::item_path(old, self.selected);
        if let Some(&id) = folders.get(selected.as_path()) {
            next.select(Item::Folder(id));
        } else if let Some(id) = selected
            .parent()
            .and_then(|parent| new.file_named(*folders.get(parent)?, selected.file_name()?))
        {
            next.select(Item::File(id));
        }
        next
    }
    pub fn new(result: &ScanResult) -> Self {
        Self::with_expanded(result, 0, HashSet::new())
    }

    pub(crate) fn with_expanded(
        result: &ScanResult,
        folder: usize,
        mut expanded: HashSet<usize>,
    ) -> Self {
        expanded.insert(0);
        let mut explorer = Self {
            folder,
            entries: Vec::new(),
            selected: Item::Folder(folder),
            expanded,
            visible_tree: Vec::new(),
        };
        explorer.open_folder(result, folder);
        explorer
    }

    pub fn folder(&self) -> usize {
        self.folder
    }

    pub fn entries(&self) -> &[Item] {
        &self.entries
    }

    pub fn selected(&self) -> Item {
        self.selected
    }

    pub fn select(&mut self, item: Item) {
        self.selected = item;
    }
    pub fn reveal(&mut self, result: &ScanResult, item: Item) {
        let mut parent = match item {
            Item::Folder(id) => result.folders[id].parent,
            Item::File(id) => Some(result.files[id].parent),
        };
        while let Some(id) = parent {
            self.expanded.insert(id);
            parent = result.folders[id].parent;
        }
        self.selected = item;
        self.rebuild_tree(result);
    }

    pub fn visible_tree(&self) -> &[(usize, usize)] {
        &self.visible_tree
    }

    pub fn is_expanded(&self, folder: usize) -> bool {
        self.expanded.contains(&folder)
    }

    pub fn toggle_folder(&mut self, result: &ScanResult, folder: usize) {
        if !self.expanded.remove(&folder) {
            self.expanded.insert(folder);
        }
        self.rebuild_tree(result);
    }

    fn rebuild_tree(&mut self, result: &ScanResult) {
        self.visible_tree.clear();
        let mut pending = vec![(0, 0)];
        while let Some((folder, depth)) = pending.pop() {
            self.visible_tree.push((folder, depth));
            if self.expanded.contains(&folder) {
                pending.extend(
                    result.children[folder]
                        .iter()
                        .rev()
                        .map(|&child| (child, depth + 1)),
                );
            }
        }
    }

    pub fn open_folder(&mut self, result: &ScanResult, folder: usize) {
        self.folder = folder;
        self.selected = Item::Folder(folder);
        let mut ancestor = result.folders[folder].parent;
        while let Some(parent) = ancestor {
            self.expanded.insert(parent);
            ancestor = result.folders[parent].parent;
        }
        self.rebuild_tree(result);
        self.entries.clear();
        self.entries
            .extend(result.children[folder].iter().copied().map(Item::Folder));
        self.entries
            .extend(result.file_children[folder].iter().copied().map(Item::File));
    }

    pub fn breadcrumbs(&self, result: &ScanResult) -> Vec<usize> {
        let mut path = vec![self.folder];
        while let Some(parent) = result.folders[*path.last().unwrap()].parent {
            path.push(parent);
        }
        path.reverse();
        path
    }

    pub fn up(&mut self, result: &ScanResult) {
        if let Some(parent) = result.folders[self.folder].parent {
            self.open_folder(result, parent);
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn live_results_preserve_navigation_and_fall_back_after_a_folder_leaves_the_scope() {
        use std::fs;
        let root =
            std::env::temp_dir().join(format!("diskweave-explorer-live-{}", std::process::id()));
        fs::create_dir_all(root.join("a/deep")).unwrap();
        fs::write(root.join("a/deep/known"), b"123").unwrap();
        let old = crate::scanner::scan(&root, || false).unwrap();
        let folder = old.folder_index(std::path::Path::new("a/deep")).unwrap();
        let mut view = super::Explorer::new(&old);
        view.open_folder(&old, folder);
        let selected = super::Item::File(old.file_children[folder][0]);
        view.select(selected);
        fs::write(root.join("new"), b"12345").unwrap();
        let new = crate::scanner::scan(&root, || false).unwrap();
        let live = view.rebase(&old, &new);
        assert_eq!(new.folders[live.folder()].path, root.join("a/deep"));
        assert!(matches!(live.selected(), super::Item::File(_)));
        fs::remove_dir_all(root.join("a/deep")).unwrap();
        let next = crate::scanner::scan(&root, || false).unwrap();
        let live = live.rebase(&new, &next);
        assert_eq!(next.folders[live.folder()].path, root.join("a"));
        fs::remove_dir_all(root).unwrap();
    }
    use super::*;
    use crate::scanner::{self, ScanResult};
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};

    struct Fixture {
        root: PathBuf,
        result: ScanResult,
    }

    impl Fixture {
        fn new() -> Self {
            let stamp = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let root = std::env::temp_dir()
                .join(format!("diskweave-explorer-{}-{stamp}", std::process::id()));
            fs::create_dir_all(root.join("a").join("deep")).unwrap();
            fs::create_dir(root.join("empty")).unwrap();
            fs::write(root.join("root.bin"), [0; 9]).unwrap();
            fs::write(root.join("a").join("nested.bin"), [0; 2]).unwrap();
            let result = scanner::scan(&root, || false).unwrap();
            Self { root, result }
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.root).unwrap();
        }
    }

    #[test]
    fn selecting_a_file_keeps_folder_scope_and_new_result_clears_old_selection() {
        let fixture = Fixture::new();
        let result = &fixture.result;
        let a = result.folder_index(Path::new("a")).unwrap();
        let file = result.file_children[a][0];
        let mut explorer = Explorer::new(result);
        explorer.open_folder(result, a);
        explorer.select(Item::File(file));
        assert_eq!(
            (explorer.folder(), explorer.selected()),
            (a, Item::File(file))
        );

        let other = Fixture::new();
        explorer = Explorer::new(&other.result);
        assert_eq!(
            (explorer.folder(), explorer.selected()),
            (0, Item::Folder(0))
        );
        assert_eq!(explorer.breadcrumbs(&other.result), vec![0]);
        assert!(!explorer.is_expanded(a));
    }

    #[test]
    fn tree_expansion_is_independent_of_selection_and_navigation_reveals_ancestors() {
        let fixture = Fixture::new();
        let result = &fixture.result;
        let a = result.folder_index(Path::new("a")).unwrap();
        let deep = result.folder_index(Path::new("a\\deep")).unwrap();
        let empty = result.folder_index(Path::new("empty")).unwrap();
        let mut explorer = Explorer::new(result);
        assert_eq!(explorer.visible_tree(), &[(0, 0), (a, 1), (empty, 1)]);
        explorer.toggle_folder(result, a);
        assert_eq!(
            explorer.visible_tree(),
            &[(0, 0), (a, 1), (deep, 2), (empty, 1)]
        );
        assert_eq!(explorer.selected(), Item::Folder(0));
        explorer.toggle_folder(result, a);
        explorer.open_folder(result, deep);
        assert!(explorer.visible_tree().contains(&(deep, 2)));
        assert_eq!(explorer.selected(), Item::Folder(deep));
        explorer.toggle_folder(result, 0);
        assert_eq!(explorer.visible_tree(), &[(0, 0)]);
        assert_eq!(explorer.folder(), deep);
    }

    #[test]
    fn folder_navigation_lists_only_direct_children_and_returns_to_parent() {
        let fixture = Fixture::new();
        let result = &fixture.result;
        let a = result.folder_index(Path::new("a")).unwrap();
        let deep = result.folder_index(Path::new("a\\deep")).unwrap();
        let empty = result.folder_index(Path::new("empty")).unwrap();
        let root_file = result.file_children[0][0];
        let mut explorer = Explorer::new(result);
        assert_eq!(
            explorer.entries(),
            &[Item::Folder(a), Item::Folder(empty), Item::File(root_file)]
        );
        explorer.open_folder(result, a);
        assert_eq!(explorer.folder(), a);
        assert_eq!(
            explorer.entries(),
            &[Item::Folder(deep), Item::File(result.file_children[a][0])]
        );
        explorer.open_folder(result, deep);
        assert_eq!(explorer.breadcrumbs(result), vec![0, a, deep]);
        assert!(explorer.entries().is_empty());
        explorer.up(result);
        assert_eq!(explorer.folder(), a);
        explorer.up(result);
        explorer.up(result);
        assert_eq!(explorer.folder(), 0);
    }
}
