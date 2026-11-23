mod full_hash;

use crate::{explorer::Item, locations::Snapshot, metadata::Identity, search::item_path};
use std::{
    collections::BTreeMap,
    fs::File,
    io::{self, Read, Seek, SeekFrom},
    os::windows::{
        fs::{MetadataExt, OpenOptionsExt},
        io::AsRawHandle,
    },
    path::PathBuf,
};
use windows_sys::Win32::Storage::FileSystem::{
    FileIdInfo, GetFileInformationByHandleEx, FILE_ATTRIBUTE_OFFLINE,
    FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS, FILE_FLAG_OPEN_NO_RECALL, FILE_FLAG_OPEN_REPARSE_POINT,
    FILE_ID_INFO,
};
const REMOTE_CONTENT: u32 = FILE_ATTRIBUTE_OFFLINE | FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS;
const MIB: u64 = 1024 * 1024;
const SAMPLE_BYTES: usize = 64 * 1024;
const FULL_IN_SAMPLE: u64 = (2 * SAMPLE_BYTES) as u64;
const FULL_HASH_LIMIT: usize = 8;

fn full_hash_workers(file_bytes: u64, candidates: usize, limit: usize) -> usize {
    // 중간 크기 파일에서 작업 스레드 생성과 동시 읽기 부담이 커지는 구간 제외.
    if limit <= 1
        || file_bytes < 8 * MIB
        || candidates < 4
        || file_bytes.saturating_mul(candidates as u64) < 128 * MIB
    {
        1
    } else {
        candidates.min(limit)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    All,
    Folder { location: u64, folder: usize },
}
#[derive(Clone, Debug)]
pub struct Member {
    pub source: usize,
    pub file: usize,
    pub path: PathBuf,
    pub bytes: u64,
    stamp: Stamp,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Stamp {
    id: Identity,
    bytes: u64,
    modified: std::time::SystemTime,
}
pub struct Issue {
    pub path: PathBuf,
    pub reason: String,
}
pub struct Report {
    pub sources: Vec<Snapshot>,
    pub groups: Vec<Vec<Member>>,
    pub links: Vec<Vec<Member>>,
    pub issues: Vec<Issue>,
    pub empty: u64,
    pub progress: Progress,
}
#[derive(Clone, Copy, Default)]
pub struct Progress {
    pub stage: &'static str,
    pub files: u64,
    pub bytes: u64,
}

fn stamp(file: &File) -> io::Result<Stamp> {
    let m = file.metadata()?;
    if !m.is_file() || m.file_attributes() & 0x400 != 0 {
        return Err(io::Error::other("일반 파일이 아니거나 링크 경로로 변경됨"));
    }
    if m.file_attributes() & REMOTE_CONTENT != 0 {
        return Err(io::Error::other("로컬에 없는 파일 내용은 검사하지 않음"));
    }
    let mut id = FILE_ID_INFO::default();
    // 파일 내용과 같은 핸들에서 전체 ID 조회. 경로 교체와 Hard Link 구분에 사용
    if unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle(),
            FileIdInfo,
            (&mut id as *mut FILE_ID_INFO).cast(),
            std::mem::size_of_val(&id) as u32,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(Stamp {
        id: Identity {
            volume: id.VolumeSerialNumber,
            file: id.FileId.Identifier,
        },
        bytes: m.len(),
        modified: m.modified()?,
    })
}
fn open(member: &Member) -> io::Result<File> {
    for parent in member.path.ancestors() {
        let attributes = std::fs::symlink_metadata(parent)?.file_attributes();
        if attributes & REMOTE_CONTENT != 0 {
            return Err(io::Error::other("로컬에 없는 파일 내용은 검사하지 않음"));
        }
        if attributes & 0x400 != 0 {
            return Err(io::Error::other("Junction·Symbolic Link 경로 제외"));
        }
    }
    let f = File::options()
        .read(true)
        .share_mode(1)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_OPEN_NO_RECALL)
        .open(&member.path)?;
    if stamp(&f)? != member.stamp {
        return Err(io::Error::other(
            "분석 후 파일이 변경되거나 교체됨 · 재검사 필요",
        ));
    }
    Ok(f)
}
fn digest(
    member: &Member,
    partial: bool,
    buffer: &mut [u8],
    cancelled: &impl Fn() -> bool,
    progress: &mut Progress,
    notify: &mut impl FnMut(Progress),
) -> io::Result<[u8; 32]> {
    let mut f = open(member)?;
    let mut hash = blake3::Hasher::new();
    if partial && member.bytes > FULL_IN_SAMPLE {
        for offset in [0, member.bytes - SAMPLE_BYTES as u64] {
            crate::output::check(cancelled)?;
            f.seek(SeekFrom::Start(offset))?;
            f.read_exact(&mut buffer[..SAMPLE_BYTES])?;
            hash.update(&buffer[..SAMPLE_BYTES]);
            progress.bytes += SAMPLE_BYTES as u64;
            notify(*progress);
        }
    } else {
        loop {
            crate::output::check(cancelled)?;
            let n = f.read(buffer)?;
            if n == 0 {
                break;
            }
            hash.update(&buffer[..n]);
            progress.bytes += n as u64;
            notify(*progress);
        }
    }
    if stamp(&f)? != member.stamp {
        return Err(io::Error::other("읽는 동안 파일이 변경됨"));
    }

    notify(*progress);
    Ok(*hash.finalize().as_bytes())
}
pub fn inspect(
    sources: Vec<Snapshot>,
    scope: Scope,
    cancelled: &impl Fn() -> bool,
    notify: &mut impl FnMut(Progress),
) -> io::Result<Report> {
    let limit =
        std::thread::available_parallelism().map_or(1, |count| count.get().min(FULL_HASH_LIMIT));
    inspect_with_limit(sources, scope, limit, cancelled, notify)
}

fn inspect_with_limit(
    sources: Vec<Snapshot>,
    scope: Scope,
    limit: usize,
    cancelled: &impl Fn() -> bool,
    notify: &mut impl FnMut(Progress),
) -> io::Result<Report> {
    let mut report = Report {
        sources,
        groups: vec![],
        links: vec![],
        issues: vec![],
        empty: 0,
        progress: Progress {
            stage: "크기 비교",
            ..Default::default()
        },
    };
    if let Scope::Folder { location, folder } = scope {
        if !report
            .sources
            .iter()
            .any(|s| s.id == location && folder < s.scan.folders.len())
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "검사 범위가 현재 분석에 없음",
            ));
        }
    }
    let mut sizes: BTreeMap<u64, Vec<(usize, usize, Stamp)>> = BTreeMap::new();
    for (source, snapshot) in report.sources.iter().enumerate() {
        let mut collect = |file: usize| -> io::Result<()> {
            crate::output::check(cancelled)?;
            let entry = &snapshot.scan.files[file];
            report.progress.files += 1;
            if entry.logical_bytes == 0 {
                report.empty += 1;
                return Ok(());
            }
            let path = || item_path(&snapshot.scan, Item::File(file));
            if entry.attributes & REMOTE_CONTENT != 0 {
                report.issues.push(Issue {
                    path: path(),
                    reason: "로컬에 없는 파일 내용은 검사하지 않음".into(),
                });
                return Ok(());
            }
            let Some(id) = entry.extra.identity.value() else {
                report.issues.push(Issue {
                    path: path(),
                    reason: "파일 ID 미확인".into(),
                });
                return Ok(());
            };
            let Some(modified) = entry.modified else {
                report.issues.push(Issue {
                    path: path(),
                    reason: "수정 시각 미확인".into(),
                });
                return Ok(());
            };

            sizes.entry(entry.logical_bytes).or_default().push((
                source,
                file,
                Stamp {
                    id,
                    bytes: entry.logical_bytes,
                    modified,
                },
            ));
            Ok(())
        };
        match scope {
            Scope::Folder { location, folder } if snapshot.id == location => {
                let mut pending = vec![folder];
                while let Some(folder) = pending.pop() {
                    crate::output::check(cancelled)?;
                    for &file in snapshot.scan.file_children[folder].iter() {
                        collect(file)?;
                    }
                    pending.extend(snapshot.scan.children[folder].iter().copied());
                }
            }
            Scope::Folder { .. } => {}
            Scope::All => {
                for file in 0..snapshot.scan.files.len() {
                    collect(file)?;
                }
            }
        }
    }
    let mut buffer = vec![0; MIB as usize];
    for (size, members) in sizes.into_iter().filter(|(_, members)| members.len() > 1) {
        let mut objects: std::collections::HashMap<Identity, Vec<Member>> =
            std::collections::HashMap::new();
        for (source, file, stamp) in members {
            crate::output::check(cancelled)?;
            let member = Member {
                source,
                file,
                path: item_path(&report.sources[source].scan, Item::File(file)),
                bytes: stamp.bytes,
                stamp,
            };
            match open(&member) {
                Ok(_) => objects.entry(member.stamp.id).or_default().push(member),
                Err(e) => report.issues.push(Issue {
                    path: member.path,
                    reason: e.to_string(),
                }),
            }
        }
        let mut objects: Vec<_> = objects.into_values().collect();
        objects.sort_by(|a, b| a[0].path.cmp(&b[0].path));
        for object in &objects {
            if object.len() > 1 {
                report.links.push(object.clone());
            }
        }
        if objects.len() < 2 {
            continue;
        }
        report.progress.stage = "일부 내용 확인";
        let mut partials: BTreeMap<[u8; 32], Vec<Vec<Member>>> = BTreeMap::new();
        for object in objects {
            match digest(
                &object[0],
                true,
                &mut buffer,
                cancelled,
                &mut report.progress,
                notify,
            ) {
                Ok(hash) => partials.entry(hash).or_default().push(object),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => return Err(e),
                Err(e) => {
                    for member in object {
                        report.issues.push(Issue {
                            path: member.path,
                            reason: e.to_string(),
                        });
                    }
                }
            }
        }
        report.progress.stage = "전체 내용 확인";
        partials.retain(|_, objects| objects.len() > 1);
        let candidates = partials.values().map(Vec::len).sum();
        let workers = full_hash_workers(size, candidates, limit);
        let mut parallel_hashes = if workers > 1 {
            let jobs: Vec<_> = partials
                .values()
                .flatten()
                .map(|object| &object[0])
                .collect();
            Some(
                full_hash::calculate(&jobs, workers, cancelled, &mut report.progress, notify)?
                    .into_iter(),
            )
        } else {
            None
        };
        for (partial, objects) in partials {
            let mut hashes: BTreeMap<[u8; 32], Vec<Vec<Member>>> = BTreeMap::new();
            for object in objects {
                let hash = if size <= FULL_IN_SAMPLE {
                    Ok(partial)
                } else if let Some(hashes) = &mut parallel_hashes {
                    hashes.next().expect("전체 해시 결과 수와 작업 수 일치")
                } else {
                    digest(
                        &object[0],
                        false,
                        &mut buffer,
                        cancelled,
                        &mut report.progress,
                        notify,
                    )
                };
                match hash {
                    Ok(hash) => hashes.entry(hash).or_default().push(object),
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => return Err(e),
                    Err(e) => {
                        for member in object {
                            report.issues.push(Issue {
                                path: member.path,
                                reason: e.to_string(),
                            });
                        }
                    }
                }
            }
            report.groups.extend(
                hashes
                    .into_values()
                    .filter(|v| v.len() > 1)
                    .map(|v| v.into_iter().flatten().collect()),
            );
        }
    }
    report.progress.stage = "결과 재확인";
    let mut valid = std::collections::HashMap::new();
    for member in report.groups.iter().chain(&report.links).flatten() {
        crate::output::check(cancelled)?;
        if !valid.contains_key(&member.path) {
            let checked = open(member);
            if let Err(e) = &checked {
                report.issues.push(Issue {
                    path: member.path.clone(),
                    reason: e.to_string(),
                });
            }
            valid.insert(member.path.clone(), checked.is_ok());
        }
    }
    for group in report.groups.iter_mut().chain(&mut report.links) {
        group.retain(|m| valid[&m.path]);
    }
    report.groups.retain(|group| {
        group.first().is_some_and(|first| {
            group[1..]
                .iter()
                .any(|member| member.stamp.id != first.stamp.id)
        })
    });
    // 화면 그룹 순서를 해시 값과 작업 완료 순서에서 분리.
    report.groups.sort_by(|a, b| {
        a[0].bytes
            .cmp(&b[0].bytes)
            .then_with(|| a[0].path.cmp(&b[0].path))
    });
    report.links.retain(|group| group.len() > 1);
    let mut reported = std::collections::HashSet::new();
    report
        .issues
        .retain(|issue| reported.insert(issue.path.clone()));
    crate::output::check(cancelled)?;
    Ok(report)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::{fs, path::PathBuf, sync::Arc};

    pub(crate) struct Fixture(pub PathBuf);
    impl Fixture {
        pub fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "diskweave-duplicates-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
        pub fn snapshot(&self) -> crate::locations::Snapshot {
            crate::locations::Snapshot {
                entries: None,
                id: 1,
                generation: 1,
                scan: Arc::new(
                    crate::scanner::scan_with_options(&self.0, true, || false, |_| {}).unwrap(),
                ),
            }
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn streamed_file_digest_matches_the_blake3_reference() {
        let fixture = Fixture::new();
        let path = fixture.0.join("abc.bin");
        fs::write(&path, b"abc").unwrap();
        let member = Member {
            source: 0,
            file: 0,
            stamp: stamp(&File::open(&path).unwrap()).unwrap(),
            path,
            bytes: 3,
        };
        let digest = digest(
            &member,
            false,
            &mut [0; 2],
            &|| false,
            &mut Progress::default(),
            &mut |_| {},
        )
        .unwrap();
        let hex: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
        assert_eq!(
            hex,
            "6437b3ac38465133ffb63b75273a8db548c558465d79db03fd359c6cd5bd9d85"
        );
    }

    #[test]
    fn full_hash_workers_require_enough_large_candidates_and_respect_limits() {
        assert_eq!(full_hash_workers(8 * MIB - 1, 128, 8), 1);
        assert_eq!(full_hash_workers(64 * MIB, 3, 8), 1);
        assert_eq!(full_hash_workers(32 * MIB - 1, 4, 8), 1);
        assert_eq!(full_hash_workers(32 * MIB, 4, 8), 4);
        assert_eq!(full_hash_workers(16 * MIB, 8, 4), 4);
        assert_eq!(full_hash_workers(16 * MIB, 8, 8), 8);
        assert_eq!(full_hash_workers(16 * MIB, 8, 1), 1);
    }

    #[test]
    fn parallel_large_files_keep_duplicate_groups_links_and_cancel_safely() {
        let fixture = Fixture::new();
        for name in ["a", "b", "c", "d"] {
            File::create(fixture.0.join(name))
                .unwrap()
                .set_len(32 * MIB)
                .unwrap();
        }
        let mut middle = File::options()
            .write(true)
            .open(fixture.0.join("c"))
            .unwrap();
        middle.seek(SeekFrom::Start(16 * MIB)).unwrap();
        std::io::Write::write_all(&mut middle, &[1]).unwrap();
        drop(middle);
        fs::hard_link(fixture.0.join("a"), fixture.0.join("a-alias")).unwrap();
        let source = fixture.snapshot();
        let bytes = 128 * MIB + 4 * FULL_IN_SAMPLE;
        let run = |limit| {
            inspect_with_limit(
                vec![source.clone()],
                Scope::All,
                limit,
                &|| false,
                &mut |_| {},
            )
            .unwrap()
        };
        let paths = |report: &Report| {
            report
                .groups
                .iter()
                .map(|group| {
                    group
                        .iter()
                        .map(|member| member.path.clone())
                        .collect::<Vec<_>>()
                })
                .collect::<Vec<_>>()
        };
        let expected = run(1);
        for limit in [4, 8] {
            let report = run(limit);
            assert_eq!(paths(&report), paths(&expected));
            assert_eq!(report.groups.len(), 1);
            assert_eq!(report.groups[0].len(), 4);
            assert!(report.groups[0]
                .iter()
                .all(|file| !file.path.ends_with("c")));
            assert_eq!(report.links.len(), 1);
            assert_eq!(report.links[0].len(), 2);
            assert_eq!(report.progress.bytes, bytes);
            assert!(report.issues.is_empty());

            let stopped = std::cell::Cell::new(false);
            let cancelled = inspect_with_limit(
                vec![source.clone()],
                Scope::All,
                limit,
                &|| stopped.get(),
                &mut |progress| {
                    if progress.stage == "전체 내용 확인" && progress.bytes > 4 * FULL_IN_SAMPLE
                    {
                        stopped.set(true);
                    }
                },
            );
            assert!(stopped.get());
            assert_eq!(cancelled.err().unwrap().kind(), io::ErrorKind::Interrupted);
        }
        for path in ["a", "b", "c", "d"] {
            File::options()
                .read(true)
                .write(true)
                .share_mode(0)
                .open(fixture.0.join(path))
                .unwrap();
        }
        assert_eq!(paths(&run(4)), paths(&expected));
    }

    #[test]
    fn content_groups_ignore_names_and_reject_same_size_different_content() {
        let fixture = Fixture::new();
        fs::write(fixture.0.join("alpha.txt"), b"same contents").unwrap();
        fs::write(fixture.0.join("renamed.bin"), b"same contents").unwrap();
        fs::write(fixture.0.join("different"), b"diff contents").unwrap();
        fs::write(fixture.0.join("empty"), b"").unwrap();
        let report = inspect(vec![fixture.snapshot()], Scope::All, &|| false, &mut |_| {}).unwrap();
        assert_eq!(report.groups.len(), 1);
        assert_eq!(report.groups[0].len(), 2);
        assert_eq!(report.empty, 1);
        assert_eq!(
            report.progress.files, 4,
            "범위에 포함된 파일을 경로별로 한 번씩 계산"
        );
        assert!(report.issues.is_empty());
    }

    #[test]
    fn hard_links_are_one_object_and_unique_sizes_do_not_read_content() {
        let fixture = Fixture::new();
        let a = fixture.0.join("one");
        fs::write(&a, b"linked content").unwrap();
        fs::hard_link(&a, fixture.0.join("alias")).unwrap();
        fs::write(fixture.0.join("unique"), vec![7; 300_000]).unwrap();
        let report = inspect(vec![fixture.snapshot()], Scope::All, &|| false, &mut |_| {}).unwrap();
        assert!(report.groups.is_empty());
        assert_eq!(report.links.len(), 1);
        assert_eq!(report.links[0].len(), 2);
        assert_eq!(report.progress.bytes, 0);
    }

    #[test]
    fn partial_filter_avoids_full_reads_but_middle_differences_are_not_duplicates() {
        let fixture = Fixture::new();
        for (name, byte) in [("a", 1), ("b", 2), ("c", 3)] {
            fs::write(fixture.0.join(name), vec![byte; 1_000_000]).unwrap();
        }
        let report = inspect(vec![fixture.snapshot()], Scope::All, &|| false, &mut |_| {}).unwrap();
        assert!(report.groups.is_empty());
        assert!(
            report.progress.bytes < 500_000,
            "다른 앞/뒤 내용을 확인한 파일 전체를 읽지 않음"
        );
        let mut bytes = vec![1; 300_000];
        fs::write(fixture.0.join("same-ends-a"), &bytes).unwrap();
        bytes[150_000] = 2;
        fs::write(fixture.0.join("same-ends-b"), &bytes).unwrap();
        let report = inspect(vec![fixture.snapshot()], Scope::All, &|| false, &mut |_| {}).unwrap();
        assert!(
            report.groups.is_empty(),
            "중간 내용 차이는 전체 해시로 구분"
        );
    }

    #[test]
    fn folder_scope_is_recursive_and_does_not_include_siblings_or_other_locations() {
        let fixture = Fixture::new();
        fs::create_dir_all(fixture.0.join("inside/deep")).unwrap();
        fs::write(fixture.0.join("inside/a"), b"same").unwrap();
        fs::write(fixture.0.join("inside/deep/b"), b"same").unwrap();
        fs::write(fixture.0.join("outside"), b"same").unwrap();
        for i in 0..512 {
            fs::write(fixture.0.join(format!("outside-{i:04}")), b"same").unwrap();
        }
        let snapshot = fixture.snapshot();
        let folder = snapshot
            .scan
            .folder_index(std::path::Path::new("inside"))
            .unwrap();
        let checks = std::cell::Cell::new(0);
        let report = inspect(
            vec![snapshot],
            Scope::Folder {
                location: 1,
                folder,
            },
            &|| {
                checks.set(checks.get() + 1);
                checks.get() > 64
            },
            &mut |_| {},
        )
        .unwrap();
        assert_eq!(report.progress.files, 2);
        assert_eq!(report.groups[0].len(), 2);
        assert!(report.groups[0]
            .iter()
            .all(|m| m.path.starts_with(fixture.0.join("inside"))));
        assert!(inspect(
            vec![fixture.snapshot()],
            Scope::Folder {
                location: 99,
                folder: 0
            },
            &|| false,
            &mut |_| {}
        )
        .is_err());
    }

    #[test]
    fn changing_an_already_hashed_file_is_excluded_before_publication() {
        let fixture = Fixture::new();
        let a = fixture.0.join("a");
        fs::write(&a, b"same").unwrap();
        fs::write(fixture.0.join("b"), b"same").unwrap();
        let mut changed = false;
        let report = inspect(vec![fixture.snapshot()], Scope::All, &|| false, &mut |p| {
            if p.bytes >= 8 && !changed {
                fs::write(&a, b"changed!").unwrap();
                changed = true;
            }
        })
        .unwrap();
        assert!(changed);
        assert!(report.groups.is_empty());
        assert_eq!(report.issues.len(), 1);
    }

    #[test]
    fn old_timestamps_are_valid_and_lock_delete_replace_missing_id_are_excluded() {
        let fixture = Fixture::new();
        for name in ["a", "b"] {
            let p = fixture.0.join(name);
            fs::write(&p, b"same").unwrap();
            File::options()
                .write(true)
                .open(p)
                .unwrap()
                .set_times(
                    std::fs::FileTimes::new()
                        .set_modified(std::time::UNIX_EPOCH - std::time::Duration::from_secs(60)),
                )
                .unwrap();
        }
        let source = fixture.snapshot();
        let report = inspect(vec![source.clone()], Scope::All, &|| false, &mut |_| {}).unwrap();
        assert_eq!(report.groups.len(), 1);
        let locked = File::options()
            .read(true)
            .share_mode(0)
            .open(fixture.0.join("a"))
            .unwrap();
        let report = inspect(vec![source.clone()], Scope::All, &|| false, &mut |_| {}).unwrap();
        assert!(report.groups.is_empty());
        assert_eq!(report.issues.len(), 1);
        drop(locked);
        fs::remove_file(fixture.0.join("a")).unwrap();
        let report = inspect(vec![source.clone()], Scope::All, &|| false, &mut |_| {}).unwrap();
        assert!(report.groups.is_empty());
        assert_eq!(report.issues.len(), 1);
        fs::write(fixture.0.join("a"), b"same").unwrap();
        let report = inspect(vec![source], Scope::All, &|| false, &mut |_| {}).unwrap();
        assert!(report.groups.is_empty());
        assert_eq!(report.issues.len(), 1);
        let mut missing = fixture.snapshot();
        Arc::get_mut(&mut missing.scan).unwrap().files[0]
            .extra
            .identity = Default::default();
        let report = inspect(vec![missing], Scope::All, &|| false, &mut |_| {}).unwrap();
        assert!(report.groups.is_empty());
        assert_eq!(report.issues.len(), 1);
    }

    #[test]
    fn cancelling_during_content_reads_releases_handles_and_allows_a_new_inspection() {
        let fixture = Fixture::new();
        for name in ["a", "b"] {
            fs::write(fixture.0.join(name), vec![3; 2_000_000]).unwrap();
        }
        let cancel = std::cell::Cell::new(false);
        let result = inspect(
            vec![fixture.snapshot()],
            Scope::All,
            &|| cancel.get(),
            &mut |p| {
                if p.bytes > 131072 {
                    cancel.set(true);
                }
            },
        );
        assert_eq!(result.err().unwrap().kind(), io::ErrorKind::Interrupted);
        for name in ["a", "b"] {
            File::options()
                .read(true)
                .write(true)
                .share_mode(0)
                .open(fixture.0.join(name))
                .unwrap();
        }
        let report = inspect(vec![fixture.snapshot()], Scope::All, &|| false, &mut |_| {}).unwrap();
        assert_eq!(report.groups.len(), 1);
    }

    #[test]
    fn changed_hard_link_paths_are_reported_once_each_across_hash_and_final_checks() {
        let fixture = Fixture::new();
        fs::write(fixture.0.join("a"), vec![3; 300_000]).unwrap();
        fs::hard_link(fixture.0.join("a"), fixture.0.join("alias")).unwrap();
        fs::write(fixture.0.join("z"), vec![3; 300_000]).unwrap();
        let mut changed = false;
        let report = inspect(vec![fixture.snapshot()], Scope::All, &|| false, &mut |p| {
            if p.bytes >= 262144 && !changed {
                fs::write(fixture.0.join("a"), b"changed").unwrap();
                changed = true;
            }
        })
        .unwrap();
        assert!(changed);
        assert!(report.groups.is_empty());
        assert!(report.links.is_empty());
        assert_eq!(
            report.issues.len(),
            2,
            "여러 단계에서 확인 불가여도 파일 경로별로 한 번만 집계"
        );
    }

    #[test]
    fn offline_metadata_is_excluded_without_reading_file_content() {
        let fixture = Fixture::new();
        fs::write(fixture.0.join("a"), b"same").unwrap();
        fs::write(fixture.0.join("b"), b"same").unwrap();
        let mut snapshot = fixture.snapshot();
        Arc::get_mut(&mut snapshot.scan).unwrap().files[0].attributes |= 0x1000;
        let report = inspect(vec![snapshot], Scope::All, &|| false, &mut |_| {}).unwrap();
        assert!(report.groups.is_empty());
        assert_eq!(report.issues.len(), 1);
        assert_eq!(report.progress.bytes, 0);
    }

    #[test]
    fn a_folder_replaced_by_a_junction_is_not_followed() {
        let fixture = Fixture::new();
        fs::create_dir(fixture.0.join("folder")).unwrap();
        fs::write(fixture.0.join("folder/a"), b"same").unwrap();
        fs::write(fixture.0.join("folder/b"), b"same").unwrap();
        let source = fixture.snapshot();
        fs::rename(fixture.0.join("folder"), fixture.0.join("original")).unwrap();
        let result = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(fixture.0.join("folder"))
            .arg(fixture.0.join("original"))
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        let report = inspect(vec![source], Scope::All, &|| false, &mut |_| {}).unwrap();
        assert!(report.groups.is_empty());
        assert_eq!(report.issues.len(), 2);
        assert_eq!(report.progress.bytes, 0);
        fs::remove_dir(fixture.0.join("folder")).unwrap();
    }

    #[test]
    fn duplicates_across_locations_keep_sources_and_hard_link_aliases_together() {
        let a = Fixture::new();
        let b = Fixture::new();
        fs::write(a.0.join("a"), b"shared").unwrap();
        fs::hard_link(a.0.join("a"), a.0.join("alias")).unwrap();
        fs::write(b.0.join("other-name"), b"shared").unwrap();
        let mut second = b.snapshot();
        second.id = 2;
        second.generation = 7;
        let report = inspect(
            vec![a.snapshot(), second],
            Scope::All,
            &|| false,
            &mut |_| {},
        )
        .unwrap();
        assert_eq!(report.groups.len(), 1);
        assert_eq!(report.groups[0].len(), 3);
        assert_eq!(report.links.len(), 1);
        assert_eq!(report.links[0].len(), 2);
        assert!(report.groups[0]
            .iter()
            .any(|m| report.sources[m.source].id == 2 && report.sources[m.source].generation == 7));
        assert_eq!(
            report.progress.bytes, 12,
            "Hard Link 경로를 반복 해싱하지 않음"
        );
    }
}
