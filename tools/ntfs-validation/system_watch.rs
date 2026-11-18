use std::{
    cell::Cell,
    fs, io,
    path::Path,
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

fn probe_bytes(record: &crate::snapshot::SavedScan, relative: &Path) -> io::Result<Option<u64>> {
    let invalid = || io::Error::other("시험 경로의 결과 행과 경로 매핑이 일치하지 않습니다");
    let entries = record.entries.as_ref().ok_or_else(invalid)?;
    let scan = &record.scan;
    let Some(key) = entries.path_key(relative) else {
        let actual = scan.folders[0].path.join(relative);
        if let Some(parent) = scan
            .folders
            .iter()
            .position(|folder| Some(folder.path.as_path()) == actual.parent())
        {
            if scan.files.iter().any(|file| {
                file.parent == parent && Some(file.name.as_os_str()) == relative.file_name()
            }) {
                return Err(invalid());
            }
        }
        return Ok(None);
    };
    let Some(crate::explorer::Item::File(row)) = entries.item(key) else {
        return Err(invalid());
    };
    let file = scan.files.get(row).ok_or_else(invalid)?;
    let folder = scan.folders.get(file.parent).ok_or_else(invalid)?;
    if folder.path.join(&file.name) != scan.folders[0].path.join(relative) {
        return Err(invalid());
    }
    Ok(Some(file.logical_bytes))
}

fn publication_times(
    start: Instant,
    changed_at: Option<Instant>,
    published: Instant,
) -> (f64, Option<f64>) {
    (
        published.duration_since(start).as_secs_f64(),
        changed_at.map(|time| published.duration_since(time).as_secs_f64()),
    )
}

/// 기존 C 볼륨은 읽기만 수행하고 새 소유 임시 폴더에서만 변경 반영을 대조.
pub fn run(output: &Path, profile: bool) -> io::Result<()> {
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
    let stability = Duration::from_secs(if profile { 0 } else { 1800 });
    let checking = Cell::new(None::<Instant>);
    let transfer = Cell::new(None::<Instant>);
    let received = Cell::new(None::<Instant>);
    let worker_seconds = Cell::new(None::<f64>);
    let mut cycles = Vec::new();
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
            let published = Instant::now();
            let stages = profile.then(crate::usn::cost::finish);
            let record_received = received.get();
            let crate::usn::WatchUpdate::Record(record) = update.into_update() else {
                if profile && cycles.len() < 4096 {
                    cycles.push(serde_json::json!({
                        "at_seconds": published.duration_since(start).as_secs_f64(),
                        "record": false,
                        "checking_to_stream_seconds": worker_seconds.get(),
                        "stream_to_receive_seconds": transfer.get().zip(record_received).map(|(a,b)| (b-a).as_secs_f64()),
                    }));
                }
                if holding_since.get().is_some() {
                    stable_cursors += 1;
                }
                return Ok(());
            };
            let validation = record.tracking.as_ref().unwrap().validate(&record.scan);
            let audit_seconds = published.elapsed().as_secs_f64();
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
            let bytes = match probe_bytes(&record, relative) {
                Ok(bytes) => bytes,
                Err(error) => {
                    failure = Some(error);
                    stop.store(true, Ordering::Relaxed);
                    return Ok(());
                }
            };
            let expected = match stage {
                1 => Some(3),
                2 => Some(17),
                _ => None,
            };
            if profile && cycles.len() < 4096 {
                let cycle = serde_json::json!({
                    "at_seconds": published.duration_since(start).as_secs_f64(),
                    "waiting_for_stage": stage,
                    "matched": bytes == expected,
                    "record": true,
                    "checking_to_stream_seconds": worker_seconds.get(),
                    "stream_to_receive_seconds": transfer.get().zip(record_received).map(|(a,b)| (b-a).as_secs_f64()),
                    "receive_to_publish_seconds": record_received.map(|a| (published-a).as_secs_f64()),
                    "receive_validation_seconds": stages.map(|ns| ns[3] as f64 / 1e9),
                    "publication_validation_seconds": stages.map(|ns| ns[4] as f64 / 1e9),
                    "tool_audit_seconds": audit_seconds,
                });
                eprintln!("USN timing {cycle}");
                cycles.push(cycle);
            }
            if bytes != expected {
                return Ok(());
            }
            let unavailable = &record.tracking.as_ref().unwrap().unavailable;
            eprintln!(
                "C whole-drive: stage {stage}, files {}, unavailable {}",
                record.scan.files.len(),
                unavailable.len()
            );
            let (seconds, change_delay_seconds) = publication_times(start, changed_at, published);
            observations.push(serde_json::json!({
                "stage":stage,"fixture_bytes":bytes,"files":record.scan.files.len(),
                "folders":record.scan.folders.len(),"unavailable":unavailable.len(),
                "system_files_excluded":unavailable.iter().filter(|e| ["pagefile.sys","swapfile.sys","hiberfil.sys"].iter().any(|p| e.relative()==Path::new(p))).count(),
                "seconds":seconds,"change_delay_seconds":change_delay_seconds,"checkpoint_valid":true
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
            use crate::engine::Stage;
            if profile {
                match event.stage {
                    Stage::Transferring => {
                        let now = Instant::now();
                        worker_seconds.set(checking.take().map(|time| (now - time).as_secs_f64()));
                        transfer.set(Some(now));
                        received.set(None);
                        crate::usn::cost::start();
                    }
                    Stage::Basic | Stage::Metadata
                        if transfer.get().is_some() && received.get().is_none() =>
                    {
                        received.set(Some(Instant::now()));
                    }
                    Stage::Basic => checking.set(Some(Instant::now())),
                    _ => {}
                }
            }
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
        && (profile || stable_records + stable_cursors > 0);
    serde_json::to_writer_pretty(fs::File::create(output)?, &serde_json::json!({
        "success":success,"observations":observations,
        "error":failure.as_ref().map(ToString::to_string).or_else(|| (!success).then(|| format!("{result:?}"))),
        "fixture_removed":!fixture.exists(),"journal_modified":false,
        "stability_seconds":stability_seconds,"stability_records":stable_records,"stability_cursors":stable_cursors,
        "cycles":cycles,"cancelled":cancelled,"cancellation_seconds":cancellation_seconds,
        "seconds":start.elapsed().as_secs_f64()
    })).map_err(io::Error::other)?;
    if !success {
        return Err(failure.unwrap_or_else(|| {
            io::Error::other(format!("C드라이브 반영 대조 미완료: {result:?}"))
        }));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn publication_delay_ends_at_callback_entry() {
        let start = Instant::now() - Duration::from_secs(30);
        let changed = start + Duration::from_secs(4);
        let published = start + Duration::from_secs(10);
        assert_eq!(
            publication_times(start, Some(changed), published),
            (10.0, Some(6.0))
        );
        assert_eq!(publication_times(start, None, published), (10.0, None));
    }

    #[test]
    fn missing_projection_cannot_confirm_probe_deletion() {
        let dir = std::env::temp_dir().join(format!(
            "diskweave-watch-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&dir).unwrap();
        let probe = dir.join("probe.bin");
        fs::write(&probe, b"abc").unwrap();
        let scan = crate::scanner::scan_unified(&dir, Default::default(), || false, |_| {}, None);
        fs::remove_file(probe).unwrap();
        fs::remove_dir(dir).unwrap();
        let now = SystemTime::now();
        let record = crate::snapshot::SavedScan::new(
            std::sync::Arc::new(scan.unwrap()),
            now,
            now,
            Duration::ZERO,
        );
        assert_eq!(record.scan.files[0].logical_bytes, 3);
        assert!(probe_bytes(&record, Path::new("probe.bin")).is_err());
    }
}
