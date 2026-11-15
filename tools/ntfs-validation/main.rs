#![allow(dead_code)]
#[cfg(test)]
#[path = "../../src/file_open.rs"]
mod file_open;
#[cfg(test)]
#[path = "../../src/global_search.rs"]
mod global_search;
#[cfg(test)]
#[path = "../../src/local_date.rs"]
mod local_date;
#[cfg(test)]
#[path = "../../src/search.rs"]
mod search;
#[path = "../../src/usn.rs"]
mod usn;
mod usn_cases;
#[path = "../../src/usn_native.rs"]
mod usn_native;

#[path = "../../src/engine.rs"]
mod engine;
#[path = "../../src/explorer.rs"]
mod explorer;
#[path = "../../src/index_store.rs"]
mod index_store;
#[path = "../../src/locations.rs"]
mod locations;
#[path = "../../src/metadata.rs"]
mod metadata;
#[path = "../../src/ntfs.rs"]
mod ntfs;
#[path = "../../src/ntfs_helper.rs"]
mod ntfs_helper;
#[path = "../../src/ntfs_native.rs"]
mod ntfs_native;
#[path = "../../src/output.rs"]
mod output;
#[path = "../../src/scan_job.rs"]
mod scan_job;
#[path = "../../src/scanner.rs"]
mod scanner;
#[path = "../../src/snapshot.rs"]
mod snapshot;

use std::{
    collections::BTreeMap,
    io,
    path::{Path, PathBuf},
    time::Instant,
};
struct Timed {
    source: ntfs_native::Native,
    enumeration: f64,
    metadata: f64,
    links: f64,
}
impl ntfs::Source for Timed {
    fn root_id(&mut self) -> io::Result<ntfs::Id> {
        self.source.root_id()
    }
    fn page(&mut self) -> io::Result<Option<Vec<u8>>> {
        let t = Instant::now();
        let r = self.source.page();
        self.enumeration += t.elapsed().as_secs_f64() * 1000.0;
        r
    }
    fn inspect(&mut self, path: &Path, detailed: bool) -> io::Result<ntfs::Info> {
        let t = Instant::now();
        let r = self.source.inspect(path, detailed);
        self.metadata += t.elapsed().as_secs_f64() * 1000.0;
        r
    }
    fn links(&mut self, path: &Path) -> io::Result<Vec<PathBuf>> {
        let t = Instant::now();
        let r = self.source.links(path);
        self.links += t.elapsed().as_secs_f64() * 1000.0;
        r
    }
}
type Row = (bool, u64, u64, u64, metadata::Allocation);
fn rows(scan: &scanner::ScanResult) -> BTreeMap<PathBuf, Row> {
    let root = &scan.folders[0].path;
    let mut rows = BTreeMap::new();
    for f in &scan.folders {
        rows.insert(
            f.path.strip_prefix(root).unwrap().to_owned(),
            (
                true,
                f.logical_bytes,
                f.file_count,
                f.folder_count,
                f.allocation,
            ),
        );
    }
    for f in &scan.files {
        rows.insert(
            scan.folders[f.parent]
                .path
                .join(&f.name)
                .strip_prefix(root)
                .unwrap()
                .to_owned(),
            (false, f.logical_bytes, 1, 0, {
                let mut a = metadata::Allocation::default();
                a.add(f.extra.allocation).unwrap();
                a
            }),
        );
    }
    rows
}
fn main() {
    if let Err(e) = run() {
        eprintln!("{e}");
        std::process::exit(1);
    }
}
fn run() -> io::Result<()> {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    if args.first().is_some_and(|a| a == "--ntfs-worker") {
        return ntfs_helper::worker(&args[1..]);
    }
    if args.len() != 1 {
        return Err(io::Error::other("새 임시 VHD의 요청 파일이 필요함"));
    }
    let request: serde_json::Value =
        serde_json::from_reader(std::fs::File::open(&args[0])?).map_err(output::json_error)?;
    let get = |name: &str| {
        request[name]
            .as_str()
            .ok_or_else(|| io::Error::other("시험 요청 필드 누락"))
    };
    let root = Path::new(get("root")?);
    let target = ntfs_native::target(root)?;
    let token = get("token")?;
    if target.volume != get("volume")?
        || target.root[..2]
            .eq_ignore_ascii_case(&std::env::var("SystemDrive").unwrap_or_else(|_| "C:".into()))
        || token.len() != 32
        || !token.bytes().all(|b| b.is_ascii_hexdigit())
    {
        return Err(io::Error::other("독립 시험 볼륨 확인 실패"));
    }
    if std::fs::read_to_string(root.join(format!(".diskweave-validation-{token}")))? != token {
        return Err(io::Error::other("시험 볼륨 소유 표식 불일치"));
    }
    if request["usn"].as_bool() == Some(true) {
        return usn_cases::run(root, &target);
    }
    let mut timings = Vec::new();
    for round in 0..3 {
        let t = Instant::now();
        let basic = scanner::scan_with_options(root, true, || false, |_| {})?;
        let basic_ms = t.elapsed().as_secs_f64() * 1000.0;
        let t = Instant::now();
        let outcome = engine::scan_with(
            root,
            engine::Method::Ntfs,
            true,
            || false,
            |p| eprintln!("{:?}: {}", p.stage, p.counts.files_scanned),
            ntfs_helper::scan,
        )?;
        let fast_ms = t.elapsed().as_secs_f64() * 1000.0;
        if outcome.report.actual != engine::Method::Ntfs {
            return Err(io::Error::other(format!(
                "고속 경로 미실행: {}",
                outcome.report.label()
            )));
        }
        let expected = rows(&basic);
        let observed = rows(&outcome.result);
        if expected != observed {
            let differences: Vec<_> = expected
                .keys()
                .chain(observed.keys())
                .filter(|k| expected.get(*k) != observed.get(*k))
                .take(20)
                .map(|k| {
                    format!(
                        "{}: {:?} / {:?}",
                        k.display(),
                        expected.get(k),
                        observed.get(k)
                    )
                })
                .collect();
            return Err(io::Error::other(format!(
                "기본/NTFS 결과 불일치: {}",
                differences.join("\n")
            )));
        }
        for (path, bytes) in [
            ("Cases/regular.bin", 13),
            ("Cases/sub/alias.bin", 13),
            ("Cases/empty", 0),
            ("Cases/sparse.bin", 4_194_304),
        ] {
            if observed
                .get(Path::new(path))
                .is_none_or(|row| row.0 || row.1 != bytes)
            {
                return Err(io::Error::other(format!("독립 manifest 불일치: {path}")));
            }
        }
        let bulk = request["file_count"].as_u64().unwrap_or(0);
        if observed
            .iter()
            .filter(|(p, row)| p.starts_with("Cases/bulk") && !row.0)
            .count() as u64
            != bulk
        {
            return Err(io::Error::other("독립 bulk manifest 개수 불일치"));
        }
        for i in 0..bulk {
            let path = PathBuf::from(format!("Cases/bulk/entry-{i:05}.bin"));
            if observed.get(&path).is_none_or(|r| r.0 || r.1 != 8) {
                return Err(io::Error::other("독립 bulk manifest 크기 불일치"));
            }
        }
        let mut timed = Timed {
            source: ntfs_native::Native::open(&target)?,
            enumeration: 0.0,
            metadata: 0.0,
            links: 0.0,
        };
        let t = Instant::now();
        let direct = ntfs::collect(root, true, &mut timed, || false, |_| {})?;
        let direct_ms = t.elapsed().as_secs_f64() * 1000.0;
        if rows(&direct) != expected {
            return Err(io::Error::other("직접 수집기와 기본 결과 불일치"));
        }
        timings.push(serde_json::json!({"round":round+1,"basic_ms":basic_ms,"ntfs_with_helper_ms":fast_ms,"direct_ntfs_ms":direct_ms,"enum_api_ms":timed.enumeration,"metadata_api_ms":timed.metadata,"link_api_ms":timed.links,"files":basic.files.len(),"folders":basic.folders.len(),"errors":basic.error_count,"reparse_excluded":basic.skipped_reparse_points}));
    }
    let mut cancellation = Vec::new();
    for stage in [engine::Stage::Enumerating, engine::Stage::Transferring] {
        let cancel = std::cell::Cell::new(false);
        let fallback = std::cell::Cell::new(false);
        let started = Instant::now();
        let result = engine::scan_with(
            root,
            engine::Method::Ntfs,
            true,
            || cancel.get(),
            |p| {
                if p.stage == stage {
                    cancel.set(true);
                }
                if p.stage == engine::Stage::Fallback {
                    fallback.set(true);
                }
            },
            ntfs_helper::scan,
        );
        if !cancel.get()
            || fallback.get()
            || result
                .err()
                .is_none_or(|e| e.kind() != io::ErrorKind::Interrupted)
        {
            return Err(io::Error::other(
                "실제 도우미 취소·기본 재스캔 금지 검증 실패",
            ));
        }
        cancellation.push(serde_json::json!({"stage":format!("{stage:?}"),"total_ms":started.elapsed().as_secs_f64()*1000.0,"fallback":false}));
    }
    println!(
        "{}",
        serde_json::json!({"root":target.root,"volume":target.volume,"timings":timings,"cancellation":cancellation,"verified":"실제 새 NTFS VHD에서 전체 경로·논리/할당 합계 및 독립 manifest 대조"})
    );
    Ok(())
}
