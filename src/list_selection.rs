use crate::explorer::Item;
use std::collections::BTreeSet;

#[derive(Default, Debug, PartialEq, Eq)]
pub struct SelectionSummary {
    pub files: usize,
    pub folders: usize,
}
impl SelectionSummary {
    pub fn from_items(items: impl Iterator<Item = Item>) -> Self {
        let mut summary = Self::default();
        for item in items {
            match item {
                Item::File(_) => summary.files += 1,
                Item::Folder(_) => summary.folders += 1,
            }
        }
        summary
    }
    pub fn count(&self) -> usize {
        self.files + self.folders
    }
}

#[derive(Default)]
pub struct ListSelection {
    rows: BTreeSet<usize>,
    anchor: Option<usize>,
}

impl ListSelection {
    pub fn click(&mut self, row: usize, ctrl: bool, shift: bool) {
        if shift {
            let anchor = self.anchor.unwrap_or(row);
            if !ctrl {
                self.rows.clear();
            }
            self.rows.extend(anchor.min(row)..=anchor.max(row));
        } else {
            if !ctrl {
                self.rows.clear();
            }
            if !self.rows.remove(&row) {
                self.rows.insert(row);
            }
            self.anchor = Some(row);
        }
    }
    pub fn all(&mut self, len: usize) {
        self.rows = (0..len).collect();
        self.anchor = (len > 0).then_some(0);
    }
    pub fn clear(&mut self) {
        self.rows.clear();
        self.anchor = None;
    }
    pub fn count(&self) -> usize {
        self.rows.len()
    }
    pub fn contains(&self, row: usize) -> bool {
        self.rows.contains(&row)
    }
    pub fn rows(&self) -> impl Iterator<Item = usize> + '_ {
        self.rows.iter().copied()
    }
    pub fn single(&self, items: &[Item]) -> Option<Item> {
        if self.rows.len() != 1 {
            return None;
        }
        self.rows().next().and_then(|row| items.get(row)).copied()
    }

    pub fn analyze(
        &mut self,
        result: &crate::scanner::ScanResult,
        items: &[Item],
        explorer: &mut crate::explorer::Explorer,
        input: &mut crate::search::SearchInput,
    ) -> bool {
        let Some(item) = self.single(items) else {
            return false;
        };
        let folder = match item {
            Item::Folder(id) => id,
            Item::File(id) => result.files[id].parent,
        };
        explorer.open_folder(result, folder);
        explorer.select(item);
        *input = crate::search::SearchInput {
            sort: input.sort,
            descending: input.descending,
            ..Default::default()
        };
        self.clear();
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::explorer::Item;

    #[test]
    fn analysis_navigation_clears_filters_and_reveals_the_selected_file() {
        let root = std::env::temp_dir().join(format!(
            "diskweave-analysis-selection-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(root.join("nested")).unwrap();
        std::fs::write(root.join("nested").join("model.bin"), [0; 3]).unwrap();
        let scan = crate::scanner::scan(&root, || false).unwrap();
        let parent = scan.folder_index(std::path::Path::new("nested")).unwrap();
        let file = scan.file_children[parent][0];
        let items = [Item::File(file)];
        let mut explorer = crate::explorer::Explorer::new(&scan);
        let mut input = crate::search::SearchInput {
            text: "model".into(),
            ..Default::default()
        };
        let mut selection = ListSelection::default();
        selection.click(0, false, false);
        assert!(selection.analyze(&scan, &items, &mut explorer, &mut input));
        assert_eq!(
            (explorer.folder(), explorer.selected()),
            (parent, Item::File(file))
        );
        assert_eq!(input, crate::search::SearchInput::default());
        assert_eq!(selection.count(), 0);
        assert!(!selection.analyze(&scan, &items, &mut explorer, &mut input));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn ctrl_shift_and_select_all_follow_display_order_and_reset() {
        let items = [Item::File(3), Item::File(1), Item::Folder(2), Item::File(0)];
        let mut selection = ListSelection::default();
        selection.click(1, false, false);
        assert_eq!(selection.single(&items), Some(Item::File(1)));
        selection.click(3, true, false);
        assert_eq!(selection.count(), 2);
        selection.click(1, true, false);
        assert_eq!(selection.single(&items), Some(Item::File(0)));
        selection.click(0, false, false);
        selection.click(2, false, true);
        assert_eq!(selection.rows().collect::<Vec<_>>(), [0, 1, 2]);
        selection.all(items.len());
        assert_eq!(selection.count(), 4);
        assert!(selection.single(&items).is_none());
        selection.clear();
        assert_eq!(selection.count(), 0);
    }
}
