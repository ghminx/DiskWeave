use super::{stream, Excluded, Index, Node, Unavailable};
use crate::{
    explorer::Item,
    metadata::Allocation,
    scanner::{PartialUpdate, ScanResult},
};
use std::collections::HashSet;
use std::{
    collections::BTreeMap,
    io,
    ops::{Deref, DerefMut},
    os::windows::ffi::{OsStrExt, OsStringExt},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
};

pub(super) enum Change {
    Node {
        path: PathBuf,
        expected: Option<u64>,
        value: Option<Node>,
        old_cost: u64,
    },
    Unavailable {
        path: PathBuf,
        present: bool,
        value: Option<Unavailable>,
        old_cost: u64,
    },
}
impl Change {
    pub fn budget_delta(&self) -> i128 {
        let (before, after) = match self {
            Self::Node {
                path,
                value,
                old_cost,
                ..
            } => (
                *old_cost,
                value
                    .as_ref()
                    .map_or(0, |node| stream::node_cost(path, node)),
            ),
            Self::Unavailable {
                path,
                value,
                old_cost,
                ..
            } => (
                *old_cost,
                value
                    .as_ref()
                    .map_or(0, |entry| stream::unavailable_cost(path, entry)),
            ),
        };
        i128::from(after) - i128::from(before)
    }
}

#[derive(Clone, Copy, Debug, Default, serde::Serialize)]
pub struct Stats {
    pub touched_nodes: usize,
    pub touched_unavailable: usize,
    pub undo_bytes: u64,
    pub changed_entries: usize,
}
pub(super) struct Budget {
    maximum: u64,
    used: AtomicU64,
}
impl Budget {
    fn new(maximum: u64) -> Self {
        Self {
            maximum,
            used: AtomicU64::new(0),
        }
    }
    pub fn charge(&self, bytes: u64) -> io::Result<()> {
        self.used
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |used| {
                used.checked_add(bytes).filter(|next| *next <= self.maximum)
            })
            .map(|_| ())
            .map_err(|_| io::Error::other("변경 원본 보관 예산 초과"))
    }
    fn used(&self) -> u64 {
        self.used.load(Ordering::Relaxed)
    }
}

struct IssueUndo {
    before: BTreeMap<PathBuf, Option<Unavailable>>,
    budget: Arc<Budget>,
}
#[derive(Default)]
pub(super) struct Issues {
    values: BTreeMap<PathBuf, Unavailable>,
    undo: Option<IssueUndo>,
}
impl Issues {
    pub fn new() -> Self {
        Self::default()
    }
    fn begin(&mut self, budget: Arc<Budget>) -> io::Result<()> {
        if self.undo.is_some() {
            return Err(io::Error::other("중첩된 변경 원본 보관"));
        }
        self.undo = Some(IssueUndo {
            before: BTreeMap::new(),
            budget,
        });
        Ok(())
    }
    fn record(
        undo: &mut Option<IssueUndo>,
        path: &Path,
        old: Option<&Unavailable>,
    ) -> io::Result<()> {
        if let Some(undo) = undo {
            if !undo.before.contains_key(path) {
                let cost = old.map_or(384 + path.as_os_str().len() as u64 * 4, |old| {
                    super::stream::unavailable_cost(path, old)
                });
                undo.budget.charge(cost)?;
                undo.before.insert(path.to_owned(), old.cloned());
            }
        }
        Ok(())
    }
    pub fn insert(&mut self, path: PathBuf, value: Unavailable) -> io::Result<Option<Unavailable>> {
        if self.values.get(&path) == Some(&value) {
            return Ok(Some(value));
        }
        Self::record(&mut self.undo, &path, self.values.get(&path))?;
        Ok(self.values.insert(path, value))
    }
    pub fn remove(&mut self, path: &Path) -> io::Result<Option<Unavailable>> {
        if !self.values.contains_key(path) {
            return Ok(None);
        }
        Self::record(&mut self.undo, path, self.values.get(path))?;
        Ok(self.values.remove(path))
    }
    pub fn subtree_keys<'a>(&'a self, root: &'a Path) -> impl Iterator<Item = &'a PathBuf> + 'a {
        self.values
            .range::<Path, _>((std::ops::Bound::Included(root), std::ops::Bound::Unbounded))
            .map(|(path, _)| path)
            .take_while(move |path| path.starts_with(root))
    }

    pub fn remove_subtree(&mut self, root: &Path, cancelled: &impl Fn() -> bool) -> io::Result<()> {
        let _sample = super::cost::measure(super::cost::Stage::SubtreeRemoval);
        loop {
            let batch: Vec<_> = self.subtree_keys(root).take(256).cloned().collect();
            if batch.is_empty() {
                return Ok(());
            }
            for path in batch {
                crate::output::check(cancelled)?;
                self.remove(&path)?;
            }
        }
    }
    pub fn changes(&self) -> impl Iterator<Item = Change> + '_ {
        self.undo
            .as_ref()
            .into_iter()
            .flat_map(|undo| &undo.before)
            .filter_map(|(path, old)| {
                let new = self.values.get(path);
                (old.as_ref() != new).then(|| Change::Unavailable {
                    path: path.clone(),
                    old_cost: old
                        .as_ref()
                        .map_or(0, |entry| stream::unavailable_cost(path, entry)),
                    present: old.is_some(),
                    value: new.cloned(),
                })
            })
    }
    fn original(&self, path: &Path) -> Option<&Unavailable> {
        match self.undo.as_ref().and_then(|undo| undo.before.get(path)) {
            Some(old) => old.as_ref(),
            None => self.values.get(path),
        }
    }
    fn touched(&self) -> usize {
        self.undo.as_ref().map_or(0, |undo| undo.before.len())
    }
    fn rollback(&mut self) {
        if let Some(undo) = self.undo.take() {
            for (path, old) in undo.before {
                match old {
                    Some(old) => {
                        self.values.insert(path, old);
                    }
                    None => {
                        self.values.remove(&path);
                    }
                }
            }
        }
    }
}
impl Deref for Issues {
    type Target = BTreeMap<PathBuf, Unavailable>;
    fn deref(&self) -> &Self::Target {
        &self.values
    }
}
impl<'a> IntoIterator for &'a Issues {
    type Item = (&'a PathBuf, &'a Unavailable);
    type IntoIter = std::collections::btree_map::Iter<'a, PathBuf, Unavailable>;
    fn into_iter(self) -> Self::IntoIter {
        self.values.iter()
    }
}
impl FromIterator<(PathBuf, Unavailable)> for Issues {
    fn from_iter<T: IntoIterator<Item = (PathBuf, Unavailable)>>(iter: T) -> Self {
        Self {
            values: iter.into_iter().collect(),
            undo: None,
        }
    }
}

pub(super) struct Edit<'a> {
    index: &'a mut Index,
    budget: Arc<Budget>,
    baseline: bool,
    finished: bool,
}
impl<'a> Edit<'a> {
    pub fn begin(index: &'a mut Index, maximum: u64, baseline: bool) -> io::Result<Self> {
        let budget = Arc::new(Budget::new(maximum));
        if baseline {
            if !index.nodes.is_empty() || !index.unavailable.is_empty() {
                return Err(io::Error::other("비어 있지 않은 기준 목록"));
            }
        } else {
            index.nodes.begin_edit(Arc::clone(&budget))?;
            if let Err(error) = index.unavailable.begin(Arc::clone(&budget)) {
                index.nodes.rollback();
                return Err(error);
            }
        }
        Ok(Self {
            index,
            budget,
            baseline,
            finished: false,
        })
    }
    pub fn undo_bytes(&self) -> u64 {
        self.budget.used()
    }
    pub fn commit(mut self, cancelled: &impl Fn() -> bool, emit_changes: bool) -> io::Result<bool> {
        crate::output::check(cancelled)?;
        let mut changes = Vec::new();
        let mut changed_entries = 0;
        let mut budget_change = 0;
        for change in self
            .index
            .nodes
            .changes()
            .chain(self.index.unavailable.changes())
        {
            crate::output::check(cancelled)?;
            changed_entries += 1;
            if emit_changes {
                budget_change += change.budget_delta();
            }
            changes.push(change);
        }
        crate::output::check(cancelled)?;
        let stats = Stats {
            touched_nodes: self.index.nodes.touched(),
            touched_unavailable: self.index.unavailable.touched(),
            undo_bytes: self.budget.used(),
            changed_entries: if self.baseline {
                self.index.nodes.len() + self.index.unavailable.len()
            } else {
                changed_entries
            },
        };
        let changed = stats.changed_entries > 0;
        let mapping_changed = self.index.nodes.mapping_changed();
        let patched = if changed && !self.baseline {
            patch_scan(self.index, &changes, mapping_changed, cancelled)?
        } else {
            None
        };
        let rechecked = if patched.is_some()
            && changes
                .iter()
                .any(|c| matches!(c, Change::Unavailable { .. }))
        {
            let mut entries = Vec::with_capacity(self.index.unavailable.len());
            for value in self.index.unavailable.values() {
                crate::output::check(cancelled)?;
                entries.push(value.clone());
            }
            Some(entries)
        } else {
            None
        };
        crate::output::check(cancelled)?;
        if emit_changes {
            self.index.budget_change = budget_change;
        }
        self.index.nodes.finish_edit();
        self.index.unavailable.undo = None;
        self.index.changes = if emit_changes { changes } else { vec![] };
        self.index.last_edit = stats;
        if changed {
            if let Some(unavailable) = rechecked {
                self.index.checkpoint.unavailable = unavailable;
            }
            self.index.scan = patched.map(|patch| {
                if let Some(excluded) = patch.excluded {
                    self.index.checkpoint.excluded = excluded;
                }
                if let Some((projection, ids, folders)) = patch.rows {
                    self.index.projection = Some(Arc::new(projection));
                    self.index.checkpoint.files = ids;
                    if let Some(folders) = folders {
                        self.index.checkpoint.folders = folders;
                    }
                }
                Arc::new(patch.scan)
            });
        }
        if mapping_changed && self.index.scan.is_none() {
            self.index.projection = None;
        }
        self.finished = true;
        Ok(changed)
    }
}
impl Deref for Edit<'_> {
    type Target = Index;
    fn deref(&self) -> &Index {
        self.index
    }
}
impl DerefMut for Edit<'_> {
    fn deref_mut(&mut self) -> &mut Index {
        self.index
    }
}
impl Drop for Edit<'_> {
    fn drop(&mut self) {
        if !self.finished && !self.baseline {
            self.index.nodes.rollback();
            self.index.unavailable.rollback();
        }
    }
}

struct ScanPatch {
    scan: ScanResult,
    rows: Option<(super::entries::Projection, Vec<u64>, Option<Vec<u64>>)>,
    excluded: Option<Vec<Excluded>>,
}

fn patch_scan(
    index: &Index,
    changes: &[Change],
    structural: bool,
    cancelled: &impl Fn() -> bool,
) -> io::Result<Option<ScanPatch>> {
    const MAX_PATCH_CHANGES: usize = 4096;
    let (Some(previous), Some(projection)) = (&index.scan, &index.projection) else {
        return Ok(None);
    };
    let find_existing = |path: &Path| {
        projection
            .path_key(path)
            .and_then(|key| projection.item(key))
    };
    let mut appended = Vec::new();
    for change in changes {
        crate::output::check(cancelled)?;
        let Change::Node {
            path,
            value: Some(node),
            ..
        } = change
        else {
            continue;
        };
        if node.directory_id().is_some() && index.nodes.original(path).is_none_or(Node::excluded) {
            let key = index.nodes.key_number(path).expect("추가한 폴더의 고정 ID");
            appended.push((path.as_path(), node, key));
        }
    }
    if !appended.is_empty() && changes.len() > MAX_PATCH_CHANGES {
        return patch_tree(index, previous, projection, changes, cancelled);
    }
    appended.sort_unstable_by(|a, b| {
        a.0.components()
            .count()
            .cmp(&b.0.components().count())
            .then_with(|| a.0.cmp(b.0))
    });
    let appended_rows: std::collections::HashMap<_, _> = appended
        .iter()
        .enumerate()
        .map(|(offset, &(path, _, _))| (path, previous.folders.len() + offset))
        .collect();
    let find = |path: &Path| {
        appended_rows
            .get(path)
            .copied()
            .map(Item::Folder)
            .or_else(|| find_existing(path))
    };
    let mut edits = Vec::with_capacity(changes.len());
    let mut changed_paths = HashSet::new();
    let mut access_changes = Vec::new();
    let mut excluded_changes = BTreeMap::new();
    let mut removed_folders = Vec::new();
    for change in changes {
        crate::output::check(cancelled)?;
        let (path, value) = match change {
            Change::Node { path, value, .. } => (path, value),
            Change::Unavailable { path, value, .. } => {
                let old = index.unavailable.original(path);
                let new = value.as_ref();
                if !old.zip(new).is_some_and(|(old, new)| old.same_status(new)) {
                    access_changes.push(path.as_path());
                }
                continue;
            }
        };
        let relative = path.as_path();
        let old = index.nodes.original(relative);
        let new = value.as_ref();
        if old.into_iter().chain(new).any(Node::excluded) {
            excluded_changes.insert(relative, new.filter(|node| node.excluded()));
        }
        if appended_rows.contains_key(relative) {
            continue;
        }
        let old = old.filter(|node| !node.excluded());
        let new = new.filter(|node| !node.excluded());
        if old.is_none() && new.is_none() {
            continue;
        }
        if old.and_then(|n| n.issue.as_ref()) != new.and_then(|n| n.issue.as_ref()) {
            return Ok(None);
        }
        if old.into_iter().chain(new).any(Node::dir) {
            if let (Some(Item::Folder(row)), Some(_), None) = (find(relative), old, new) {
                if row != 0 {
                    removed_folders.push((row, relative));
                    continue;
                }
            }
            if !old
                .zip(new)
                .is_some_and(|(a, b)| a.id == b.id && a.attributes & 0x410 == b.attributes & 0x410)
            {
                return patch_tree(index, previous, projection, changes, cancelled);
            }
            let Some(Item::Folder(row)) = find(relative) else {
                return Ok(None);
            };
            edits.push((relative, Some(Item::Folder(row)), row, old, new));
        } else {
            let Some(Item::Folder(parent)) = relative.parent().and_then(find) else {
                return Ok(None);
            };
            let item = find(relative);
            if structural && new.is_some() && changes.len() <= 256 {
                changed_paths.insert(relative.to_owned());
            }
            edits.push((relative, item, parent, old, new));
        }
    }
    let folder_rows_changed = !appended.is_empty() || !removed_folders.is_empty();
    let structural = folder_rows_changed
        || structural
            && edits.iter().any(|(_, item, _, old, new)| {
                !matches!(item, Some(Item::Folder(_)))
                    && old.map(|node| node.id) != new.map(|node| node.id)
            });

    // 작은 일반 파일 묶음은 무관한 파일 행을 순회하지 않고 바뀐 행만 교체.
    let mut removed = Vec::new();
    let mut added = Vec::new();
    let local_change = if structural && changes.len() <= MAX_PATCH_CHANGES {
        for &(path, item, parent, old, new) in &edits {
            if matches!(item, Some(Item::Folder(_))) || old.map(|n| n.id) == new.map(|n| n.id) {
                continue;
            }
            if let Some(Item::File(row)) = item {
                removed.push((row, path));
            }
            if let Some(node) = new {
                let key = index.nodes.key_number(path).expect("추가한 파일의 고정 ID");
                added.push((path, parent, node, key));
            }
        }
        removed.sort_unstable_by_key(|&(row, _)| std::cmp::Reverse(row));
        projection.change_files(
            removed.iter().copied(),
            added.iter().map(|&(path, _, _, key)| (path, key)),
            appended.iter().map(|&(path, _, key)| (path, key)),
        )
    } else {
        None
    };
    if folder_rows_changed && local_change.is_none() {
        return patch_tree(index, previous, projection, changes, cancelled);
    }

    let mut result = previous.copy_for_update(!structural || local_change.is_some());
    let mut affected = HashSet::new();
    let mut file_parents = HashSet::new();
    let mut folder_parents = HashSet::new();
    let mut folder_ids = folder_rows_changed.then(|| index.checkpoint.folders.clone());
    for (path, node, _) in appended {
        crate::output::check(cancelled)?;
        let Some(Item::Folder(parent)) = path.parent().and_then(find) else {
            return Ok(None);
        };
        let row = result.folders.len();
        if parent >= row {
            return Ok(None);
        }
        folder_ids.as_mut().unwrap().push(node.id);
        result.folders.push(crate::scanner::Folder {
            access: Default::default(),
            path: index.root.join(path),
            parent: Some(parent),
            logical_bytes: 0,
            allocation: Allocation::default(),
            file_count: 0,
            folder_count: 0,
            created: node.created,
            modified: node.modified,
            attributes: node.attributes,
        });
        result.children.push(Arc::default());
        result.file_children.push(Arc::default());
        Arc::make_mut(&mut result.children[parent]).push(row);
        folder_parents.insert(parent);
        mark_ancestors(&result, &mut affected, row);
        let mut ancestor = Some(parent);
        while let Some(id) = ancestor {
            let folder = &mut result.folders[id];
            folder.folder_count += 1;
            ancestor = folder.parent;
        }
    }
    // 감소를 먼저 반영하여 같은 배치의 증가분으로 인한 중간 합계 초과 방지.
    for add in [false, true] {
        for &(_, item, parent, old, new) in &edits {
            crate::output::check(cancelled)?;
            if let Some(Item::Folder(row)) = item {
                if add {
                    let n = new.unwrap();
                    let folder = &mut result.folders[row];
                    folder.created = n.created;
                    folder.modified = n.modified;
                    folder.attributes = n.attributes;
                    mark_ancestors(&result, &mut affected, row);
                }
                continue;
            }
            let resized = old.map(|n| n.bytes) != new.map(|n| n.bytes);
            let allocation_changed =
                old.map(|n| n.extra.allocation) != new.map(|n| n.extra.allocation);
            if add {
                mark_ancestors(&result, &mut affected, parent);
                if resized || structural {
                    file_parents.insert(parent);
                }
                if !structural {
                    let Some(Item::File(row)) = item else {
                        unreachable!()
                    };
                    let n = new.unwrap();
                    let file = &mut result.files[row];
                    file.logical_bytes = n.bytes;
                    file.extra = n.extra;
                    file.created = n.created;
                    file.modified = n.modified;
                    file.attributes = n.attributes;
                }
            }
            if !resized && !allocation_changed {
                continue;
            }
            let Some(n) = (if add { new } else { old }) else {
                continue;
            };
            let mut ancestor = Some(parent);
            while let Some(row) = ancestor {
                let folder = &mut result.folders[row];
                apply_file_total(folder, n, add)?;
                if resized {
                    if let Some(p) = folder.parent {
                        folder_parents.insert(p);
                    }
                }
                ancestor = folder.parent;
            }
        }
    }
    let mut new_files = Vec::new();

    let mut file_rows = None;
    let used_local_change = local_change.is_some();
    let mut rows = if let Some((next, remap)) = local_change {
        let mut ids = index.checkpoint.files.clone();
        for (removed, _) in removed {
            crate::output::check(cancelled)?;
            let parent = result.files[removed].parent;
            let last = result.files.len() - 1;
            let moved_parent = result.files[last].parent;
            ids.swap_remove(removed);
            result.files.swap_remove(removed);
            Arc::make_mut(&mut result.file_children[parent]).retain(|&row| row != removed);
            if removed != last {
                let list = Arc::make_mut(&mut result.file_children[moved_parent]);
                *list
                    .iter_mut()
                    .find(|row| **row == last)
                    .expect("이동한 파일의 부모 목록") = removed;
                mark_ancestors(&result, &mut affected, moved_parent);
            }
        }
        for (path, parent, node, _) in added {
            crate::output::check(cancelled)?;
            let row = result.files.len();
            ids.push(node.id);
            result.files.push(file_entry(path, parent, node));
            Arc::make_mut(&mut result.file_children[parent]).push(row);
            new_files.push(Item::File(row));
        }
        for &(path, item, parent, old, new) in &edits {
            if let (Some(Item::File(row)), Some(old), Some(new)) = (item, old, new) {
                if old.id == new.id {
                    crate::output::check(cancelled)?;
                    let row = remap.get(row).expect("수정한 파일의 위치");
                    result.files[row] = file_entry(path, parent, new);
                    new_files.push(Item::File(row));
                }
            }
        }
        file_rows = Some(remap);
        Some((next, ids, folder_ids))
    } else if structural {
        let mut files = RowProjection::new(&previous.files, |(path, parent, node)| {
            file_entry(path, parent, node)
        });
        let mut ids = Vec::with_capacity(previous.files.len());
        let mut keys = Vec::with_capacity(previous.files.len());
        let mut lists = vec![Vec::new(); result.folders.len()];
        for (path, key, node) in index.nodes.identified() {
            crate::output::check(cancelled)?;
            if node.dir() || node.excluded() {
                continue;
            }
            let Some(Item::Folder(parent)) = path.parent().and_then(find) else {
                return Ok(None);
            };
            let row = keys.len();
            if !projection.has_file_key(row, key) {
                file_parents.insert(parent);
                mark_ancestors(&result, &mut affected, parent);
            }
            if changed_paths.contains(path) {
                new_files.push(Item::File(row));
            }
            lists[parent].push(row);
            files.push(
                (path, parent, node),
                projection.has_file_key(row, key)
                    && previous.files[row].parent == parent
                    && index.nodes.original(path) == Some(node),
            );
            ids.push(node.id);
            keys.push(key);
        }
        for (parent, list) in lists.into_iter().enumerate() {
            if file_parents.contains(&parent) {
                result.file_children[parent] = Arc::new(list);
            }
        }
        result.files = files.finish();
        Some((
            index.nodes.project_update(
                projection.folder_keys().to_vec(),
                keys,
                projection,
                cancelled,
            )?,
            ids,
            None,
        ))
    } else {
        None
    };
    for parent in file_parents {
        crate::output::check(cancelled)?;
        Arc::make_mut(&mut result.file_children[parent]).sort_unstable_by(|&a, &b| {
            result.files[b]
                .logical_bytes
                .cmp(&result.files[a].logical_bytes)
                .then_with(|| result.files[a].name.cmp(&result.files[b].name))
        });
    }
    for parent in folder_parents {
        crate::output::check(cancelled)?;
        Arc::make_mut(&mut result.children[parent]).sort_unstable_by(|&a, &b| {
            result.folders[b]
                .logical_bytes
                .cmp(&result.folders[a].logical_bytes)
                .then_with(|| result.folders[a].path.cmp(&result.folders[b].path))
        });
    }
    if !access_changes.is_empty()
        && !update_access(
            index,
            &mut result,
            &mut affected,
            &access_changes,
            find,
            cancelled,
        )?
    {
        return Ok(None);
    }
    let mut search_before = (changes.len() <= 256).then(|| {
        affected
            .iter()
            .copied()
            .map(Item::Folder)
            .collect::<Vec<_>>()
    });
    let mut search_after = search_before.clone();
    let mut folder_rows = None;
    if !removed_folders.is_empty() {
        removed_folders.sort_unstable_by_key(|&(row, _)| std::cmp::Reverse(row));
        let Some((next, _, Some(ids))) = rows.as_mut() else {
            unreachable!("폴더 행과 기준점 필요");
        };
        let Some((positions, moves)) = compact_folder_rows(
            &mut result,
            ids,
            &removed_folders,
            &index.root,
            find,
            &mut affected,
            cancelled,
        )?
        else {
            return patch_tree(index, previous, projection, changes, cancelled);
        };
        next.compact_folders(&removed_folders, &moves);
        if let Some(items) = search_after.as_mut() {
            items.retain_mut(|item| {
                let Item::Folder(old) = *item else {
                    unreachable!()
                };
                let Some(row) = positions.get(&old).copied().unwrap_or(Some(old)) else {
                    return false;
                };
                *item = Item::Folder(row);
                true
            });
        }
        folder_rows = Some(crate::scanner::RowRemap::with_changes(
            previous.folders.len(),
            positions
                .into_iter()
                .filter(|(old, _)| *old < previous.folders.len())
                .collect(),
        ));
        if let Some(folders) = search_before.as_mut() {
            folders.extend(removed_folders.iter().map(|(row, _)| Item::Folder(*row)));
        }
    }
    if !used_local_change {
        if let (Some(_), Some((next, _, _))) = (&search_before, &rows) {
            file_rows = Some(remap_rows(
                previous.files.len(),
                Item::File,
                projection,
                next,
                cancelled,
            )?);
        }
    }
    let search = search_before.map(|mut before| {
        let mut after = search_after.unwrap();
        before.retain(|item| !matches!(item, Item::Folder(row) if *row >= previous.folders.len()));
        before.extend(edits.iter().filter_map(|(_, item, _, old, _)| {
            old.and(item.filter(|item| matches!(item, Item::File(_))))
        }));
        if structural {
            after.extend(new_files);
        } else {
            after = before.clone();
        }
        crate::scanner::SearchDelta {
            before,
            after,
            file_rows,
            folder_rows,
        }
    });
    result.partial = Some(Arc::new(PartialUpdate {
        previous: Arc::downgrade(previous),
        affected,
        search,
    }));
    let excluded = if excluded_changes.is_empty() {
        None
    } else {
        let updated = update_excluded(&index.checkpoint.excluded, excluded_changes, cancelled)?;
        result.skipped_reparse_points = updated.len() as u64;
        Some(updated)
    };
    Ok(Some(ScanPatch {
        scan: result,
        rows,
        excluded,
    }))
}

type FolderPositions = BTreeMap<usize, Option<usize>>;
type FolderMoves = Vec<(usize, usize)>;

fn compact_folder_rows(
    scan: &mut ScanResult,
    ids: &mut Vec<u64>,
    removed: &[(usize, &Path)],
    root: &Path,
    find: impl Fn(&Path) -> Option<Item>,
    affected: &mut HashSet<usize>,
    cancelled: &impl Fn() -> bool,
) -> io::Result<Option<(FolderPositions, FolderMoves)>> {
    let mut positions = BTreeMap::new();
    let mut moves = Vec::new();
    let mut relocated = HashSet::new();
    for &(removed, _) in removed {
        crate::output::check(cancelled)?;
        if !scan.children[removed].is_empty() || !scan.file_children[removed].is_empty() {
            return Ok(None);
        }
        positions.insert(removed, None);
        let parent = scan.folders[removed].parent.unwrap();
        Arc::make_mut(&mut scan.children[parent]).retain(|&row| row != removed);
        mark_ancestors(scan, affected, parent);
        let mut ancestor = Some(parent);
        while let Some(row) = ancestor {
            let folder = &mut scan.folders[row];
            folder.folder_count -= 1;
            ancestor = folder.parent;
        }

        let last = scan.folders.len() - 1;
        let mut empty = removed;
        while empty < last {
            crate::output::check(cancelled)?;
            let mut from = last;
            // 부모가 빈 행보다 앞서는 후보를 찾아 부모 우선 순서 유지.
            while let Some(parent) = scan.folders[from].parent {
                if parent < empty {
                    break;
                }
                from = parent;
            }
            if from <= empty {
                return Ok(None);
            }
            let folder = scan.folders[from].clone();
            let path = folder.path.strip_prefix(root).expect("추적 범위 폴더");
            let Some(Item::Folder(original)) = find(path) else {
                return Ok(None);
            };
            positions.insert(original, Some(empty));
            relocated.insert(original);
            moves.push((from, empty));
            let parent = folder.parent.unwrap();
            let siblings = Arc::make_mut(&mut scan.children[parent]);
            *siblings
                .iter_mut()
                .find(|row| **row == from)
                .expect("이동하는 폴더 행") = empty;
            scan.folders[empty] = folder;
            scan.children[empty] = Arc::clone(&scan.children[from]);
            scan.file_children[empty] = Arc::clone(&scan.file_children[from]);
            ids[empty] = ids[from];
            for &child in scan.children[empty].iter() {
                crate::output::check(cancelled)?;
                scan.folders[child].parent = Some(empty);
            }
            for &file in scan.file_children[empty].iter() {
                crate::output::check(cancelled)?;
                scan.files[file].parent = empty;
            }
            empty = from;
        }
        scan.folders.swap_remove(last);
        scan.children.pop();
        scan.file_children.pop();
        ids.pop();
    }
    *affected = std::mem::take(affected)
        .into_iter()
        .filter_map(|old| positions.get(&old).copied().unwrap_or(Some(old)))
        .collect();
    for original in relocated {
        if let Some(Some(row)) = positions.get(&original) {
            mark_ancestors(scan, affected, *row);
            affected.extend(scan.children[*row].iter().copied());
        }
    }
    Ok(Some((positions, moves)))
}

fn update_excluded(
    previous: &[Excluded],
    changes: BTreeMap<&Path, Option<&Node>>,
    cancelled: &impl Fn() -> bool,
) -> io::Result<Vec<Excluded>> {
    let mut entries = BTreeMap::new();
    for entry in previous {
        crate::output::check(cancelled)?;
        let path = PathBuf::from(std::ffi::OsString::from_wide(&entry.path));
        if !changes.contains_key(path.as_path()) {
            entries.insert(path, entry.clone());
        }
    }
    for (path, node) in changes {
        crate::output::check(cancelled)?;
        if let Some(node) = node {
            entries.insert(
                path.to_path_buf(),
                Excluded {
                    path: path.as_os_str().encode_wide().collect(),
                    id: node.id,
                    attributes: node.attributes,
                },
            );
        }
    }
    Ok(entries.into_values().collect())
}

fn update_access(
    index: &Index,
    scan: &mut ScanResult,
    affected: &mut HashSet<usize>,
    paths: &[&Path],
    find: impl Fn(&Path) -> Option<Item>,
    cancelled: &impl Fn() -> bool,
) -> io::Result<bool> {
    use crate::scanner::{FolderAccess, ScanIssue};
    let mut access = std::collections::HashMap::new();
    scan.error_count = index.unavailable.len() as u64;
    scan.issues.clear();
    for (path, issue) in &index.unavailable {
        crate::output::check(cancelled)?;
        let Some(Item::Folder(parent)) = path.parent().and_then(&find) else {
            return Ok(false);
        };
        let mut row = Some(parent);
        while let Some(id) = row {
            access.entry(id).or_insert(FolderAccess::Partial);
            row = scan.folders[id].parent;
        }
        if issue.listing {
            let Some(Item::Folder(row)) = find(path) else {
                return Ok(false);
            };
            access.insert(
                row,
                if issue.reason == super::UnavailableReason::Os(5) {
                    FolderAccess::Denied
                } else {
                    FolderAccess::Unavailable
                },
            );
        }
        if scan.issues.len() < 20 {
            scan.issues.push(ScanIssue {
                path: index.root.join(path),
                error: issue.reason.error(),
            });
        }
    }
    for path in paths {
        crate::output::check(cancelled)?;
        let Some(Item::Folder(parent)) = path.parent().and_then(&find) else {
            return Ok(false);
        };
        let mut row = match find(path) {
            Some(Item::Folder(row)) => Some(row),
            _ => Some(parent),
        };
        while let Some(id) = row {
            let folder = &mut scan.folders[id];
            folder.access = access.get(&id).copied().unwrap_or_default();
            row = folder.parent;
            affected.insert(id);
        }
    }
    Ok(true)
}

fn mark_ancestors(scan: &ScanResult, affected: &mut HashSet<usize>, mut folder: usize) {
    while affected.insert(folder) {
        let Some(parent) = scan.folders[folder].parent else {
            break;
        };
        folder = parent;
    }
}

// 새 폴더 행에 이전 경로의 집계를 이어 붙이고 변경 경로의 기여분만 보정.
fn patch_tree(
    index: &Index,
    previous: &Arc<ScanResult>,
    projection: &super::entries::Projection,
    changes: &[Change],
    cancelled: &impl Fn() -> bool,
) -> io::Result<Option<ScanPatch>> {
    if !index.unavailable.is_empty() || previous.error_count != 0 {
        return Ok(None);
    }
    let mut delta = Vec::with_capacity(changes.len());
    for change in changes {
        crate::output::check(cancelled)?;
        let Change::Node { path, value, .. } = change else {
            return Ok(None);
        };
        let old = index.nodes.original(path);
        let new = value.as_ref();
        if old.into_iter().chain(new).any(|n| n.excluded())
            || old.and_then(|n| n.issue.as_ref()) != new.and_then(|n| n.issue.as_ref())
        {
            return Ok(None);
        }
        delta.push((path.as_path(), old, new));
    }

    let mut scan = previous.copy_for_update(false);
    scan.folders.clear();
    let mut paths = std::collections::HashMap::new();
    let mut folder_ids = Vec::new();
    let mut folder_keys = Vec::new();
    let mut affected = HashSet::new();
    let mut dirs: Vec<_> = index
        .nodes
        .identified()
        .filter(|(_, _, n)| n.dir() && !n.excluded())
        .collect();
    dirs.sort_by_key(|(p, _, _)| p.components().count());
    let mut folder_rows = RowProjection::new(
        &previous.folders,
        |(path, parent, n, before): (
            &Path,
            Option<usize>,
            &super::Node,
            Option<&crate::scanner::Folder>,
        )| {
            crate::scanner::Folder {
                access: Default::default(),
                path: index.root.join(path),
                parent,
                logical_bytes: before.map_or(0, |f| f.logical_bytes),
                allocation: before.map_or(Allocation::default(), |f| f.allocation),
                file_count: before.map_or(0, |f| f.file_count),
                folder_count: before.map_or(0, |f| f.folder_count),
                created: n.created,
                modified: n.modified,
                attributes: n.attributes,
            }
        },
    );
    for (path, key, n) in dirs {
        crate::output::check(cancelled)?;
        let parent = path.parent().map(|p| paths[p]);
        let old_row = projection
            .path_key(path)
            .and_then(|key| projection.item(key))
            .and_then(|item| {
                if let Item::Folder(row) = item {
                    Some(row)
                } else {
                    None
                }
            });
        let before = old_row.map(|row| &previous.folders[row]);
        let row = folder_keys.len();
        paths.insert(path, row);
        folder_keys.push(key);
        folder_ids.push(n.id);
        let same = old_row == Some(row)
            && before.is_some_and(|f| {
                f.parent == parent
                    && f.attributes == n.attributes
                    && f.created == n.created
                    && f.modified == n.modified
            });
        folder_rows.push((path, parent, n, before), same);
    }
    scan.folders = folder_rows.finish();
    for (&path, &row) in &paths {
        let unchanged_row = projection
            .path_key(path)
            .and_then(|key| projection.item(key))
            == Some(Item::Folder(row));
        if !unchanged_row {
            mark_ancestors(&scan, &mut affected, row);
        }
    }
    for (path, _, _) in &delta {
        if let Some(&row) = path.ancestors().find_map(|p| paths.get(p)) {
            mark_ancestors(&scan, &mut affected, row);
        }
    }
    for add in [false, true] {
        for (path, old, new) in &delta {
            crate::output::check(cancelled)?;
            if old.zip(*new).is_some_and(|(before, after)| {
                before.dir() == after.dir()
                    && (before.dir()
                        || before.bytes == after.bytes
                            && before.extra.allocation == after.extra.allocation)
            }) {
                continue;
            }
            let Some(n) = (if add { new } else { old }) else {
                continue;
            };
            for ancestor in path.ancestors().skip(1) {
                let Some(&row) = paths.get(ancestor) else {
                    continue;
                };
                if !add
                    && !projection
                        .path_key(ancestor)
                        .and_then(|k| projection.item(k))
                        .is_some_and(|item| matches!(item, Item::Folder(_)))
                {
                    continue;
                }
                let folder = &mut scan.folders[row];
                if n.dir() {
                    if add {
                        folder.folder_count += 1;
                    } else {
                        folder.folder_count -= 1;
                    }
                } else {
                    apply_file_total(folder, n, add)?;
                }
            }
        }
    }
    let mut file_ids = Vec::with_capacity(previous.files.len());
    let mut file_keys = Vec::with_capacity(previous.files.len());
    let mut files = RowProjection::new(&previous.files, |(path, parent, node)| {
        file_entry(path, parent, node)
    });
    for (path, key, n) in index.nodes.identified() {
        crate::output::check(cancelled)?;
        if n.dir() || n.excluded() {
            continue;
        }
        let parent = paths[path.parent().unwrap()];
        let row = file_keys.len();
        let same_key = projection.has_file_key(row, key);
        if !same_key {
            mark_ancestors(&scan, &mut affected, parent);
        }
        files.push(
            (path, parent, n),
            same_key
                && previous.files[row].parent == parent
                && index.nodes.original(path) == Some(n),
        );
        file_keys.push(key);
        file_ids.push(n.id);
    }
    scan.files = files.finish();
    crate::scanner::finish_lists(&mut scan);
    let next = index
        .nodes
        .project_update(folder_keys, file_keys, projection, cancelled)?;
    let search = if delta.len() <= 256 {
        tree_search_delta(previous, projection, &next, &delta, cancelled)?
    } else {
        None
    };
    scan.partial = Some(Arc::new(PartialUpdate {
        previous: Arc::downgrade(previous),
        affected,
        search,
    }));
    Ok(Some(ScanPatch {
        scan,
        rows: Some((next, file_ids, Some(folder_ids))),
        excluded: None,
    }))
}

type TreeChange<'a> = (&'a Path, Option<&'a super::Node>, Option<&'a super::Node>);

fn tree_search_delta(
    previous: &ScanResult,
    before: &super::entries::Projection,
    after: &super::entries::Projection,
    delta: &[TreeChange<'_>],
    cancelled: &impl Fn() -> bool,
) -> io::Result<Option<crate::scanner::SearchDelta>> {
    let changed = |projection: &super::entries::Projection| {
        delta
            .iter()
            .flat_map(|(path, _, _)| path.ancestors())
            .filter_map(|path| {
                projection
                    .path_key(path)
                    .and_then(|key| projection.item(key))
            })
            .collect::<HashSet<_>>()
    };
    let old_items = changed(before);
    let new_items = changed(after);
    if old_items.len().max(new_items.len()) > 256 {
        return Ok(None);
    }
    Ok(Some(crate::scanner::SearchDelta {
        before: old_items.into_iter().collect(),
        after: new_items.into_iter().collect(),
        file_rows: Some(remap_rows(
            previous.files.len(),
            Item::File,
            before,
            after,
            cancelled,
        )?),
        folder_rows: Some(remap_rows(
            previous.folders.len(),
            Item::Folder,
            before,
            after,
            cancelled,
        )?),
    }))
}

fn remap_rows(
    len: usize,
    make: fn(usize) -> Item,
    before: &super::entries::Projection,
    after: &super::entries::Projection,
    cancelled: &impl Fn() -> bool,
) -> io::Result<crate::scanner::RowRemap> {
    let mut result = crate::scanner::RowRemap::default();
    for row in 0..len {
        crate::output::check(cancelled)?;
        if let Some(Item::File(new) | Item::Folder(new)) =
            before.key(make(row)).and_then(|key| after.item(key))
        {
            result.push_ordered(row, new);
        }
    }
    Ok(result)
}

fn apply_file_total(
    folder: &mut crate::scanner::Folder,
    n: &super::Node,
    add: bool,
) -> io::Result<()> {
    let mut allocation = Allocation::default();
    allocation.add(n.extra.allocation)?;
    if add {
        folder.logical_bytes = folder
            .logical_bytes
            .checked_add(n.bytes)
            .ok_or_else(|| io::Error::other("용량 합계 초과"))?;
        folder.allocation.merge(allocation)?;
        folder.file_count += 1;
    } else {
        folder.logical_bytes -= n.bytes;
        folder.allocation.bytes -= allocation.bytes;
        folder.allocation.known -= allocation.known;
        folder.allocation.not_collected -= allocation.not_collected;
        folder.allocation.unsupported -= allocation.unsupported;
        folder.allocation.failed -= allocation.failed;
        folder.file_count -= 1;
    }
    Ok(())
}

// 행 번호와 내용이 그대로인 묶음은 이전 결과의 저장소를 그대로 재사용.
struct RowProjection<'a, T, R, F> {
    previous: &'a crate::scanner::Rows<T>,
    output: crate::scanner::Rows<T>,
    pending: Vec<R>,
    build: F,
    all_same: bool,
}
impl<'a, T: Clone, R, F: FnMut(R) -> T> RowProjection<'a, T, R, F> {
    fn new(previous: &'a crate::scanner::Rows<T>, build: F) -> Self {
        Self {
            previous,
            output: Default::default(),
            pending: Vec::new(),
            build,
            all_same: true,
        }
    }
    fn push(&mut self, row: R, same: bool) {
        self.pending.push(row);
        self.all_same &= same;
        if self.pending.len() == crate::scanner::Rows::<T>::chunk_size() {
            self.flush();
        }
    }
    fn flush(&mut self) {
        if self.pending.is_empty() {
            return;
        }
        if self.all_same
            && self
                .output
                .append_reused_chunk(self.previous, self.pending.len())
        {
            self.pending.clear();
        } else {
            for row in self.pending.drain(..) {
                self.output.push((self.build)(row));
            }
        }
        self.all_same = true;
    }
    fn finish(mut self) -> crate::scanner::Rows<T> {
        self.flush();
        self.output
    }
}

fn file_entry(path: &Path, parent: usize, n: &super::Node) -> crate::scanner::FileEntry {
    crate::scanner::FileEntry {
        name: path.file_name().unwrap().to_owned(),
        parent,
        logical_bytes: n.bytes,
        extra: n.extra,
        attributes: n.attributes,
        created: n.created,
        modified: n.modified,
    }
}
