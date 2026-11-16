#![allow(dead_code)]
// USN 변경 추적·파일 관리의 독립 시험 볼륨 검증 전용 실행기.
#[path = "../../src/engine.rs"]
mod engine;
#[path = "../../src/explorer.rs"]
mod explorer;
mod file_cases;
#[cfg(test)]
#[path = "../../src/file_open.rs"]
mod file_open;
#[path = "../../src/file_ops.rs"]
mod file_ops;
#[path = "../../src/global_search.rs"]
mod global_search;
#[path = "../../src/local_date.rs"]
mod local_date;
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
#[path = "../../src/recycle.rs"]
mod recycle;
#[path = "../../src/scan_job.rs"]
mod scan_job;
#[path = "../../src/scanner.rs"]
mod scanner;
#[path = "../../src/search.rs"]
mod search;
#[path = "../../src/snapshot.rs"]
mod snapshot;
mod system_watch;
#[path = "../../src/tracking.rs"]
mod tracking;
#[path = "../../src/usn.rs"]
mod usn;
mod usn_cases;
#[path = "../../src/usn_native.rs"]
mod usn_native;
use std::{
    collections::BTreeMap,
    io,
    path::{Path, PathBuf},
};
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
    if args.len() == 2 && args[0] == "--system-watch" {
        return system_watch::run(Path::new(&args[1]));
    }
    if args.len() != 1 {
        return Err(io::Error::other("독립 시험 볼륨의 요청 파일이 필요합니다"));
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
    if request["file_management"].as_bool() == Some(true) {
        let parent = Path::new(&args[0])
            .parent()
            .ok_or_else(|| io::Error::other("시험 요청 경로 확인 실패"))?;
        let parent = std::fs::canonicalize(parent)?;
        let owned = std::fs::canonicalize(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/ntfs-validation"),
        )?;
        if !parent.starts_with(&owned) || parent.file_name().and_then(|n| n.to_str()) != Some(token)
        {
            return Err(io::Error::other("별도 시험 작업 폴더 확인 실패"));
        }
        return file_cases::run(root, &parent);
    }
    if request["usn"].as_bool() == Some(true) {
        return usn_cases::run(root, &target);
    }
    Err(io::Error::other(
        "이 실행기는 USN·파일 관리 시험만 지원합니다",
    ))
}
