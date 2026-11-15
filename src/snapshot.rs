use crate::metadata::{Allocation, Extra};
use crate::scanner::{FileEntry, Folder, ScanIssue, ScanResult};
use serde::{Deserialize, Serialize};
use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io::{self, BufReader, Read, Write};
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::{Component, Path};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub struct SavedScan {
    pub tracking: Option<crate::usn::Checkpoint>,
    pub engine: Option<crate::engine::Report>,
    pub scan: Arc<ScanResult>,
    pub started: SystemTime,
    pub finished: SystemTime,
    pub saved: SystemTime,
    pub elapsed: Duration,
}
impl SavedScan {
    pub fn new(
        scan: Arc<ScanResult>,
        started: SystemTime,
        finished: SystemTime,
        elapsed: Duration,
    ) -> Self {
        Self {
            tracking: None,
            engine: None,
            scan,
            started,
            finished,
            saved: SystemTime::now(),
            elapsed,
        }
    }
}
#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct Time {
    before_epoch: bool,
    seconds: u64,
    nanos: u32,
}
impl From<SystemTime> for Time {
    fn from(value: SystemTime) -> Self {
        let (before_epoch, d) = match value.duration_since(UNIX_EPOCH) {
            Ok(d) => (false, d),
            Err(e) => (true, e.duration()),
        };
        Self {
            before_epoch,
            seconds: d.as_secs(),
            nanos: d.subsec_nanos(),
        }
    }
}
impl Time {
    pub(crate) fn decode(self) -> io::Result<SystemTime> {
        if self.nanos >= 1_000_000_000 {
            return Err(invalid());
        }
        let d = Duration::new(self.seconds, self.nanos);
        if self.before_epoch {
            UNIX_EPOCH.checked_sub(d)
        } else {
            UNIX_EPOCH.checked_add(d)
        }
        .ok_or_else(invalid)
    }
}
#[derive(Serialize, Deserialize)]
struct Directory {
    name: Vec<u16>,
    parent: Option<usize>,
    created: Option<Time>,
    modified: Option<Time>,
    attributes: u32,
}
#[derive(Serialize, Deserialize)]
struct Entry {
    name: Vec<u16>,
    parent: usize,
    bytes: u64,
    created: Option<Time>,
    modified: Option<Time>,
    attributes: u32,
    extra: Extra,
}
#[derive(Serialize, Deserialize)]
struct Issue {
    path: Vec<u16>,
    message: String,
    os_code: Option<i32>,
}
impl Issue {
    fn encode(issue: &ScanIssue) -> Self {
        Self {
            path: wide(issue.path.as_os_str()),
            message: issue.error.to_string(),
            os_code: issue.error.raw_os_error(),
        }
    }
    fn decode(self) -> ScanIssue {
        ScanIssue {
            path: OsString::from_wide(&self.path).into(),
            error: self
                .os_code
                .map(io::Error::from_raw_os_error)
                .unwrap_or_else(|| io::Error::other(self.message)),
        }
    }
}
#[derive(Serialize, Deserialize)]
struct Archive {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    tracking: Option<crate::usn::Checkpoint>,
    #[serde(default)]
    engine: Option<crate::engine::Report>,
    format: String,
    version: u32,
    basis: String,
    root: Vec<u16>,
    started: Time,
    finished: Time,
    saved: Time,
    elapsed_secs: u64,
    elapsed_nanos: u32,
    detailed: bool,
    error_count: u64,
    metadata_error_count: u64,
    skipped_reparse_points: u64,
    issues: Vec<Issue>,
    metadata_issues: Vec<Issue>,
    folders: Vec<Directory>,
    files: Vec<Entry>,
}
pub fn wide(value: &OsStr) -> Vec<u16> {
    value.encode_wide().collect()
}
fn invalid() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "분석 결과 파일이 손상되었거나 올바른 형식이 아닙니다",
    )
}
fn component(name: &[u16]) -> io::Result<OsString> {
    if name.is_empty() || name.len() > 255 || name.iter().any(|n| [0, 47, 92, 58].contains(n)) {
        return Err(invalid());
    }
    let name = OsString::from_wide(name);
    let mut parts = Path::new(&name).components();
    if !matches!(parts.next(), Some(Component::Normal(_))) || parts.next().is_some() {
        return Err(invalid());
    }
    Ok(name)
}
fn encoded(record: &SavedScan, cancelled: &impl Fn() -> bool) -> io::Result<Archive> {
    let scan = &record.scan;
    let folders = scan
        .folders
        .iter()
        .map(|f| {
            crate::output::check(cancelled)?;
            Ok(Directory {
                name: wide(f.path.file_name().unwrap_or(OsStr::new(""))),
                parent: f.parent,
                created: f.created.map(Into::into),
                modified: f.modified.map(Into::into),
                attributes: f.attributes,
            })
        })
        .collect::<io::Result<_>>()?;
    let files = scan
        .files
        .iter()
        .map(|f| {
            crate::output::check(cancelled)?;
            Ok(Entry {
                name: wide(&f.name),
                parent: f.parent,
                bytes: f.logical_bytes,
                created: f.created.map(Into::into),
                modified: f.modified.map(Into::into),
                attributes: f.attributes,
                extra: f.extra,
            })
        })
        .collect::<io::Result<_>>()?;
    Ok(Archive {
        tracking: record.tracking.clone(),
        engine: record.engine.clone(),
        format: "DiskWeave".into(),
        version: 1,
        basis: "paths-default-stream-reparse-excluded-v1".into(),
        root: wide(scan.folders[0].path.as_os_str()),
        started: record.started.into(),
        finished: record.finished.into(),
        saved: SystemTime::now().into(),
        elapsed_secs: record.elapsed.as_secs(),
        elapsed_nanos: record.elapsed.subsec_nanos(),
        detailed: scan.detailed,
        error_count: scan.error_count,
        metadata_error_count: scan.metadata_error_count,
        skipped_reparse_points: scan.skipped_reparse_points,
        issues: scan.issues.iter().map(Issue::encode).collect(),
        metadata_issues: scan.metadata_issues.iter().map(Issue::encode).collect(),
        folders,
        files,
    })
}
pub fn save(
    record: &SavedScan,
    path: &Path,
    overwrite: bool,
    cancelled: impl Fn() -> bool,
) -> io::Result<()> {
    crate::output::protect(path, &record.scan, &cancelled)?;
    let data = encoded(record, &cancelled)?;
    crate::output::write(path, overwrite, cancelled, |writer| {
        serde_json::to_writer(LimitedWriter { writer, bytes: 0 }, &data)
            .map_err(crate::output::json_error)
    })
}
struct LimitedWriter<'a> {
    writer: &'a mut dyn Write,
    bytes: u64,
}
impl Write for LimitedWriter<'_> {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        if buffer.len() as u64 > 1_073_741_824 - self.bytes {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "분석 결과 파일은 최대 1 GiB까지 저장할 수 있습니다",
            ));
        }
        let written = self.writer.write(buffer)?;
        self.bytes += written as u64;
        Ok(written)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.writer.flush()
    }
}
struct CheckedReader<'a, C, R> {
    reader: BufReader<R>,
    bytes: u64,
    cancelled: &'a C,
}
impl<C: Fn() -> bool, R: Read> Read for CheckedReader<'_, C, R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if (self.cancelled)() {
            return Err(io::Error::other("작업을 취소했습니다"));
        }
        let n = self.reader.read(buffer)?;
        self.bytes += n as u64;
        if self.bytes > 1_073_741_824 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "분석 결과는 최대 1 GiB까지 열 수 있습니다",
            ));
        }
        Ok(n)
    }
}
pub fn load(path: &Path, cancelled: impl Fn() -> bool) -> io::Result<SavedScan> {
    crate::output::check(&cancelled)?;
    let file = File::open(path)?;
    if file.metadata()?.len() > 1_073_741_824 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "분석 결과 파일은 최대 1 GiB까지 열 수 있습니다",
        ));
    }
    read_from(file, cancelled)
}
pub fn write_to(
    record: &SavedScan,
    writer: &mut dyn Write,
    cancelled: impl Fn() -> bool,
) -> io::Result<()> {
    let data = encoded(record, &cancelled)?;
    let result = serde_json::to_writer(LimitedWriter { writer, bytes: 0 }, &data)
        .map_err(crate::output::json_error);
    crate::output::check(&cancelled)?;
    result
}
pub fn read_from(reader: impl Read, cancelled: impl Fn() -> bool) -> io::Result<SavedScan> {
    let parsed = serde_json::from_reader(CheckedReader {
        reader: BufReader::new(reader),
        bytes: 0,
        cancelled: &cancelled,
    });
    crate::output::check(&cancelled)?;
    let a: Archive = parsed.map_err(|error| {
        let error = crate::output::json_error(error);
        io::Error::new(
            error.kind(),
            format!("분석 결과 파일을 읽을 수 없습니다. {error}"),
        )
    })?;
    if a.format != "DiskWeave"
        || a.version != 1
        || a.basis != "paths-default-stream-reparse-excluded-v1"
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "지원하지 않는 분석 결과 버전 또는 계산 기준입니다",
        ));
    }
    if a.folders.is_empty()
        || a.elapsed_nanos >= 1_000_000_000
        || a.issues.len() > 20
        || a.metadata_issues.len() > 20
        || a.error_count < a.issues.len() as u64
        || a.metadata_error_count < a.metadata_issues.len() as u64
    {
        return Err(invalid());
    }
    let root = std::path::PathBuf::from(OsString::from_wide(&a.root));
    if !root.is_absolute() || a.root.contains(&0) || a.root.len() > 32767 {
        return Err(invalid());
    }
    let mut scan = ScanResult {
        detailed: a.detailed,
        metadata_error_count: a.metadata_error_count,
        metadata_issues: a.metadata_issues.into_iter().map(Issue::decode).collect(),
        folders: Vec::new(),
        files: Vec::new(),
        children: vec![Vec::new(); a.folders.len()],
        file_children: vec![Vec::new(); a.folders.len()],
        error_count: a.error_count,
        issues: a.issues.into_iter().map(Issue::decode).collect(),
        skipped_reparse_points: a.skipped_reparse_points,
    };
    let mut names = std::collections::HashSet::new();
    for (index, f) in a.folders.into_iter().enumerate() {
        crate::output::check(&cancelled)?;
        let path = if index == 0 {
            if f.parent.is_some() {
                return Err(invalid());
            }
            root.clone()
        } else {
            let parent = f.parent.filter(|&p| p < index).ok_or_else(invalid)?;
            let name = component(&f.name)?;
            if !names.insert((parent, name.clone())) {
                return Err(invalid());
            }
            let path = scan.folders[parent].path.join(name);
            if path.as_os_str().encode_wide().count() > 32767 {
                return Err(invalid());
            }
            scan.children[parent].push(index);
            path
        };
        scan.folders.push(Folder {
            path,
            parent: f.parent,
            created: f.created.map(Time::decode).transpose()?,
            modified: f.modified.map(Time::decode).transpose()?,
            attributes: f.attributes,
            allocation: Allocation::default(),
            logical_bytes: 0,
            file_count: 0,
            folder_count: 0,
        });
    }
    for f in a.files {
        crate::output::check(&cancelled)?;
        if f.parent >= scan.folders.len() {
            return Err(invalid());
        }
        let name = component(&f.name)?;
        if !names.insert((f.parent, name.clone())) {
            return Err(invalid());
        }
        let parent = &mut scan.folders[f.parent];
        parent.logical_bytes = parent
            .logical_bytes
            .checked_add(f.bytes)
            .ok_or_else(invalid)?;
        parent.file_count = parent.file_count.checked_add(1).ok_or_else(invalid)?;
        parent.allocation.add(f.extra.allocation)?;
        scan.file_children[f.parent].push(scan.files.len());
        scan.files.push(FileEntry {
            name,
            parent: f.parent,
            logical_bytes: f.bytes,
            created: f.created.map(Time::decode).transpose()?,
            modified: f.modified.map(Time::decode).transpose()?,
            attributes: f.attributes,
            extra: f.extra,
        });
    }
    for id in (1..scan.folders.len()).rev() {
        crate::output::check(&cancelled)?;
        let (before, after) = scan.folders.split_at_mut(id);
        let child = &after[0];
        let parent = &mut before[child.parent.unwrap()];
        parent.logical_bytes = parent
            .logical_bytes
            .checked_add(child.logical_bytes)
            .ok_or_else(invalid)?;
        parent.file_count = parent
            .file_count
            .checked_add(child.file_count)
            .ok_or_else(invalid)?;
        parent.folder_count = parent
            .folder_count
            .checked_add(child.folder_count)
            .and_then(|n| n.checked_add(1))
            .ok_or_else(invalid)?;
        parent.allocation.merge(child.allocation)?;
    }
    for siblings in &mut scan.children {
        siblings.sort_unstable_by(|&a, &b| {
            scan.folders[b]
                .logical_bytes
                .cmp(&scan.folders[a].logical_bytes)
                .then_with(|| scan.folders[a].path.cmp(&scan.folders[b].path))
        });
    }
    for siblings in &mut scan.file_children {
        siblings.sort_unstable_by(|&a, &b| {
            scan.files[b]
                .logical_bytes
                .cmp(&scan.files[a].logical_bytes)
                .then_with(|| scan.files[a].name.cmp(&scan.files[b].name))
        });
    }
    if let Some(state) = &a.tracking {
        state.validate(&scan)?;
    }
    Ok(SavedScan {
        tracking: a.tracking,
        engine: a.engine,
        scan: Arc::new(scan),
        started: a.started.decode()?,
        finished: a.finished.decode()?,
        saved: a.saved.decode()?,
        elapsed: Duration::new(a.elapsed_secs, a.elapsed_nanos),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::windows::ffi::OsStringExt;
    use std::sync::Arc;
    use std::time::{Duration, SystemTime};

    #[test]
    fn scan_method_survives_stream_transfer_and_old_archives_stay_unknown() {
        let root = std::env::temp_dir().join(format!("diskweave-method-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let scan = Arc::new(crate::scanner::scan(&root, || false).unwrap());
        let mut record = SavedScan::new(scan, SystemTime::now(), SystemTime::now(), Duration::ZERO);
        record.engine = Some(crate::engine::Report {
            requested: crate::engine::Method::Ntfs,
            actual: crate::engine::Method::Basic,
            fallback: Some("권한 없음".into()),
        });
        let mut bytes = Vec::new();
        write_to(&record, &mut bytes, || false).unwrap();
        let loaded = read_from(bytes.as_slice(), || false).unwrap();
        assert_eq!(loaded.engine, record.engine);
        let mut old: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        old.as_object_mut().unwrap().remove("engine");
        assert!(
            read_from(serde_json::to_vec(&old).unwrap().as_slice(), || false)
                .unwrap()
                .engine
                .is_none()
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn save_never_overwrites_scanned_source_and_cancel_keeps_existing_output() {
        let root =
            std::env::temp_dir().join(format!("diskweave-save-preserve-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let original = root.join("source.bin");
        std::fs::write(&original, b"original").unwrap();
        let time = SystemTime::now();
        let record = SavedScan::new(
            Arc::new(crate::scanner::scan(&root, || false).unwrap()),
            time,
            time,
            Duration::ZERO,
        );
        assert!(save(&record, &original, true, || false).is_err());
        assert_eq!(std::fs::read(&original).unwrap(), b"original");
        let output = root.join("existing.dwsnap");
        std::fs::write(&output, b"previous").unwrap();
        assert!(save(&record, &output, false, || false).is_err());
        let calls = std::cell::Cell::new(0);
        let error = save(&record, &output, true, || {
            calls.set(calls.get() + 1);
            calls.get() > 12
        })
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
        assert_eq!(std::fs::read(&output).unwrap(), b"previous");
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), 2);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn loading_rejects_bad_version_parent_and_overflow_and_preserves_detailed_errors() {
        let base =
            std::env::temp_dir().join(format!("diskweave-save-validation-{}", std::process::id()));
        let root = base.join("data");
        std::fs::create_dir_all(root.join("child")).unwrap();
        std::fs::write(root.join("child/data.bin"), [0; 7]).unwrap();
        std::fs::hard_link(root.join("child/data.bin"), root.join("alias.bin")).unwrap();
        let mut scan = crate::scanner::scan_with_options(&root, true, || false, |_| {}).unwrap();
        scan.error_count = 4;
        scan.issues.push(ScanIssue {
            path: root.join("missing"),
            error: io::Error::from_raw_os_error(5),
        });
        let expected = scan.folders[0].allocation.bytes;
        let time = SystemTime::now();
        let record = SavedScan::new(Arc::new(scan), time, time, Duration::ZERO);
        let output = base.join("result.dwsnap");
        save(&record, &output, false, || false).unwrap();
        let restored = load(&output, || false).unwrap();
        assert_eq!(restored.scan.folders[0].logical_bytes, 14);
        assert_eq!(restored.scan.folders[0].allocation.bytes, expected);
        assert_eq!(
            restored.scan.files[0].extra.identity,
            restored.scan.files[1].extra.identity
        );
        assert_eq!(restored.scan.error_count, 4);
        assert_eq!(restored.scan.issues[0].error.raw_os_error(), Some(5));
        let bytes = std::fs::read(&output).unwrap();
        let reads = std::cell::Cell::new(0);
        assert_eq!(
            load(&output, || {
                reads.set(reads.get() + 1);
                reads.get() > 32
            })
            .err()
            .unwrap()
            .kind(),
            io::ErrorKind::Interrupted
        );
        let original: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        for (field, value) in [
            ("version", serde_json::json!(99)),
            ("root", serde_json::json!([0])),
            ("folders", serde_json::json!([])),
        ] {
            let mut bad = original.clone();
            bad[field] = value;
            std::fs::write(&output, serde_json::to_vec(&bad).unwrap()).unwrap();
            assert!(load(&output, || false).is_err());
        }
        let mut bad = original.clone();
        bad["folders"][1]["parent"] = serde_json::json!(1);
        std::fs::write(&output, serde_json::to_vec(&bad).unwrap()).unwrap();
        assert!(load(&output, || false).is_err());
        let mut bad = original;
        for entry in bad["files"].as_array_mut().unwrap() {
            entry["bytes"] = serde_json::json!(u64::MAX);
        }
        std::fs::write(&output, serde_json::to_vec(&bad).unwrap()).unwrap();
        assert!(load(&output, || false).is_err());
        std::fs::write(&output, b"{broken").unwrap();
        assert!(load(&output, || false).is_err());
        assert_eq!(
            load(&output, || true).err().unwrap().kind(),
            io::ErrorKind::Interrupted
        );
        std::fs::remove_dir_all(base).unwrap();
    }
    #[test]
    fn saved_result_reopens_offline_without_losing_paths_sizes_or_missing_values() {
        let base = std::env::temp_dir().join(format!("diskweave-save-{}", std::process::id()));
        let root = base.join("자료");
        std::fs::create_dir_all(root.join("child")).unwrap();
        std::fs::write(root.join("child/model.bin"), [1; 7]).unwrap();
        let name = std::ffi::OsString::from_wide(&[0xd800, 0x2e, 0x78]);
        std::fs::write(root.join(&name), [2; 3]).unwrap();
        std::fs::write(root.join("empty"), []).unwrap();
        let scan = Arc::new(crate::scanner::scan(&root, || false).unwrap());
        let time = SystemTime::now();
        let record = SavedScan::new(scan, time, time, Duration::from_millis(12));
        let output = base.join("result.dwsnap");
        save(&record, &output, false, || false).unwrap();
        assert_eq!(std::fs::read(root.join("child/model.bin")).unwrap(), [1; 7]);
        std::fs::remove_dir_all(&root).unwrap();
        let restored = load(&output, || false).unwrap();
        assert_eq!(restored.scan.folders[0].logical_bytes, 10);
        assert_eq!(restored.scan.folders[0].file_count, 3);
        assert_eq!(restored.scan.folders[0].folder_count, 1);
        assert_eq!(restored.scan.folders[0].path, root);
        assert!(restored.scan.files.iter().any(|file| file.name == name));
        assert_eq!(restored.started, time);
        assert_eq!(restored.finished, time);
        assert_eq!(restored.elapsed, Duration::from_millis(12));
        assert_eq!(restored.scan.folders[0].allocation.not_collected, 3);
        assert!(restored
            .scan
            .files
            .iter()
            .all(|f| f.extra.allocation.value().is_none()));
        std::fs::remove_dir_all(base).unwrap();
    }
}
