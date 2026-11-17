use std::{
    fs, io,
    path::Path,
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

/// 기존 C 볼륨은 읽기만 수행하고 새 소유 임시 폴더에서만 변경 반영을 대조.
pub fn run(output: &Path) -> io::Result<()> {
    let owned = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/rust-recovery");
    if output.parent().map(fs::canonicalize).transpose()? != Some(fs::canonicalize(owned)?) {
        return Err(io::Error::other(
            "시험 결과는 프로젝트 시험 폴더에만 저장 가능",
        ));
    }
    let token = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let fixture =
        std::env::temp_dir().join(format!("diskweave-live-{}-{token}", std::process::id()));
    fs::create_dir(&fixture)?;
    let file = fixture.join("probe.bin");
    let start = Instant::now();
    let stop = AtomicBool::new(false);
    let mut stage = 0;
    let mut failure = None;
    let mut observations = Vec::new();
    eprintln!("C whole-drive: read-only baseline started");
    let mut progress = Instant::now();
    let result = crate::ntfs_helper::watch(
        Path::new("C:\\"),
        true,
        None,
        crate::ntfs_helper::WatchControl {
            cancelled: &|| {
                stop.load(Ordering::Relaxed) || start.elapsed() > Duration::from_secs(1800)
            },
            ready: &|| true,
        },
        |update| {
            let crate::usn::WatchUpdate::Record(record) = update else {
                return Ok(());
            };
            let validation = record.tracking.as_ref().unwrap().validate(&record.scan);
            if let Err(e) = validation {
                failure = Some(e);
                stop.store(true, Ordering::Relaxed);
                return Ok(());
            }
            let found = record
                .scan
                .files
                .iter()
                .find(|f| record.scan.folders[f.parent].path.join(&f.name) == file);
            let bytes = found.map(|f| f.logical_bytes);
            let expected = match stage {
                1 => Some(3),
                2 => Some(17),
                _ => None,
            };
            if bytes != expected {
                return Ok(());
            }
            let unavailable = &record.tracking.as_ref().unwrap().unavailable;
            eprintln!(
                "C whole-drive: stage {stage}, files {}, unavailable {}",
                record.scan.files.len(),
                unavailable.len()
            );
            observations.push(serde_json::json!({
                "stage":stage,"fixture_bytes":bytes,"files":record.scan.files.len(),
                "folders":record.scan.folders.len(),"unavailable":unavailable.len(),
                "system_files_excluded":unavailable.iter().filter(|e| ["pagefile.sys","swapfile.sys","hiberfil.sys"].iter().any(|p| e.relative()==Path::new(p))).count(),
                "seconds":start.elapsed().as_secs_f64(),"checkpoint_valid":true
            }));
            let change = match stage {
                0 => fs::write(&file, b"abc"),
                1 => fs::write(&file, b"12345678901234567"),
                2 => fs::remove_file(&file),
                _ => {
                    stop.store(true, Ordering::Relaxed);
                    Ok(())
                }
            };
            if let Err(e) = change {
                failure = Some(e);
                stop.store(true, Ordering::Relaxed);
            }
            stage += 1;
            Ok(())
        },
        |event| {
            if progress.elapsed() >= Duration::from_secs(10) {
                eprintln!(
                    "C whole-drive: {:?}, files {}, folders {}",
                    event.stage, event.counts.files_scanned, event.counts.folders_scanned
                );
                progress = Instant::now();
            }
        },
    );
    // 이 함수에서 새로 만든 소유 파일과 빈 폴더만 정리.
    if file.exists() {
        fs::remove_file(&file)?;
    }
    fs::remove_dir(&fixture)?;
    let success = stage == 4 && failure.is_none() && result.is_err();
    serde_json::to_writer_pretty(fs::File::create(output)?, &serde_json::json!({
        "success":success,"observations":observations,
        "error":failure.as_ref().map(ToString::to_string).or_else(|| (!success).then(|| format!("{result:?}"))),
        "fixture_removed":!fixture.exists(),"journal_modified":false,
        "seconds":start.elapsed().as_secs_f64()
    })).map_err(io::Error::other)?;
    if !success {
        return Err(failure.unwrap_or_else(|| {
            io::Error::other(format!("C드라이브 반영 대조 미완료: {result:?}"))
        }));
    }
    Ok(())
}
