use std::{
    cell::Cell,
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
    fixture
        .strip_prefix(Path::new("C:\\"))
        .map_err(io::Error::other)?;
    fs::create_dir(&fixture)?;
    let file = fixture.join("probe.bin");
    let start = Instant::now();
    let stop = AtomicBool::new(false);
    let mut stage = 0;
    let mut failure = None;
    let mut observations = Vec::new();
    let mut changed_at = None::<Instant>;
    let holding_since = Cell::new(None::<Instant>);
    let cancelled_at = Cell::new(None::<Instant>);
    let mut stable_records = 0u64;
    let mut stable_cursors = 0u64;
    let stability = Duration::from_secs(1800);
    eprintln!("C whole-drive: read-only baseline started");
    let mut progress = Instant::now();
    let result = crate::ntfs_helper::watch(
        Path::new("C:\\"),
        true,
        None,
        crate::ntfs_helper::WatchControl {
            cancelled: &|| {
                if holding_since
                    .get()
                    .is_some_and(|time| time.elapsed() >= stability)
                {
                    if cancelled_at.get().is_none() {
                        cancelled_at.set(Some(Instant::now()));
                        eprintln!("C whole-drive: cancellation requested after stability window");
                    }
                    return true;
                }
                stop.load(Ordering::Relaxed) || start.elapsed() > Duration::from_secs(3900)
            },
            ready: &|| true,
        },
        |update| {
            let crate::usn::WatchUpdate::Record(record) = update else {
                if holding_since.get().is_some() {
                    stable_cursors += 1;
                }
                return Ok(());
            };
            let validation = record.tracking.as_ref().unwrap().validate(&record.scan);
            if let Err(e) = validation {
                failure = Some(e);
                stop.store(true, Ordering::Relaxed);
                return Ok(());
            }
            if holding_since.get().is_some() {
                stable_records += 1;
                return Ok(());
            }
            let relative = file
                .strip_prefix(Path::new("C:\\"))
                .map_err(io::Error::other)?;
            let bytes = record
                .entries
                .as_ref()
                .and_then(|entries| entries.path_key(relative).and_then(|key| entries.item(key)))
                .and_then(|item| match item {
                    crate::explorer::Item::File(row) => Some(record.scan.files[row].logical_bytes),
                    _ => None,
                });
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
                "seconds":start.elapsed().as_secs_f64(),"change_delay_seconds":changed_at.map(|time| time.elapsed().as_secs_f64()),"checkpoint_valid":true
            }));
            let change = match stage {
                0 => fs::write(&file, b"abc"),
                1 => fs::write(&file, b"12345678901234567"),
                2 => fs::remove_file(&file),
                _ => {
                    holding_since.set(Some(Instant::now()));
                    eprintln!("C whole-drive: stability observation started");
                    Ok(())
                }
            };
            if let Err(e) = change {
                failure = Some(e);
                stop.store(true, Ordering::Relaxed);
            }
            changed_at = Some(Instant::now());
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
    let cancellation_seconds = cancelled_at.get().map(|time| time.elapsed().as_secs_f64());
    // 이 함수에서 새로 만든 소유 파일과 빈 폴더만 정리.
    if file.exists() {
        fs::remove_file(&file)?;
    }
    fs::remove_dir(&fixture)?;
    let stability_seconds = holding_since.get().map(|time| time.elapsed().as_secs_f64());
    let cancelled = result
        .as_ref()
        .is_err_and(|error| error.kind() == io::ErrorKind::Interrupted);
    let success = stage == 4
        && failure.is_none()
        && cancelled_at.get().is_some()
        && cancelled
        && stable_records + stable_cursors > 0;
    serde_json::to_writer_pretty(fs::File::create(output)?, &serde_json::json!({
        "success":success,"observations":observations,
        "error":failure.as_ref().map(ToString::to_string).or_else(|| (!success).then(|| format!("{result:?}"))),
        "fixture_removed":!fixture.exists(),"journal_modified":false,
        "stability_seconds":stability_seconds,"stability_records":stable_records,"stability_cursors":stable_cursors,
        "cancelled":cancelled,"cancellation_seconds":cancellation_seconds,
        "seconds":start.elapsed().as_secs_f64()
    })).map_err(io::Error::other)?;
    if !success {
        return Err(failure.unwrap_or_else(|| {
            io::Error::other(format!("C드라이브 반영 대조 미완료: {result:?}"))
        }));
    }
    Ok(())
}
