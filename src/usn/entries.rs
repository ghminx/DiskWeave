use super::Node;
use crate::{explorer::Item, scanner::ScanResult};
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
    paths: Arc<BTreeMap<PathBuf, u64>>,
    folders: Vec<u64>,
    files: Vec<u64>,
    items: HashMap<u64, Item>,
}
impl Projection {
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
        let number = *self.paths.get(relative)?;
        self.items.contains_key(&number).then_some(Key {
            scope: self.scope,
            number,
        })
    }
}

/// 경로 인덱스와 메타데이터 소유 저장소를 분리한 추적 수명 내 고정 ID 보관.
#[derive(Clone)]
pub(super) struct Entries {
    scope: u64,
    next: u64,
    paths: Arc<BTreeMap<PathBuf, u64>>,
    values: HashMap<u64, Node>,
}
impl PartialEq for Entries {
    fn eq(&self, other: &Self) -> bool {
        self.scope == other.scope && self.paths == other.paths && self.values == other.values
    }
}
impl Eq for Entries {}
impl Entries {
    pub fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        Self {
            scope: NEXT.fetch_add(1, Ordering::Relaxed),
            next: 1,
            paths: Arc::new(BTreeMap::new()),
            values: HashMap::new(),
        }
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
    pub fn insert(&mut self, path: PathBuf, node: Node) -> Option<Node> {
        let previous = self.paths.get(&path).copied();
        let same = previous.is_some_and(|number| {
            let old = &self.values[&number];
            old.id == node.id && old.attributes & 0x410 == node.attributes & 0x410
        });
        let number = if same {
            previous.unwrap()
        } else {
            let number = self.next;
            self.next = self.next.checked_add(1).expect("추적 항목 ID 범위 초과");
            Arc::make_mut(&mut self.paths).insert(path, number);
            number
        };
        let old = previous.and_then(|number| self.values.remove(&number));
        self.values.insert(number, node);
        old
    }
    pub fn remove(&mut self, path: &Path) -> Option<Node> {
        let number = Arc::make_mut(&mut self.paths).remove(path)?;
        self.values.remove(&number)
    }
    pub fn retain(&mut self, mut keep: impl FnMut(&PathBuf, &Node) -> bool) {
        let values = &mut self.values;
        Arc::make_mut(&mut self.paths).retain(|path, number| {
            if keep(path, &values[number]) {
                true
            } else {
                values.remove(number);
                false
            }
        });
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
    pub fn keys(&self) -> impl Iterator<Item = &PathBuf> {
        self.paths.keys()
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
        Ok(Projection {
            scope: self.scope,
            paths: Arc::clone(&self.paths),
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
