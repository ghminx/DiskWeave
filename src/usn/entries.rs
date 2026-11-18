use super::{edit::Budget, edit::Change, Node};
use crate::{explorer::Item, scanner::ScanResult};
use std::io;
use std::{
    collections::{btree_map, BTreeMap, HashMap},
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
    folders: Vec<u64>,
    files: Vec<u64>,
    items: HashMap<u64, Item>,
}
// 완료 화면이 같은 경로표를 공유하고 바뀐 경로만 별도로 보관.
enum PathIndex {
    Full(BTreeMap<PathBuf, u64>),
    Changes {
        parent: Arc<PathIndex>,
        depth: usize,
        paths: BTreeMap<PathBuf, Option<u64>>,
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
impl Projection {
    pub(super) fn has_file_key(&self, row: usize, key: u64) -> bool {
        self.files.get(row) == Some(&key)
    }
    pub(super) fn folder_keys(&self) -> &[u64] {
        &self.folders
    }

    pub(super) fn matches_rows(&self, folders: &[u64], files: &[u64]) -> bool {
        self.folders == folders && self.files == files
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
            .then(|| self.items.get(&key.number).copied())
            .flatten()
    }
    pub fn path_key(&self, relative: &Path) -> Option<Key> {
        let number = self.paths.get(relative)?;
        self.items.contains_key(&number).then_some(Key {
            scope: self.scope,
            number,
        })
    }
}

struct Undo {
    next: u64,
    before: BTreeMap<PathBuf, Option<(u64, Node)>>,
    budget: Arc<Budget>,
}
/// 경로 인덱스와 메타데이터 소유 저장소를 분리한 추적 수명 내 고정 ID 보관.
pub(super) struct Entries {
    scope: u64,
    next: u64,
    paths: BTreeMap<PathBuf, u64>,
    values: HashMap<u64, Node>,
    undo: Option<Undo>,
}
impl Entries {
    pub fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        Self {
            scope: NEXT.fetch_add(1, Ordering::Relaxed),
            next: 1,
            paths: BTreeMap::new(),
            values: HashMap::new(),
            undo: None,
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
        self.paths.contains_key(path)
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
                    super::stream::node_cost(path, node)
                });
                undo.budget.charge(cost)?;
                undo.before.insert(
                    path.to_owned(),
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
    pub fn insert(&mut self, path: PathBuf, node: Node) -> io::Result<Option<Node>> {
        let previous = self.paths.get(&path).copied();
        if previous.is_some_and(|number| self.values[&number] == node) {
            return Ok(Some(node));
        }
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
            .and_then(|undo| undo.before.get(&path))
            .and_then(Option::as_ref)
            .filter(|(_, old)| same(old))
            .map(|(number, _)| *number);
        let number = current.or(original).unwrap_or_else(|| {
            let number = self.next;
            self.next = self.next.checked_add(1).expect("추적 항목 ID 범위 초과");
            number
        });
        if previous != Some(number) {
            self.paths.insert(path, number);
        }
        let old = previous.and_then(|number| self.values.remove(&number));
        self.values.insert(number, node);
        Ok(old)
    }
    pub fn remove(&mut self, path: &Path) -> io::Result<Option<Node>> {
        let Some(number) = self.paths.get(path).copied() else {
            return Ok(None);
        };
        Self::record(&mut self.undo, path, Some((number, &self.values[&number])))?;
        self.paths.remove(path);
        Ok(self.values.remove(&number))
    }
    pub fn subtree_keys<'a>(&'a self, root: &'a Path) -> impl Iterator<Item = &'a PathBuf> + 'a {
        self.paths
            .range::<Path, _>((std::ops::Bound::Included(root), std::ops::Bound::Unbounded))
            .map(|(path, _)| path)
            .take_while(move |path| path.starts_with(root))
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
                .subtree_keys(root)
                .filter(|path| !keep_root || path.as_path() != root)
                .take(256)
                .cloned()
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
                    path: path.clone(),
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
                if let Some(number) = self.paths.remove(&path) {
                    self.values.remove(&number);
                }
                if let Some((number, node)) = old {
                    self.paths.insert(path, number);
                    self.values.insert(number, node);
                }
            }
            self.next = undo.next;
        }
    }
    pub fn iter(&self) -> Iter<'_> {
        Iter {
            paths: self.paths.iter(),
            values: &self.values,
        }
    }
    pub fn values(&self) -> impl Iterator<Item = &Node> {
        self.values.values()
    }
    pub fn identified(&self) -> impl Iterator<Item = (&PathBuf, u64, &Node)> {
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
            folders.push(self.paths[folder.path.strip_prefix(root).unwrap()]);
        }
        for file in &scan.files {
            crate::output::check(cancelled)?;
            let path = scan.folders[file.parent].path.join(&file.name);
            files.push(self.paths[path.strip_prefix(root).unwrap()]);
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
            folders,
            files,
            items,
        })
    }
}
pub(super) struct Iter<'a> {
    paths: btree_map::Iter<'a, PathBuf, u64>,
    values: &'a HashMap<u64, Node>,
}
impl<'a> Iterator for Iter<'a> {
    type Item = (&'a PathBuf, &'a Node);
    fn next(&mut self) -> Option<Self::Item> {
        self.paths
            .next()
            .map(|(path, number)| (path, &self.values[number]))
    }
}
impl<'a> IntoIterator for &'a Entries {
    type Item = (&'a PathBuf, &'a Node);
    type IntoIter = Iter<'a>;
    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}
impl<P: AsRef<Path> + ?Sized> std::ops::Index<&P> for Entries {
    type Output = Node;
    fn index(&self, path: &P) -> &Self::Output {
        self.get(path.as_ref()).expect("추적 항목 경로 불일치")
    }
}
