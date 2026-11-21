use super::{edit::Budget, edit::Change, paths, Node};
use crate::{
    explorer::Item,
    scanner::{RowRemap, ScanResult},
};
use std::io;
use std::{
    collections::{hash_map, BTreeMap, HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Key {
    scope: u64,
    number: u64,
}

/// 같은 추적 수명의 항목 ID와 화면용 배열 번호 대응.
pub struct Projection {
    scope: u64,
    paths: Arc<PathIndex>,
    folders: Arc<[u64]>,
    files: Vec<u64>,
    items: Arc<ItemIndex>,
}
// 완료 화면이 같은 경로표를 공유하고 바뀐 경로만 별도로 보관.
enum PathIndex {
    Full(paths::Map<u64>),
    Changes {
        parent: Arc<PathIndex>,
        depth: usize,
        paths: BTreeMap<Arc<Path>, Option<u64>>,
    },
}
impl PathIndex {
    fn get(&self, path: &Path) -> Option<u64> {
        match self {
            Self::Full(paths) => paths.get(path).copied(),
            Self::Changes { parent, paths, .. } => {
                paths.get(path).copied().unwrap_or_else(|| parent.get(path))
            }
        }
    }
    fn depth(&self) -> usize {
        match self {
            Self::Full(_) => 0,
            Self::Changes { depth, .. } => *depth,
        }
    }
}
enum ItemIndex {
    Full(HashMap<u64, Item>),
    Changed {
        parent: Arc<Self>,
        depth: usize,
        items: HashMap<u64, Option<Item>>,
    },
}
impl ItemIndex {
    fn get(&self, number: u64) -> Option<Item> {
        match self {
            Self::Full(items) => items.get(&number).copied(),
            Self::Changed { parent, items, .. } => items
                .get(&number)
                .copied()
                .unwrap_or_else(|| parent.get(number)),
        }
    }
    fn depth(&self) -> usize {
        match self {
            Self::Full(_) => 0,
            Self::Changed { depth, .. } => *depth,
        }
    }
}
impl Projection {
    pub(super) fn has_file_key(&self, row: usize, key: u64) -> bool {
        self.files.get(row) == Some(&key)
    }
    pub(super) fn folder_keys(&self) -> &[u64] {
        &self.folders
    }

    pub(super) fn matches_rows(&self, folders: &[u64], files: &[u64]) -> bool {
        self.folders.as_ref() == folders && self.files == files
    }
    pub fn same_scope(&self, key: Key) -> bool {
        self.scope == key.scope
    }
    pub fn key(&self, item: Item) -> Option<Key> {
        let number = match item {
            Item::Folder(row) => self.folders.get(row),
            Item::File(row) => self.files.get(row),
        }?;
        Some(Key {
            scope: self.scope,
            number: *number,
        })
    }
    pub fn item(&self, key: Key) -> Option<Item> {
        (key.scope == self.scope)
            .then(|| self.items.get(key.number))
            .flatten()
    }
    pub(super) fn change_files<'a>(
        &self,
        removed: impl Iterator<Item = (usize, &'a Path)>,
        added: impl Iterator<Item = (&'a Path, u64)>,
        appended_folders: impl Iterator<Item = (&'a Path, u64)>,
    ) -> Option<(Self, RowRemap)> {
        if self.paths.depth() >= 8 || self.items.depth() >= 8 {
            return None;
        }
        let mut files = self.files.clone();
        let mut items = HashMap::new();
        let mut paths = BTreeMap::new();
        let mut old_rows = BTreeMap::new();
        // 역순으로 삭제해 아직 삭제할 원래 행의 위치 유지.
        for (row, path) in removed {
            let last = files.len() - 1;
            items.insert(files.swap_remove(row), None);
            old_rows.insert(row, None);
            paths.insert(Arc::from(path), None);
            if row != last {
                let key = files[row];
                let Some(Item::File(old)) = self.items.get(key) else {
                    unreachable!("이동하는 파일 행")
                };
                old_rows.insert(old, Some(row));
                items.insert(key, Some(Item::File(row)));
            }
        }
        for (path, key) in added {
            items.insert(key, Some(Item::File(files.len())));
            files.push(key);
            paths.insert(Arc::from(path), Some(key));
        }
        let folders: Arc<[u64]> = if appended_folders.size_hint().1 == Some(0) {
            Arc::clone(&self.folders)
        } else {
            let mut folders = self.folders.to_vec();
            for (path, key) in appended_folders {
                items.insert(key, Some(Item::Folder(folders.len())));
                folders.push(key);
                paths.insert(Arc::from(path), Some(key));
            }
            folders.into()
        };
        Some((
            Self {
                scope: self.scope,
                paths: Arc::new(PathIndex::Changes {
                    parent: Arc::clone(&self.paths),
                    depth: self.paths.depth() + 1,
                    paths,
                }),
                folders,
                files,
                items: Arc::new(ItemIndex::Changed {
                    parent: Arc::clone(&self.items),
                    depth: self.items.depth() + 1,
                    items,
                }),
            },
            RowRemap::with_changes(self.files.len(), old_rows),
        ))
    }

    pub(super) fn compact_folders(&mut self, removed: &[(usize, &Path)], moves: &[(usize, usize)]) {
        let Some(ItemIndex::Changed { items, .. }) = Arc::get_mut(&mut self.items) else {
            unreachable!("현재 묶음의 항목 변경표");
        };
        let Some(PathIndex::Changes { paths, .. }) = Arc::get_mut(&mut self.paths) else {
            unreachable!("현재 묶음의 경로 변경표");
        };
        let mut keys = self.folders.to_vec();
        for &(row, path) in removed {
            items.insert(keys[row], None);
            paths.insert(Arc::from(path), None);
        }
        for &(from, to) in moves {
            keys[to] = keys[from];
            items.insert(keys[to], Some(Item::Folder(to)));
        }
        keys.truncate(keys.len() - removed.len());
        self.folders = keys.into();
    }
    pub fn path_key(&self, relative: &Path) -> Option<Key> {
        let number = self.paths.get(relative)?;
        self.items.get(number).is_some().then_some(Key {
            scope: self.scope,
            number,
        })
    }
}

struct Undo {
    next: u64,
    before: BTreeMap<Arc<Path>, Option<(u64, Node)>>,
    budget: Arc<Budget>,
}
#[derive(Default)]
struct IdPaths {
    single: HashMap<u64, paths::Shared>,
    shared: HashMap<u64, Vec<paths::Shared>>,
    directory_conflicts: HashSet<u64>,
}
impl IdPaths {
    fn insert(&mut self, id: u64, path: &paths::Shared) {
        if let Some(paths) = self.shared.get_mut(&id) {
            paths.push(path.clone());
            return;
        }
        match self.single.entry(id) {
            hash_map::Entry::Vacant(slot) => {
                slot.insert(path.clone());
            }
            hash_map::Entry::Occupied(slot) => {
                self.shared.insert(id, vec![slot.remove(), path.clone()]);
            }
        }
    }

    fn remove(&mut self, id: u64, path: &paths::Shared) {
        if let hash_map::Entry::Occupied(mut slot) = self.shared.entry(id) {
            let paths = slot.get_mut();
            let position = paths.iter().position(|p| p == path).expect("공유 ID 경로");
            paths.swap_remove(position);
            if paths.len() == 1 {
                self.single.insert(id, slot.remove().pop().unwrap());
            }
        } else {
            self.single.remove(&id);
        }
    }

    fn get(&self, id: u64) -> &[paths::Shared] {
        self.shared
            .get(&id)
            .map(Vec::as_slice)
            .or_else(|| self.single.get(&id).map(std::slice::from_ref))
            .unwrap_or_default()
    }

    fn update_directory_conflict(
        &mut self,
        id: u64,
        paths: &paths::Map<u64>,
        values: &HashMap<u64, Node>,
    ) {
        let conflict = self.shared.get(&id).is_some_and(|aliases| {
            aliases
                .iter()
                .filter(|path| {
                    values[paths.get_stored(path).unwrap()]
                        .directory_id()
                        .is_some()
                })
                .nth(1)
                .is_some()
        });
        if conflict {
            self.directory_conflicts.insert(id);
        } else {
            self.directory_conflicts.remove(&id);
        }
    }
}
/// 경로 인덱스와 메타데이터 소유 저장소를 분리한 추적 수명 내 고정 ID 보관.
pub(super) struct Entries {
    scope: u64,
    next: u64,
    paths: paths::Map<u64>,
    parents: HashMap<Arc<Path>, usize>,
    parent_bytes: u64,
    ids: Option<IdPaths>,
    values: HashMap<u64, Node>,
    undo: Option<Undo>,
}
impl Entries {
    pub fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        Self {
            scope: NEXT.fetch_add(1, Ordering::Relaxed),
            next: 1,
            paths: paths::Map::new(),
            parents: HashMap::new(),
            parent_bytes: 0,
            ids: None,
            values: HashMap::new(),
            undo: None,
        }
    }
    pub fn enable_id_lookup(&mut self) {
        if self.ids.is_none() {
            let mut lookup = IdPaths::default();
            for (path, number) in self.paths.iter() {
                let node = &self.values[number];
                lookup.insert(node.id, path);
                if let Some(id) = node.directory_id() {
                    lookup.update_directory_conflict(id, &self.paths, &self.values);
                }
            }
            self.ids = Some(lookup);
        }
    }

    pub fn nodes_with_id(&self, id: u64) -> impl ExactSizeIterator<Item = (&paths::Shared, &Node)> {
        self.ids
            .as_ref()
            .expect("수집기 ID 조회")
            .get(id)
            .iter()
            .map(|path| (path, &self.values[self.paths.get_stored(path).unwrap()]))
    }

    pub fn conflicting_directory_ids(&self) -> impl Iterator<Item = u64> + '_ {
        let ids = self.ids.as_ref().expect("수집기 ID 조회");
        ids.directory_conflicts.iter().copied()
    }

    fn update_directory_conflicts(&mut self, before: Option<u64>, after: Option<u64>) {
        if before == after {
            return;
        }
        if let Some(ids) = &mut self.ids {
            for id in [before, after].into_iter().flatten() {
                ids.update_directory_conflict(id, &self.paths, &self.values);
            }
        }
    }

    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }
    pub fn len(&self) -> usize {
        self.values.len()
    }
    pub fn get(&self, path: &Path) -> Option<&Node> {
        self.values.get(self.paths.get(path)?)
    }
    pub fn contains_key(&self, path: &Path) -> bool {
        self.paths.get(path).is_some()
    }
    pub(super) fn key_number(&self, path: &Path) -> Option<u64> {
        self.paths.get(path).copied()
    }
    pub(super) fn parent_bytes(&self) -> u64 {
        self.parent_bytes
    }
    fn store_path(&mut self, path: &Path) -> paths::Shared {
        let parent = path.parent().unwrap_or(Path::new(""));
        let parent = match self.parents.get_key_value(parent) {
            Some((stored, _)) => Arc::clone(stored),
            None => {
                // 부모 키와 경로의 인덱스·프로세스별 여유까지 메모리 산정에 포함.
                self.parent_bytes += 64 + parent.as_os_str().len() as u64 * 4;
                Arc::from(parent)
            }
        };
        *self.parents.entry(Arc::clone(&parent)).or_default() += 1;
        paths::Shared::new(parent, path.file_name().unwrap_or_default())
    }
    fn remove_path(&mut self, path: &Path) -> Option<(paths::Shared, u64)> {
        let (stored, number) = self.paths.remove(path)?;
        let parent = stored.parent();
        let count = self.parents.get_mut(parent).expect("저장한 상위 경로");
        *count -= 1;
        if *count == 0 {
            self.parents.remove(parent);
            self.parent_bytes -= 64 + parent.as_os_str().len() as u64 * 4;
        }
        Some((stored, number))
    }
    pub fn begin_edit(&mut self, budget: Arc<Budget>) -> io::Result<()> {
        if self.undo.is_some() {
            return Err(io::Error::other("중첩된 변경 원본 보관"));
        }
        self.undo = Some(Undo {
            next: self.next,
            before: BTreeMap::new(),
            budget,
        });
        Ok(())
    }
    fn record(
        undo: &mut Option<Undo>,
        path: &Path,
        previous: Option<(u64, &Node)>,
    ) -> io::Result<()> {
        if let Some(undo) = undo {
            if !undo.before.contains_key(path) {
                let cost = previous.map_or(384 + path.as_os_str().len() as u64 * 4, |(_, node)| {
                    super::stream::node_undo_cost(path, node)
                });
                undo.budget.charge(cost)?;
                undo.before.insert(
                    Arc::from(path),
                    previous.map(|(number, node)| (number, node.clone())),
                );
            }
        }
        Ok(())
    }
    pub fn original(&self, path: &Path) -> Option<&Node> {
        match self.undo.as_ref().and_then(|undo| undo.before.get(path)) {
            Some(old) => old.as_ref().map(|(_, node)| node),
            None => self.get(path),
        }
    }
    pub(super) fn changed_current_keys(
        &self,
        cancelled: &impl Fn() -> bool,
    ) -> io::Result<HashSet<u64>> {
        let mut keys = HashSet::new();
        if let Some(undo) = &self.undo {
            for (path, old) in &undo.before {
                crate::output::check(cancelled)?;
                if let Some(key) = self.key_number(path) {
                    if old
                        .as_ref()
                        .is_none_or(|(_, node)| self.values[&key] != *node)
                    {
                        keys.insert(key);
                    }
                }
            }
        }
        Ok(keys)
    }
    pub fn insert(&mut self, path: PathBuf, node: Node) -> io::Result<Option<Node>> {
        let existing = self.paths.get_key_value(path.as_path());
        let previous = existing.map(|(_, number)| *number);
        if previous.is_some_and(|number| self.values[&number] == node) {
            return Ok(Some(node));
        }
        let stored = existing.map(|(stored, _)| stored.clone());
        Self::record(
            &mut self.undo,
            &path,
            previous.map(|number| (number, &self.values[&number])),
        )?;
        let same =
            |old: &Node| old.id == node.id && old.attributes & 0x410 == node.attributes & 0x410;
        let current = previous.filter(|number| same(&self.values[number]));
        let original = self
            .undo
            .as_ref()
            .and_then(|undo| undo.before.get(path.as_path()))
            .and_then(Option::as_ref)
            .filter(|(_, old)| same(old))
            .map(|(number, _)| *number);
        let number = current.or(original).unwrap_or_else(|| {
            let number = self.next;
            self.next = self.next.checked_add(1).expect("추적 항목 ID 범위 초과");
            number
        });
        let stored = stored.unwrap_or_else(|| self.store_path(&path));
        if previous != Some(number) {
            self.paths.insert(stored.clone(), number);
        }
        let old = previous.and_then(|number| self.values.remove(&number));
        if let Some(ids) = &mut self.ids {
            if old.as_ref().is_none_or(|old| old.id != node.id) {
                if let Some(old) = &old {
                    ids.remove(old.id, &stored);
                }
                ids.insert(node.id, &stored);
            }
        }
        let after = node.directory_id();
        self.values.insert(number, node);
        self.update_directory_conflicts(old.as_ref().and_then(Node::directory_id), after);
        Ok(old)
    }
    pub fn remove(&mut self, path: &Path) -> io::Result<Option<Node>> {
        let Some(&number) = self.paths.get(path) else {
            return Ok(None);
        };
        Self::record(&mut self.undo, path, Some((number, &self.values[&number])))?;
        let (stored, _) = self.remove_path(path).expect("추적 항목 경로");
        let old = self.values.remove(&number).expect("추적 항목");
        if let Some(ids) = &mut self.ids {
            ids.remove(old.id, &stored);
        }
        self.update_directory_conflicts(old.directory_id(), None);
        Ok(Some(old))
    }
    pub fn direct_children<'a>(&'a self, root: &'a Path) -> impl Iterator<Item = PathBuf> + 'a {
        self.paths
            .subtree(root)
            .filter(move |path| !path.name().is_empty() && path.parent() == root)
            .map(paths::Shared::path)
    }

    pub fn has_descendants(&self, root: &Path) -> bool {
        self.paths.subtree(root).any(|path| path.is_below(root))
    }

    pub fn remove_subtree(
        &mut self,
        root: &Path,
        keep_root: bool,
        cancelled: &impl Fn() -> bool,
    ) -> io::Result<()> {
        let _sample = super::cost::measure(super::cost::Stage::SubtreeRemoval);
        loop {
            let batch: Vec<_> = self
                .paths
                .subtree(root)
                .filter(|path| !keep_root || path.is_below(root))
                .take(256)
                .map(paths::Shared::path)
                .collect();
            if batch.is_empty() {
                return Ok(());
            }
            for path in batch {
                crate::output::check(cancelled)?;
                self.remove(&path)?;
            }
        }
    }
    pub fn mapping_changed(&self) -> bool {
        self.undo.as_ref().is_some_and(|undo| {
            undo.before
                .iter()
                .any(|(path, old)| old.as_ref().map(|(number, _)| number) != self.paths.get(path))
        })
    }
    pub fn touched(&self) -> usize {
        self.undo.as_ref().map_or(0, |undo| undo.before.len())
    }
    pub fn changes(&self) -> impl Iterator<Item = Change> + '_ {
        self.undo
            .as_ref()
            .into_iter()
            .flat_map(|undo| &undo.before)
            .filter_map(|(path, previous)| {
                let old = previous.as_ref().map(|(_, node)| node);
                let new = self.get(path);
                (old != new).then(|| Change::Node {
                    path: path.to_path_buf(),
                    old_cost: old.map_or(0, |node| super::stream::node_cost(path, node)),
                    expected: old.map(|node| node.id),
                    value: new.cloned(),
                })
            })
    }
    pub fn finish_edit(&mut self) {
        self.undo = None;
    }
    pub fn rollback(&mut self) {
        if let Some(undo) = self.undo.take() {
            for (path, old) in undo.before {
                let mut before = None;
                if let Some((stored, number)) = self.remove_path(&path) {
                    let removed = self.values.remove(&number).expect("변경 중 추적 항목");
                    before = removed.directory_id();
                    if let Some(ids) = &mut self.ids {
                        ids.remove(removed.id, &stored);
                    }
                }
                let after = old.as_ref().and_then(|(_, node)| node.directory_id());
                if let Some((number, node)) = old {
                    let stored = self.store_path(&path);
                    if let Some(ids) = &mut self.ids {
                        ids.insert(node.id, &stored);
                    }
                    self.paths.insert(stored, number);
                    self.values.insert(number, node);
                }
                self.update_directory_conflicts(before, after);
            }
            self.next = undo.next;
        }
    }
    pub fn identified(&self) -> impl Iterator<Item = (&paths::Shared, u64, &Node)> {
        self.paths
            .iter()
            .map(|(path, &number)| (path, number, &self.values[&number]))
    }
    pub fn project_scan(
        &self,
        scan: &ScanResult,
        cancelled: &impl Fn() -> bool,
    ) -> std::io::Result<Projection> {
        let root = &scan.folders[0].path;
        let mut folders = Vec::with_capacity(scan.folders.len());
        let mut files = Vec::with_capacity(scan.files.len());
        for folder in &scan.folders {
            crate::output::check(cancelled)?;
            folders.push(
                self.key_number(folder.path.strip_prefix(root).unwrap())
                    .unwrap(),
            );
        }
        for file in &scan.files {
            crate::output::check(cancelled)?;
            let path = scan.folders[file.parent].path.join(&file.name);
            files.push(self.key_number(path.strip_prefix(root).unwrap()).unwrap());
        }
        self.project(folders, files, cancelled)
    }
    pub fn project(
        &self,
        folders: Vec<u64>,
        files: Vec<u64>,
        cancelled: &impl Fn() -> bool,
    ) -> std::io::Result<Projection> {
        self.project_rows(folders, files, None, cancelled)
    }

    pub(super) fn project_update(
        &self,
        folders: Vec<u64>,
        files: Vec<u64>,
        previous: &Projection,
        cancelled: &impl Fn() -> bool,
    ) -> io::Result<Projection> {
        self.project_rows(folders, files, Some(previous), cancelled)
    }

    fn project_rows(
        &self,
        folders: Vec<u64>,
        files: Vec<u64>,
        previous: Option<&Projection>,
        cancelled: &impl Fn() -> bool,
    ) -> io::Result<Projection> {
        let mut items = HashMap::with_capacity(folders.len() + files.len());
        for (number, item) in folders
            .iter()
            .enumerate()
            .map(|(row, &number)| (number, Item::Folder(row)))
            .chain(
                files
                    .iter()
                    .enumerate()
                    .map(|(row, &number)| (number, Item::File(row))),
            )
        {
            crate::output::check(cancelled)?;
            items.insert(number, item);
        }
        let paths = if let Some((previous, undo)) = previous
            .zip(self.undo.as_ref())
            .filter(|(previous, _)| previous.paths.depth() < 8)
        {
            let mut paths = BTreeMap::new();
            for (path, old) in &undo.before {
                crate::output::check(cancelled)?;
                let next = self.paths.get(path).copied();
                if next != old.as_ref().map(|(number, _)| *number) {
                    paths.insert(path.clone(), next);
                }
            }
            if paths.is_empty() {
                Arc::clone(&previous.paths)
            } else {
                Arc::new(PathIndex::Changes {
                    parent: Arc::clone(&previous.paths),
                    depth: previous.paths.depth() + 1,
                    paths,
                })
            }
        } else {
            Arc::new(PathIndex::Full(
                self.paths
                    .iter()
                    .map(|(path, number)| {
                        crate::output::check(cancelled)?;
                        Ok((path.clone(), *number))
                    })
                    .collect::<io::Result<_>>()?,
            ))
        };
        Ok(Projection {
            scope: self.scope,
            paths,
            folders: folders.into(),
            files,
            items: Arc::new(ItemIndex::Full(items)),
        })
    }
}
impl<P: AsRef<Path> + ?Sized> std::ops::Index<&P> for Entries {
    type Output = Node;
    fn index(&self, path: &P) -> &Self::Output {
        self.get(path.as_ref()).expect("추적 항목 경로 불일치")
    }
}
