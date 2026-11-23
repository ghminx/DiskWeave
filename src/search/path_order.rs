use crate::explorer::Item;
use crate::scanner::{Folder, ScanResult};
use std::cell::OnceCell;
use std::cmp::Ordering;
use std::ffi::OsStr;
use std::path::{Component, Path, PathBuf};

pub(crate) struct PathKey<'a> {
    folder: &'a Folder,
    name: Option<&'a OsStr>,
    single_component: bool,
    joined: OnceCell<PathBuf>,
}

impl<'a> PathKey<'a> {
    pub(crate) fn new(scan: &'a ScanResult, item: Item) -> Self {
        let (folder, name) = match item {
            Item::Folder(id) => (&scan.folders[id], None),
            Item::File(id) => {
                let file = &scan.files[id];
                (&scan.folders[file.parent], Some(file.name.as_os_str()))
            }
        };
        let single_component = name.is_some_and(|name| {
            let mut parts = Path::new(name).components();
            matches!(parts.next(), Some(Component::Normal(part)) if part == name)
                && parts.next().is_none()
        });
        Self {
            folder,
            name,
            single_component,
            joined: OnceCell::new(),
        }
    }

    fn path(&self) -> &Path {
        match self.name {
            Some(name) => self.joined.get_or_init(|| self.folder.path.join(name)),
            None => &self.folder.path,
        }
    }

    pub(crate) fn compare(&self, other: &Self, descending: bool, folders_first: bool) -> Ordering {
        if folders_first && self.name.is_some() != other.name.is_some() {
            return self.name.is_some().cmp(&other.name.is_some());
        }
        // 같은 부모에 속한 단일 이름은 공통 경로 구성 요소의 재비교 생략.
        let order = match (self.name, other.name) {
            (Some(a), Some(b))
                if self.single_component
                    && other.single_component
                    && std::ptr::eq(self.folder, other.folder) =>
            {
                a.cmp(b)
            }
            _ => self.path().cmp(other.path()),
        };
        if descending {
            order.reverse()
        } else {
            order
        }
    }
}
