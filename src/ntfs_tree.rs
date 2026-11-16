use std::{collections::HashSet, ffi::OsString, io};
#[derive(Clone, Debug)]
pub struct Name {
    pub parent: u64,
    pub flags: u32,
    pub name: OsString,
    pub cached: Option<CachedName>,
}
#[derive(Clone, Debug)]
pub struct CachedName {
    pub created: i64,
    pub modified: i64,
    pub bytes: u64,
    pub attributes: u32,
}
#[derive(Clone, Debug)]
pub struct Stream {
    pub kind: u32,
    pub name: OsString,
    pub allocation: u64,
    pub bytes: u64,
}
#[derive(Clone, Debug)]
pub struct Record {
    pub reference: u64,
    pub attributes: u32,
    pub names: Vec<Name>,
    pub streams: Vec<Stream>,
    pub times: Option<(i64, i64)>,
}
fn invalid() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "파일 배치 응답의 길이·버전·연결이 잘못됐습니다",
    )
}
pub(crate) fn system_time(ticks: i64) -> io::Result<std::time::SystemTime> {
    let delta = i128::from(ticks) - 116_444_736_000_000_000i128;
    let duration = std::time::Duration::new(
        (delta.unsigned_abs() / 10_000_000) as u64,
        ((delta.unsigned_abs() % 10_000_000) * 100) as u32,
    );
    if delta >= 0 {
        std::time::UNIX_EPOCH.checked_add(duration)
    } else {
        std::time::UNIX_EPOCH.checked_sub(duration)
    }
    .ok_or_else(invalid)
}
pub fn scan_with_probe(
    root: &std::path::Path,
    target: &crate::ntfs_native::Target,
    records: Vec<Record>,
    detailed: bool,
    cancelled: impl Fn() -> bool,
    probe: &(impl Fn(&std::path::Path, &Record, u64) -> io::Result<crate::metadata::Extra> + Sync),
) -> io::Result<crate::scanner::ScanResult> {
    use crate::{
        metadata, ntfs, ntfs_native,
        scanner::{FileEntry, Folder, ScanResult},
    };
    use std::{collections::HashMap, path::Component};
    crate::output::check(&cancelled)?;
    let observed = ntfs_native::inspect_file(root, false, &target.volume)?;
    if observed.attributes & 0x410 != 0x10 {
        return Err(invalid());
    }
    let Some(ntfs::Id::V2(root_id)) = observed.reference else {
        return Err(invalid());
    };
    let mut by_parent: HashMap<u64, Vec<(usize, usize)>> = HashMap::new();
    let mut ids = HashSet::new();
    for (i, row) in records.iter().enumerate() {
        crate::output::check(&cancelled)?;
        if !ids.insert(row.reference) {
            return Err(invalid());
        }
        for (j, name) in row.names.iter().enumerate() {
            // DOS 전용 별칭은 별도 파일로 세지 않으며 PRIMARY|DOS 이름은 유지.
            if name.flags & 3 != 2 {
                by_parent.entry(name.parent).or_default().push((i, j));
            }
        }
    }
    if !ids.contains(&root_id) {
        return Err(invalid());
    }
    for row in &records {
        crate::output::check(&cancelled)?;
        if row.reference != root_id
            && row
                .names
                .iter()
                .any(|name| name.flags & 3 != 2 && !ids.contains(&name.parent))
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "부모가 없는 레코드의 소속 범위를 확인할 수 없습니다",
            ));
        }
    }
    let folder = |path, parent, m: ntfs::Info| Folder {
        path,
        parent,
        attributes: m.attributes,
        created: m.created,
        modified: m.modified,
        allocation: Default::default(),
        logical_bytes: 0,
        file_count: 0,
        folder_count: 0,
    };
    let mut result = ScanResult {
        detailed,
        metadata_error_count: 0,
        metadata_issues: vec![],
        folders: vec![folder(root.to_owned(), None, observed)],
        files: vec![],
        children: vec![],
        file_children: vec![],
        error_count: 0,
        issues: vec![],
        skipped_reparse_points: 0,
    };
    let mut pending = vec![(root_id, 0)];
    let mut directories = HashSet::from([root_id]);
    let mut paths = HashSet::new();
    let mut tasks = Vec::new();
    while let Some((id, parent)) = pending.pop() {
        crate::output::check(&cancelled)?;
        for &(i, j) in by_parent.get(&id).map_or(&[][..], |v| v.as_slice()) {
            crate::output::check(&cancelled)?;
            let row = &records[i];
            let name = &row.names[j].name;
            if row.reference == root_id && name == "." {
                continue;
            }
            if std::path::Path::new(name).components().count() != 1
                || !matches!(
                    std::path::Path::new(name).components().next(),
                    Some(Component::Normal(_))
                )
            {
                return Err(invalid());
            }
            let path = result.folders[parent].path.join(name);
            if !paths.insert(path.clone()) {
                return Err(invalid());
            }
            if row.attributes & 0x400 != 0 {
                result.skipped_reparse_points += 1;
                continue;
            }
            if row.attributes & 0x10 != 0 {
                let info = match ntfs_native::inspect_file(&path, false, &target.volume) {
                    Ok(m) => m,
                    Err(e) => {
                        result.record_error(path, e);
                        continue;
                    }
                };
                if info.reference != Some(ntfs::Id::V2(row.reference))
                    || info.attributes & 0x410 != 0x10
                    || !directories.insert(row.reference)
                {
                    return Err(invalid());
                }
                let index = result.folders.len();
                result.folders.push(folder(path, Some(parent), info));
                pending.push((row.reference, index));
            } else {
                let streams: Vec<_> = row
                    .streams
                    .iter()
                    .filter(|s| s.kind == 128 && (s.name.is_empty() || s.name == "::$DATA"))
                    .collect();
                if streams.len() != 1 {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "기본 데이터 스트림이 없거나 중복됐습니다",
                    ));
                }
                let stream = streams[0];
                let times = row.names[j]
                    .cached
                    .as_ref()
                    .map(|c| (c.created, c.modified))
                    .or(row.times)
                    .ok_or_else(invalid)?;
                let extra = metadata::Extra::default();
                tasks.push((i, j, parent, stream.bytes));
                result.files.push(FileEntry {
                    extra,
                    created: Some(system_time(times.0)?),
                    modified: Some(system_time(times.1)?),
                    attributes: row.names[j]
                        .cached
                        .as_ref()
                        .map_or(row.attributes, |c| c.attributes),
                    name: name.clone(),
                    parent,
                    logical_bytes: row.names[j]
                        .cached
                        .as_ref()
                        .map_or(stream.bytes, |c| c.bytes),
                });
            }
        }
    }
    if detailed && !tasks.is_empty() {
        use std::sync::{
            atomic::{AtomicBool, AtomicUsize, Ordering},
            mpsc,
        };
        let next = AtomicUsize::new(0);
        let stop = AtomicBool::new(false);
        let (sender, receiver) = mpsc::sync_channel::<io::Result<Vec<(usize, metadata::Extra)>>>(8);
        let parents = &result.folders;
        let files = &mut result.files;
        std::thread::scope(|scope| -> io::Result<()> {
            let mut handles = Vec::new();
            for _ in 0..8.min(tasks.len()) {
                let sender = sender.clone();
                let next = &next;
                let stop = &stop;
                let tasks = &tasks;
                let records = &records;
                handles.push(scope.spawn(move || {
                    let run = std::panic::catch_unwind(std::panic::AssertUnwindSafe(
                        || -> io::Result<()> {
                            loop {
                                let start = next.fetch_add(32, Ordering::Relaxed);
                                if start >= tasks.len() || stop.load(Ordering::Acquire) {
                                    break;
                                }
                                let mut batch = Vec::with_capacity(32);
                                for (index, &(i, j, parent, size)) in
                                    tasks.iter().enumerate().skip(start).take(32)
                                {
                                    if stop.load(Ordering::Acquire) {
                                        break;
                                    }
                                    let path = parents[parent].path.join(&records[i].names[j].name);
                                    batch.push((index, probe(&path, &records[i], size)?));
                                }
                                if sender.send(Ok(batch)).is_err() {
                                    break;
                                }
                            }
                            Ok(())
                        },
                    ));
                    let error = match run {
                        Ok(Ok(())) => None,
                        Ok(Err(e)) => Some(e),
                        Err(_) => Some(io::Error::other("후보 상세 조회 worker 실패")),
                    };
                    if let Some(error) = error {
                        stop.store(true, Ordering::Release);
                        let _ = sender.send(Err(error));
                    }
                }));
            }
            drop(sender);
            let mut failure = None;
            loop {
                if cancelled() && failure.is_none() {
                    failure = Some(io::Error::new(io::ErrorKind::Interrupted, "후보 스캔 취소"));
                    stop.store(true, Ordering::Release);
                }
                match receiver.recv_timeout(std::time::Duration::from_millis(5)) {
                    Ok(Ok(batch)) => {
                        if failure.is_none() {
                            for (index, extra) in batch {
                                files[index].extra = extra;
                            }
                        }
                    }
                    Ok(Err(e)) => {
                        if failure.is_none() {
                            failure = Some(e);
                        }
                        stop.store(true, Ordering::Release);
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                }
            }
            for handle in handles {
                if handle.join().is_err() && failure.is_none() {
                    failure = Some(io::Error::other("후보 worker 정리 실패"));
                }
            }
            match failure {
                Some(e) => Err(e),
                None => Ok(()),
            }
        })?;
    }
    for file in &result.files {
        let folder = &mut result.folders[file.parent];
        folder.logical_bytes = folder
            .logical_bytes
            .checked_add(file.logical_bytes)
            .ok_or_else(invalid)?;
        folder.file_count += 1;
        folder.allocation.add(file.extra.allocation)?;
    }
    crate::output::check(&cancelled)?;
    crate::scanner::finish(&mut result)?;
    Ok(result)
}
