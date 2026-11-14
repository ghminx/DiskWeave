use crate::scanner::ScanResult;
use std::collections::HashSet;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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
    pub fn new(result: &ScanResult) -> Self {
        let mut explorer = Self {
            folder: 0,
            entries: Vec::new(),
            selected: Item::Folder(0),
            expanded: HashSet::from([0]),
            visible_tree: Vec::new(),
        };
        explorer.open_folder(result, 0);
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
