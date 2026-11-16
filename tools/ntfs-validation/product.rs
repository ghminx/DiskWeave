use crate::{engine, metadata, ntfs_helper, scanner};
use std::{
    io,
    path::Path,
    time::{Duration, Instant},
};

/// 실제 작업 제어·관리자 IPC·완료 게시를 포함한 제품 경로 측정.
pub fn profile(args: &[std::ffi::OsString]) -> io::Result<()> {
    if !(3..=4).contains(&args.len()) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput,"--profile-product <폴더> <basic|detailed> <basic|legacy|mft> [check|reuse|cancel-read|cancel-metadata|cancel-transfer]"));
    }
    let root = std::path::absolute(&args[0])?;
    let detailed = match args[1].to_str() {
        Some("basic") => false,
        Some("detailed") => true,
        _ => return Err(invalid()),
    };
    let (method, mode) = match args[2].to_str() {
        Some("basic") => (engine::Method::Basic, engine::FastMode::Auto),
        Some("legacy") => (engine::Method::Ntfs, engine::FastMode::Legacy),
        Some("mft") => (engine::Method::Ntfs, engine::FastMode::Mft),
        _ => return Err(invalid()),
    };
    let option = args
        .get(3)
        .map(|v| v.to_str().ok_or_else(invalid))
        .transpose()?
        .unwrap_or("");
    let cancel_stage = match option {
        "" | "check" | "reuse" => None,
        "cancel-read" => Some(engine::Stage::Enumerating),
        "cancel-metadata" => Some(engine::Stage::Metadata),
        "cancel-transfer" => Some(engine::Stage::Transferring),
        _ => return Err(invalid()),
    };
    let expected = if option == "check" {
        Some(scanner::scan_configured(
            &root,
            detailed,
            scanner::Parallelism::Eight,
            || false,
            |_| {},
        )?)
    } else {
        None
    };
    let prepare = Instant::now();
    if option == "reuse" {
        ntfs_helper::prepare()?;
    }
    let prepare_ms = prepare.elapsed().as_secs_f64() * 1000.;
    let mut job = crate::scan_job::ScanController::default();
    job.configure(detailed).map_err(io::Error::other)?;
    job.configure_method(method).map_err(io::Error::other)?;
    job.configure_fast_mode(mode).map_err(io::Error::other)?;
    job.configure_parallelism(scanner::Parallelism::Eight)
        .map_err(io::Error::other)?;
    let clock = Instant::now();
    job.start(root.clone()).map_err(io::Error::other)?;
    let mut preview = false;
    let mut cancelled = false;
    while job.is_running() {
        if cancel_stage.is_some() && job.stage() == cancel_stage {
            job.cancel();
            cancelled = true;
        }
        job.poll();
        preview |= job.preview().is_some_and(|m| m.folders.len() > 1);
        if clock.elapsed() > Duration::from_secs(600) {
            job.cancel();
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "제품 스캔 측정 시간 초과",
            ));
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    let elapsed_ms = clock.elapsed().as_secs_f64() * 1000.;
    if cancel_stage.is_some() {
        let correct = cancelled
            && matches!(job.status(), crate::scan_job::ScanStatus::Cancelled)
            && job.completed().is_none()
            && job.preview().is_none();
        println!(
            "{}",
            serde_json::json!({"completed":false,"cancelled":correct,"elapsed_ms":elapsed_ms,"prepare_ms":prepare_ms})
        );
        ntfs_helper::shutdown();
        return if correct {
            Ok(())
        } else {
            Err(io::Error::other("제품 단계 취소 미확인"))
        };
    }
    let Some(done) = job.completed() else {
        return Err(io::Error::other(match job.status() {
            crate::scan_job::ScanStatus::Failed(s) => s.clone(),
            _ => "제품 완료 결과 누락".into(),
        }));
    };
    let report = done.engine.as_ref().ok_or_else(invalid)?;
    let differences = expected
        .as_ref()
        .map(|r| crate::layout::differences(r, &done.result));
    let count = differences.as_ref().map(Vec::len);
    println!(
        "{}",
        serde_json::json!({"completed":true,"elapsed_ms":elapsed_ms,"prepare_ms":prepare_ms,"report":report,"files":done.result.files.len(),"folders":done.result.folders.len(),"bytes":done.result.folders[0].logical_bytes,"errors":done.result.error_count,"metadata_errors":done.result.metadata_error_count,"preview":preview,"difference_count":count,"differences":differences})
    );
    ntfs_helper::shutdown();
    if report.actual != method
        || (mode == engine::FastMode::Mft && report.collector != Some(engine::Collector::MftMixed))
        || count.is_some_and(|c| c > 0)
    {
        return Err(io::Error::other("제품 경로·필드 대조 실패"));
    }
    Ok(())
}
fn invalid() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, "제품 측정 입력·응답 오류")
}

/// 소유가 확인된 새 NTFS 시험 디스크에서만 실행하는 실제 제품 경로 대조.
pub fn validate(root: &Path) -> io::Result<()> {
    let mut rows = vec![];
    ntfs_helper::prepare()?;
    for scope in [root.to_owned(), root.join("Cases")] {
        for detailed in [false, true] {
            let first = scanner::scan_configured(
                &scope,
                detailed,
                scanner::Parallelism::Eight,
                || false,
                |_| {},
            )?;
            let expected = scanner::scan_configured(
                &scope,
                detailed,
                scanner::Parallelism::Eight,
                || false,
                |_| {},
            )?;
            let control = scanner::scan_configured(
                &scope,
                detailed,
                scanner::Parallelism::Eight,
                || false,
                |_| {},
            )?;
            let first_changes = crate::layout::differences(&first, &expected);
            let control_changes = crate::layout::differences(&expected, &control);
            eprintln!(
                "기본 방식 자체 대조: 최초 변경 {} · 안정성 변경 {}",
                first_changes.len(),
                control_changes.len()
            );
            if !control_changes.is_empty() {
                return Err(io::Error::other(format!(
                    "시험 데이터 관찰값 변동: {control_changes:?}"
                )));
            }
            for round in 0..3 {
                let mut preview = scanner::preview::Model::new(scope.clone());
                let mut packets = 0;
                let clock = Instant::now();
                let outcome = engine::scan_selected(
                    &scope,
                    &metadata::ScanSettings {
                        method: engine::Method::Ntfs,
                        fast_mode: engine::FastMode::Mft,
                        detailed,
                        parallelism: scanner::Parallelism::Eight,
                    },
                    || false,
                    |_| {},
                    Some(&mut |batch| {
                        packets += 1;
                        preview.apply(batch)
                    }),
                    ntfs_helper::scan_selected,
                )?;
                let differences = crate::layout::differences(&expected, &outcome.result);
                if !differences.is_empty() {
                    return Err(io::Error::other(format!(
                        "제품 MFT 필드 차이: {differences:?}"
                    )));
                }
                if outcome.report.collector != Some(engine::Collector::MftMixed)
                    || packets < 2
                    || preview.folders.len() != outcome.result.folders.len()
                {
                    return Err(io::Error::other("제품 MFT 실행·미리보기 누락"));
                }
                for folder in &preview.folders {
                    let done = outcome
                        .result
                        .folders
                        .iter()
                        .find(|f| f.path == folder.path)
                        .ok_or_else(invalid)?;
                    if (folder.bytes, folder.files, folder.folders)
                        != (done.logical_bytes, done.file_count, done.folder_count)
                    {
                        return Err(io::Error::other("제품 미리보기 합계 차이"));
                    }
                }
                rows.push(serde_json::json!({"drive":scope==root,"detailed":detailed,"round":round+1,"elapsed_ms":clock.elapsed().as_secs_f64()*1000.,"files":outcome.result.files.len(),"folders":outcome.result.folders.len(),"packets":packets,"report":outcome.report}));
            }
        }
    }
    for stage in [
        engine::Stage::Enumerating,
        engine::Stage::Metadata,
        engine::Stage::Transferring,
    ] {
        let stop = std::cell::Cell::new(false);
        let fallback = std::cell::Cell::new(false);
        let result = engine::scan_selected(
            &root.join("Cases"),
            &metadata::ScanSettings {
                method: engine::Method::Ntfs,
                fast_mode: engine::FastMode::Mft,
                ..Default::default()
            },
            || stop.get(),
            |p| {
                if p.stage == stage {
                    stop.set(true);
                }
                if p.stage == engine::Stage::Fallback {
                    fallback.set(true);
                }
            },
            None,
            ntfs_helper::scan_selected,
        );
        if !stop.get()
            || fallback.get()
            || result
                .err()
                .is_none_or(|e| e.kind() != io::ErrorKind::Interrupted)
        {
            return Err(io::Error::other(format!("제품 MFT {stage:?} 취소 실패")));
        }
    }
    ntfs_helper::shutdown();
    println!(
        "{}",
        serde_json::json!({"product_mft":rows,"cancel_stages":3})
    );
    Ok(())
}
