use std::path::PathBuf;
use std::time::SystemTime;
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum Id {
    V2(u64),
}
pub struct Info {
    pub resolved: Option<PathBuf>,
    pub reference: Option<Id>,
    pub bytes: u64,
    pub attributes: u32,
    pub created: Option<SystemTime>,
    pub modified: Option<SystemTime>,
    pub extra: crate::metadata::Report,
}
