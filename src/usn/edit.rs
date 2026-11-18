use super::{stream::Change, Index, Unavailable};
use std::{
    collections::BTreeMap,
    io,
    ops::{Deref, DerefMut},
    os::windows::ffi::OsStrExt,
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
            if emit_changes {
                changes.push(change);
            }
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
        self.index.nodes.finish_edit();
        self.index.unavailable.undo = None;
        self.index.changes = if emit_changes { changes } else { vec![] };
        self.index.last_edit = stats;
        if changed {
            self.index.scan = None;
        }
        if mapping_changed {
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
