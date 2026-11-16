pub use crate::ntfs_mft::*;
use std::{ffi::OsString, io};
pub fn parse_tuning(name: &str) -> io::Result<Tuning> {
    let mut tuning = Tuning::default();
    if name == "reference" {
        return Ok(tuning);
    }
    for flag in name.split('+') {
        match flag {
            "buffer" if !tuning.in_place => tuning.in_place = true,
            "compact" if !tuning.compact => tuning.compact = true,
            "move" if !tuning.move_names => tuning.move_names = true,
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "지원하지 않거나 중복된 MFT 후보",
                ))
            }
        }
    }
    Ok(tuning)
}
pub fn validate(_root: &std::path::Path, target: &crate::ntfs_native::Target) -> io::Result<()> {
    let start = std::time::Instant::now();
    let mut source = Native::open(target)?;
    let catalog = collect(&mut source, || false)?;
    let examples:Vec<_>=catalog.records.iter().filter(|r|r.names.iter().any(|n|n.name=="regular.bin" || n.name=="sparse.bin" || n.name=="compressed.bin" || n.name=="empty")).map(|r|serde_json::json!({"reference":r.reference,"links":r.links,"attributes":r.attributes,"times":r.times,"names":r.names.iter().map(|n|serde_json::json!({"name":n.name.to_string_lossy(),"parent":n.parent,"times":n.times,"bytes":n.bytes,"attributes":n.attributes})).collect::<Vec<_>>(),"data":r.data.as_ref().map(|d|serde_json::json!({"size":d.bytes,"allocation":d.allocation}))})).collect();
    let raw_ms = start.elapsed().as_secs_f64() * 1000.0;
    let root = _root.join("Cases");
    let mut trials = Vec::new();
    let cold_candidate = scan_catalog(&root, target, &catalog, true, || false)?;
    let cold_reference = crate::scanner::scan_with_strategy(
        &root,
        true,
        crate::scanner::Parallelism::Eight,
        crate::scanner::directory::Strategy::Combined,
        || false,
        |_| {},
        None,
    )?;
    let cold_diff = crate::layout::differences(&cold_reference, &cold_candidate);
    let second_reference = crate::scanner::scan_with_strategy(
        &root,
        true,
        crate::scanner::Parallelism::Eight,
        crate::scanner::directory::Strategy::Combined,
        || false,
        |_| {},
        None,
    )?;
    let control_diff = crate::layout::differences(&cold_reference, &second_reference);
    let mut eligible = cold_diff.is_empty();
    for detailed in [false, true] {
        for round in 1..=3 {
            let at = std::time::Instant::now();
            let reference = crate::scanner::scan_with_strategy(
                &root,
                detailed,
                crate::scanner::Parallelism::Eight,
                crate::scanner::directory::Strategy::Combined,
                || false,
                |_| {},
                None,
            )?;
            let reference_ms = at.elapsed().as_secs_f64() * 1000.0;
            let at = std::time::Instant::now();
            let mut source = Native::open(target)?;
            let catalog = collect(&mut source, || false)?;
            let candidate = scan_catalog(&root, target, &catalog, detailed, || false)?;
            let candidate_ms = at.elapsed().as_secs_f64() * 1000.0;
            let (hybrid, hybrid_calls) = scan_bulk(&root, &catalog, detailed, || false)?;
            let hybrid_differences = crate::layout::differences(&reference, &hybrid);
            if !hybrid_differences.is_empty() {
                eligible = false;
            }
            let differences = crate::layout::differences(&reference, &candidate);
            if !differences.is_empty() {
                eligible = false;
            }
            trials.push(serde_json::json!({"detailed":detailed,"round":round,"reference_ms":reference_ms,"candidate_ms":candidate_ms,"equal":differences.is_empty(),"differences":differences.into_iter().take(6).collect::<Vec<_>>(),"hybrid_equal":hybrid_differences.is_empty(),"hybrid_differences":hybrid_differences.into_iter().take(6).collect::<Vec<_>>(),"hybrid_calls":hybrid_calls,"files":candidate.files.len(),"folders":candidate.folders.len(),"bytes":candidate.folders[0].logical_bytes}));
        }
    }
    for name in [
        "buffer",
        "compact",
        "move",
        "buffer+compact",
        "buffer+move",
        "compact+move",
        "buffer+compact+move",
    ] {
        let options = Options {
            tuning: parse_tuning(name)?,
            metrics: None,
        };
        for detailed in [false, true] {
            let mut candidate_source = Native::open(target)?;
            let mut candidate = collect_tuned(&mut candidate_source, options, || false, |_| {})?;
            let reference = crate::scanner::scan_with_options(&root, detailed, || false, |_| {})?;
            let direct =
                scan_catalog_tuned(&root, target, &mut candidate, detailed, || false, options)?;
            if !crate::layout::differences(&reference, &direct).is_empty() {
                return Err(io::Error::other(format!("MFT {name} 실제 직접 대조 실패")));
            }
            if !options.tuning.move_names {
                let (hybrid, _) = scan_bulk(&root, &candidate, detailed, || false)?;
                if !crate::layout::differences(&reference, &hybrid).is_empty() {
                    return Err(io::Error::other(format!("MFT {name} 실제 혼합 대조 실패")));
                }
            }
        }
    }
    println!(
        "{}",
        serde_json::json!({"records":catalog.records.len(),"bytes":catalog.bytes,"reads":catalog.reads,"ioctl_records":catalog.ioctl_records,"raw_ms":raw_ms,"geometry":format!("{:?}",catalog.geometry),"examples":examples,"trials":trials,"cold_candidate_first_equal":cold_diff.is_empty(),"cold_differences":cold_diff.into_iter().take(4).collect::<Vec<_>>(),"reference_self_changes":control_diff.into_iter().take(4).collect::<Vec<_>>(),"eligible_for_performance":eligible})
    );
    Ok(())
}

/// NTFS 3.1의 참조 형식만 full128로 구성하고 경로별 캐시와 객체 정보를 구분.
pub fn profile(args: &[OsString], check: bool, bulk: bool) -> io::Result<()> {
    if !(2..=5).contains(&args.len()) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "--profile-mft <폴더> <basic|detailed> [제한 초] 형식 필요",
        ));
    }
    let detailed = match args[1].to_str() {
        Some("basic") => false,
        Some("detailed") => true,
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "수집 방식 오류",
            ))
        }
    };
    let limit = match args.get(2) {
        None => 180.0,
        Some(value) => value
            .to_str()
            .and_then(|v| v.parse::<f64>().ok())
            .filter(|v| v.is_finite() && *v > 0.0 && *v <= 600.0)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "제한 시간 오류"))?,
    };
    let name = args
        .get(3)
        .map(|v| v.to_str().ok_or_else(bad))
        .transpose()?
        .unwrap_or("reference");
    let tuning = parse_tuning(name)?;
    if bulk && tuning.move_names {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "이름 이동 후보는 직접 MFT 조립에서만 지원",
        ));
    }
    let timed = match args.get(4) {
        None => false,
        Some(v) if v == "timings" => true,
        Some(_) => return Err(bad()),
    };
    if timed {
        crate::scanner::profile::enable_allocations();
    }
    let metrics = crate::scanner::ScanMetrics::default();
    let options = Options {
        tuning,
        metrics: timed.then_some(&metrics),
    };
    let root = std::path::absolute(&args[0])?;
    let target = crate::ntfs_native::target(root.ancestors().last().ok_or_else(bad)?)?;
    let info = std::fs::symlink_metadata(&root)?;
    use std::os::windows::fs::MetadataExt;
    if !info.is_dir() || info.file_attributes() & 0x400 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "일반 NTFS 폴더만 시험할 수 있습니다",
        ));
    }
    let expected = if check {
        drop(crate::scanner::scan_with_strategy(
            &root,
            detailed,
            crate::scanner::Parallelism::Eight,
            crate::scanner::directory::Strategy::Combined,
            || false,
            |_| {},
            None,
        )?);
        Some(crate::scanner::scan_with_strategy(
            &root,
            detailed,
            crate::scanner::Parallelism::Eight,
            crate::scanner::directory::Strategy::Combined,
            || false,
            |_| {},
            None,
        )?)
    } else {
        None
    };
    let start = std::time::Instant::now();
    let cancel = || start.elapsed().as_secs_f64() > limit;
    let mut collect_ms = 0.0;
    let mut assemble_ms = 0.0;
    let strategy = if bulk { "mft-bulk" } else { "raw-mft" };
    let result = {
        let _overall = crate::scanner::profile::scope(
            options.metrics.map(|m| &m.trace),
            crate::scanner::profile::Stage::Overall,
        );
        let _cpu = crate::scanner::profile::cpu_scope(
            options.metrics.map(|m| &m.trace),
            false,
            crate::scanner::profile::thread_cpu,
        );
        (|| {
            let mut source = Native::open(&target)?;
            let mut catalog = collect_tuned(&mut source, options, cancel, |_| {})?;
            collect_ms = start.elapsed().as_secs_f64() * 1000.0;
            let record_count = catalog.record_count();
            let compacted_records = catalog.compacted_records();
            let assembly = std::time::Instant::now();
            let (scan, bulk_calls) = if bulk {
                scan_bulk(&root, &catalog, detailed, cancel)?
            } else {
                (
                    scan_catalog_tuned(&root, &target, &mut catalog, detailed, cancel, options)?,
                    serde_json::Value::Null,
                )
            };
            assemble_ms = assembly.elapsed().as_secs_f64() * 1000.0;
            Ok::<_, io::Error>((
                scan,
                catalog.bytes,
                catalog.reads,
                catalog.ioctl_records,
                catalog
                    .supplemental
                    .load(std::sync::atomic::Ordering::Relaxed),
                catalog
                    .metadata_queries
                    .load(std::sync::atomic::Ordering::Relaxed),
                bulk_calls,
                catalog.decoded_in_place,
                catalog.decoded_copies,
                catalog
                    .moved_names
                    .load(std::sync::atomic::Ordering::Relaxed),
                record_count,
                compacted_records,
            ))
        })()
    };
    match result {
        Ok((
            scan,
            bytes,
            reads,
            controls,
            supplemental,
            metadata_queries,
            bulk_calls,
            in_place_records,
            copied_records,
            moved_names,
            record_count,
            compacted_records,
        )) => {
            let elapsed = start.elapsed().as_secs_f64() * 1000.0;
            let differences = expected
                .as_ref()
                .map(|r| crate::layout::differences(r, &scan));
            let difference_count = differences.as_ref().map(Vec::len);
            println!(
                "{}",
                serde_json::json!({"completed":true,"strategy":strategy,"tuning":name,"decoded_in_place":in_place_records,"decoded_copies":copied_records,"moved_names":moved_names,"mft_records":record_count,"compacted_records":compacted_records,"profile":timed.then(||metrics.trace.snapshot()),"detailed":detailed,"elapsed_ms":elapsed,"collect_ms":collect_ms,"assemble_ms":assemble_ms,"files":scan.files.len(),"folders":scan.folders.len(),"bytes":scan.folders[0].logical_bytes,"errors":scan.error_count,"metadata_errors":scan.metadata_error_count,"reparse_excluded":scan.skipped_reparse_points,"mft_bytes":bytes,"mft_reads":reads,"mft_control_records":controls,"supplemental_parents":supplemental,"metadata_queries":metadata_queries,"bulk_calls":bulk_calls,"directory_rechecks":scan.folders.len(),"difference_count":difference_count,"differences":differences})
            );
            if difference_count.is_some_and(|n| n != 0) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "MFT 후보와 기준 결과 불일치",
                ));
            }
            Ok(())
        }
        Err(error) => {
            println!(
                "{}",
                serde_json::json!({"completed":false,"strategy":strategy,"tuning":name,"profile":timed.then(||metrics.trace.snapshot()),"detailed":detailed,"elapsed_ms":start.elapsed().as_secs_f64()*1000.0,"error":error.to_string(),"error_kind":format!("{:?}",error.kind())})
            );
            Err(error)
        }
    }
}

fn bad() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "잘못된 MFT 측정 입력")
}

#[cfg(test)]
mod profile_tests {
    #[test]
    fn measurement_selection_rejects_unknown_candidates_instead_of_running_a_fallback() {
        assert!(!super::parse_tuning("reference").unwrap().in_place);
        assert!(super::parse_tuning("buffer").unwrap().in_place);
        assert!(super::parse_tuning("buffer+unknown").is_err());
    }
}
