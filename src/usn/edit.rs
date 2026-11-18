use super::{stream::Change, Index, Unavailable};
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
    pub fn retain(
        &mut self,
        mut keep: impl FnMut(&PathBuf, &Unavailable) -> bool,
    ) -> io::Result<()> {
        let undo = &mut self.undo;
        let mut failure = None;
        self.values.retain(|path, value| {
            if failure.is_some() || keep(path, value) {
                return true;
            }
            if let Err(error) = Self::record(undo, path, Some(value)) {
                failure = Some(error);
                return true;
            }
            false
        });
        failure.map_or(Ok(()), Err)
    }
    pub fn changes(&self) -> impl Iterator<Item = Change> + '_ {
        self.undo
            .as_ref()
            .into_iter()
            .flat_map(|undo| &undo.before)
            .filter_map(|(path, old)| {
                let new = self.values.get(path);
                (old.as_ref() != new).then(|| Change::Unavailable {
                    path: path.as_os_str().encode_wide().collect(),
                    present: old.is_some(),
                    value: new.cloned(),
                })
            })
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
        for change in self
            .index
            .nodes
            .changes()
            .chain(self.index.unavailable.changes())
        {
            crate::output::check(cancelled)?;
            changed_entries += 1;
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
        crate::output::check(cancelled)?;
        self.index.nodes.finish_edit();
        self.index.unavailable.undo = None;
        self.index.changes = if emit_changes { changes } else { vec![] };
        self.index.last_edit = stats;
        if changed {
            self.index.scan = patched.map(|patch| {
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
}

fn patch_scan(
    index: &Index,
    changes: &[Change],
    structural: bool,
    cancelled: &impl Fn() -> bool,
) -> io::Result<Option<ScanPatch>> {
    let (Some(previous), Some(projection)) = (&index.scan, &index.projection) else {
        return Ok(None);
    };
    let find = |path: &Path| {
        projection
            .path_key(path)
            .and_then(|key| projection.item(key))
    };
    let mut edits = Vec::with_capacity(changes.len());
    let mut changed_paths = HashSet::new();
    for change in changes {
        crate::output::check(cancelled)?;
        let Change::Node { path, value, .. } = change else {
            return Ok(None);
        };
        let relative = PathBuf::from(std::ffi::OsString::from_wide(path));
        let old = index.nodes.original(&relative);
        let new = value.as_ref();
        if old.and_then(|n| n.issue.as_ref()) != new.and_then(|n| n.issue.as_ref()) {
            return Ok(None);
        }
        if old.into_iter().chain(new).any(|n| n.dir() || n.excluded()) {
            if !old
                .zip(new)
                .is_some_and(|(a, b)| a.id == b.id && a.attributes & 0x410 == b.attributes & 0x410)
            {
                return patch_tree(index, previous, projection, changes, cancelled);
            }
            if old.is_some_and(super::Node::excluded) {
                continue;
            }
            let Some(Item::Folder(row)) = find(&relative) else {
                return Ok(None);
            };
            edits.push((Some(Item::Folder(row)), row, old, new));
        } else {
            let Some(Item::Folder(parent)) = relative.parent().and_then(find) else {
                return Ok(None);
            };
            let item = find(&relative);
            if structural && new.is_some() && changes.len() <= 256 {
                changed_paths.insert(relative);
            }
            edits.push((item, parent, old, new));
        }
    }

    let mut result = previous.copy_for_update(!structural);
    let mut affected = HashSet::new();
    let mut file_parents = HashSet::new();
    let mut folder_parents = HashSet::new();
    // 감소를 먼저 반영하여 같은 배치의 증가분으로 인한 중간 합계 초과 방지.
    for add in [false, true] {
        for &(item, parent, old, new) in &edits {
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
    let search_folders = (changes.len() <= 256).then(|| {
        affected
            .iter()
            .copied()
            .map(Item::Folder)
            .collect::<Vec<_>>()
    });
    let mut new_files = Vec::new();
    let mut file_rows =
        (structural && changes.len() <= 256).then(|| vec![usize::MAX; previous.files.len()]);
    let rows = if structural {
        let mut files = crate::scanner::FileRows::default();
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
            let row = files.len();
            if !projection.has_file_key(row, key) {
                file_parents.insert(parent);
                mark_ancestors(&result, &mut affected, parent);
            }
            if let Some(remap) = &mut file_rows {
                if let Some(Item::File(old)) = find(path) {
                    remap[old] = row;
                }
                if changed_paths.contains(path) {
                    new_files.push(Item::File(row));
                }
            }
            lists[parent].push(row);
            files.push(file_entry(path, parent, node));
            ids.push(node.id);
            keys.push(key);
        }
        for (parent, list) in lists.into_iter().enumerate() {
            if file_parents.contains(&parent) {
                result.file_children[parent] = Arc::new(list);
            }
        }
        result.files = files;
        Some((
            index
                .nodes
                .project(projection.folder_keys().to_vec(), keys, cancelled)?,
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
    let search = search_folders.map(|mut before| {
        let mut after = before.clone();
        before.extend(edits.iter().filter_map(|(item, _, old, _)| {
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
        }
    });
    result.partial = Some(Arc::new(PartialUpdate {
        previous: Arc::downgrade(previous),
        affected,
        search,
    }));
    Ok(Some(ScanPatch { scan: result, rows }))
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
        let path = PathBuf::from(std::ffi::OsString::from_wide(path));
        let old = index.nodes.original(&path);
        let new = value.as_ref();
        if old.into_iter().chain(new).any(|n| n.excluded())
            || old.and_then(|n| n.issue.as_ref()) != new.and_then(|n| n.issue.as_ref())
        {
            return Ok(None);
        }
        delta.push((path, old, new));
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
    for (path, key, n) in dirs {
        crate::output::check(cancelled)?;
        let parent = path.parent().map(|p| paths[p]);
        let before = projection
            .path_key(path)
            .and_then(|key| projection.item(key))
            .and_then(|item| {
                if let Item::Folder(row) = item {
                    Some(&previous.folders[row])
                } else {
                    None
                }
            });
        paths.insert(path.as_path(), scan.folders.len());
        folder_keys.push(key);
        folder_ids.push(n.id);
        scan.folders.push(crate::scanner::Folder {
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
        });
    }
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
    for (path, key, n) in index.nodes.identified() {
        crate::output::check(cancelled)?;
        if n.dir() || n.excluded() {
            continue;
        }
        let parent = paths[path.parent().unwrap()];
        if !projection.has_file_key(scan.files.len(), key) {
            mark_ancestors(&scan, &mut affected, parent);
        }
        scan.files.push(file_entry(path, parent, n));
        file_keys.push(key);
        file_ids.push(n.id);
    }
    crate::scanner::finish_lists(&mut scan);
    let projection = index.nodes.project(folder_keys, file_keys, cancelled)?;
    scan.partial = Some(Arc::new(PartialUpdate {
        previous: Arc::downgrade(previous),
        affected,
        search: None,
    }));
    Ok(Some(ScanPatch {
        scan,
        rows: Some((projection, file_ids, Some(folder_ids))),
    }))
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
