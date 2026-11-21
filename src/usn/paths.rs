use std::{
    borrow::Borrow,
    cmp::Ordering,
    collections::{btree_map, BTreeMap},
    ffi::OsStr,
    ops::Bound,
    os::windows::ffi::OsStrExt,
    path::{Path, PathBuf},
    sync::Arc,
};

trait Parts {
    fn parts(&self) -> (&Path, &OsStr);
}
impl Parts for &Path {
    fn parts(&self) -> (&Path, &OsStr) {
        (
            self.parent().unwrap_or(Path::new("")),
            self.file_name().unwrap_or_default(),
        )
    }
}

fn components(parts: &dyn Parts) -> impl Iterator<Item = &OsStr> {
    let (parent, name) = parts.parts();
    parent.iter().chain((!name.is_empty()).then_some(name))
}
impl Ord for dyn Parts + '_ {
    fn cmp(&self, other: &Self) -> Ordering {
        let (left_parent, left_name) = self.parts();
        let (right_parent, right_name) = other.parts();
        if left_parent.as_os_str() == right_parent.as_os_str() {
            return left_name.cmp(right_name);
        }
        components(self).cmp(components(other))
    }
}
impl PartialOrd for dyn Parts + '_ {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl PartialEq for dyn Parts + '_ {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other).is_eq()
    }
}
impl Eq for dyn Parts + '_ {}

/// 파일명과 부모 경로를 분리하고 조회표 및 완료 화면에서 같은 키를 공유.
#[derive(Clone)]
pub(super) struct Shared(Arc<StoredParts>);
struct StoredParts {
    parent: Arc<Path>,
    name: Box<OsStr>,
}
impl Shared {
    pub fn new(parent: Arc<Path>, name: &OsStr) -> Self {
        Self(Arc::new(StoredParts {
            parent,
            name: name.into(),
        }))
    }
    pub fn parent(&self) -> &Path {
        &self.0.parent
    }
    pub fn name(&self) -> &OsStr {
        &self.0.name
    }
    pub fn is_below(&self, root: &Path) -> bool {
        !self.name().is_empty() && self.parent().starts_with(root)
    }
    pub fn path(&self) -> PathBuf {
        if self.name().is_empty() {
            PathBuf::new()
        } else {
            self.parent().join(self.name())
        }
    }
    pub fn wide(&self) -> Vec<u16> {
        let parent = self.parent().as_os_str();
        let name = self.name();
        let mut wide = Vec::with_capacity(parent.len() + name.len() + 1);
        if !parent.is_empty() {
            wide.extend(parent.encode_wide());
            wide.push(b'\\' as u16);
        }
        wide.extend(name.encode_wide());
        wide
    }
}
impl Parts for Shared {
    fn parts(&self) -> (&Path, &OsStr) {
        (self.parent(), self.name())
    }
}
impl Ord for Shared {
    fn cmp(&self, other: &Self) -> Ordering {
        let lhs: &dyn Parts = self;
        lhs.cmp(other)
    }
}
impl PartialOrd for Shared {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl PartialEq for Shared {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other).is_eq()
    }
}
impl Eq for Shared {}
impl<'a> Borrow<dyn Parts + 'a> for Shared {
    fn borrow(&self) -> &(dyn Parts + 'a) {
        self
    }
}

pub(super) struct Map<V>(BTreeMap<Shared, V>);
impl<V> Map<V> {
    pub fn new() -> Self {
        Self(BTreeMap::new())
    }
    pub fn get(&self, path: &Path) -> Option<&V> {
        self.0.get(&path as &dyn Parts)
    }
    pub fn get_stored(&self, path: &Shared) -> Option<&V> {
        self.0.get(path)
    }
    pub fn get_key_value(&self, path: &Path) -> Option<(&Shared, &V)> {
        self.0.get_key_value(&path as &dyn Parts)
    }
    pub fn insert(&mut self, path: Shared, value: V) {
        self.0.insert(path, value);
    }
    pub fn remove(&mut self, path: &Path) -> Option<(Shared, V)> {
        self.0.remove_entry(&path as &dyn Parts)
    }
    pub fn iter(&self) -> btree_map::Iter<'_, Shared, V> {
        self.0.iter()
    }
    pub fn subtree<'a>(&'a self, root: &'a Path) -> impl Iterator<Item = &'a Shared> + 'a {
        self.0
            .range::<dyn Parts, _>((Bound::Included(&root as &dyn Parts), Bound::Unbounded))
            .map(|(path, _)| path)
            .take_while(move |path| path.is_below(root) || (*path as &dyn Parts).eq(&root))
    }
}
impl<V> FromIterator<(Shared, V)> for Map<V> {
    fn from_iter<T: IntoIterator<Item = (Shared, V)>>(iter: T) -> Self {
        Self(iter.into_iter().collect())
    }
}
