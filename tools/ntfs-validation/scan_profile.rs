use std::{
    cell::{Cell, RefCell},
    path::Path,
    sync::atomic::{AtomicU64, Ordering},
    time::Instant,
};
/// 파일 내용을 읽거나 변경하지 않는 기본 수집의 시간·추가 조회 비용 측정.
///
/// Args:
///     args: 분석 폴더·basic/detailed 방식·선택적 제한 시간(초)·작업 수·비교 경로·timings.
///
/// Returns:
///     완료 시 JSON 요약 출력, 잘못된 입력·수집 실패·제한 시간 도달 시 오류.
pub fn run(args: &[std::ffi::OsString]) -> std::io::Result<()> {
    if !(2..=6).contains(&args.len()) {
        return Err(std::io::Error::other(
            "--profile-scan <폴더> <basic|detailed> [제한 초] [auto|1|2|4|8|12|16] [reference|large|bulk|combined] [timings] 형식 필요",
        ));
    }
    let path = Path::new(&args[0]);
    let detailed = match args[1].to_str() {
        Some("basic") => false,
        Some("detailed") => true,
        _ => return Err(std::io::Error::other("basic 또는 detailed 방식 필요")),
    };
    let limit = match args.get(2) {
        None => 150.0,
        Some(value) => value
            .to_str()
            .and_then(|v| v.parse::<f64>().ok())
            .filter(|v| v.is_finite() && *v > 0.0 && *v <= 3600.0)
            .ok_or_else(|| std::io::Error::other("제한 시간은 0 초과 3600 이하 필요"))?,
    };
    let parallelism = match args.get(3).and_then(|v| v.to_str()).unwrap_or("auto") {
        "auto" => crate::scanner::Parallelism::Auto,
        "1" => crate::scanner::Parallelism::Sequential,
        "2" => crate::scanner::Parallelism::Two,
        "4" => crate::scanner::Parallelism::Four,
        "8" => crate::scanner::Parallelism::Eight,
        "12" => crate::scanner::Parallelism::Twelve,
        "16" => crate::scanner::Parallelism::Sixteen,
        _ => {
            return Err(std::io::Error::other(
                "동시 작업 수는 auto/1/2/4/8/12/16 필요",
            ))
        }
    };
    let started = Instant::now();
    let timed = args.get(4).is_some_and(|v| v == "timings")
        || match args.get(5) {
            None => false,
            Some(value) if value == "timings" => true,
            Some(_) => return Err(std::io::Error::other("추가 계측 옵션은 timings 필요")),
        };
    if timed {
        crate::scanner::profile::enable_allocations();
    }
    let metrics = crate::scanner::ScanMetrics::default();
    let stages: [AtomicU64; 6] = std::array::from_fn(|_| AtomicU64::new(0));
    let probe_ns = AtomicU64::new(0);
    let calls = AtomicU64::new(0);
    let last = Cell::new(0.0);
    let counts = RefCell::new(crate::scanner::ScanProgress::default());
    let strategy = match args.get(4) {
        None => "reference",
        Some(value) => value.to_str().ok_or_else(|| {
            std::io::Error::other("수집 경로는 reference/large/bulk/combined 필요")
        })?,
    };
    if !["reference", "timings", "large", "bulk", "combined"].contains(&strategy) {
        return Err(std::io::Error::other(
            "reference/large/bulk/combined 방식 필요",
        ));
    }
    let update = |p: crate::scanner::ScanProgress| {
        *counts.borrow_mut() = p;
        let elapsed = started.elapsed().as_secs_f64();
        if elapsed - last.get() > 5.0 {
            eprintln!(
                "elapsed={elapsed:.1}s files={} folders={}",
                p.files_scanned, p.folders_scanned
            );
            last.set(elapsed);
        }
    };
    let result = if !["reference", "timings"].contains(&strategy) {
        crate::scanner::scan_with_strategy(
            path,
            detailed,
            parallelism,
            match strategy {
                "large" => crate::scanner::directory::Strategy::LargeFetch,
                "bulk" => crate::scanner::directory::Strategy::Bulk,
                _ => crate::scanner::directory::Strategy::Combined,
            },
            || started.elapsed().as_secs_f64() > limit,
            update,
            timed.then_some(&metrics),
        )
    } else {
        crate::scanner::scan_parallel_with_probe(
            path,
            detailed,
            parallelism,
            || started.elapsed().as_secs_f64() > limit,
            |p| {
                *counts.borrow_mut() = p;
                let elapsed = started.elapsed().as_secs_f64();
                if elapsed - last.get() > 5.0 {
                    eprintln!(
                        "elapsed={elapsed:.1}s files={} folders={} metadata_ms={:.1}",
                        p.files_scanned,
                        p.folders_scanned,
                        probe_ns.load(Ordering::Relaxed) as f64 / 1e6
                    );
                    last.set(elapsed);
                }
            },
            |p| {
                let at = Instant::now();
                let r = if timed {
                    let mut metrics = crate::metadata::ReadMetrics::default();
                    let r = crate::metadata::read_with_metrics(p, Some(&mut metrics));
                    for (counter, value) in stages.iter().zip(metrics.nanoseconds) {
                        counter.fetch_add(value, Ordering::Relaxed);
                    }
                    r
                } else {
                    crate::metadata::read(p)
                };
                probe_ns.fetch_add(at.elapsed().as_nanos() as u64, Ordering::Relaxed);
                calls.fetch_add(1, Ordering::Relaxed);
                r
            },
        )
    };
    match result {
        Ok(scan) => {
            for issue in &scan.issues {
                eprintln!("scan_issue={} {}", issue.path.display(), issue.error);
            }
            for issue in &scan.metadata_issues {
                eprintln!("metadata_issue={} {}", issue.path.display(), issue.error);
            }
            if timed && !matches!(strategy, "reference" | "timings") {
                eprintln!(
                    "enumeration_cumulative_ms={} finish_ms={} read_stages_ms={:?} read_calls={:?}",
                    metrics.enumeration_ns.load(Ordering::Relaxed) as f64 / 1e6,
                    metrics.finish_ns.load(Ordering::Relaxed) as f64 / 1e6,
                    metrics
                        .read_ns
                        .each_ref()
                        .map(|v| v.load(Ordering::Relaxed) as f64 / 1e6),
                    metrics
                        .read_calls
                        .each_ref()
                        .map(|v| v.load(Ordering::Relaxed))
                );
            }
            if timed && matches!(strategy, "reference" | "timings") {
                eprintln!(
                    "read_stages_ms(open,attributes,standard,id,compression,close)={:?}",
                    stages
                        .each_ref()
                        .map(|v| v.load(Ordering::Relaxed) as f64 / 1e6)
                );
            }
            let measured = matches!(strategy, "reference" | "timings") || timed;
            let metadata_ms = if measured {
                Some(if matches!(strategy, "reference" | "timings") {
                    probe_ns.load(Ordering::Relaxed) as f64 / 1e6
                } else {
                    metrics
                        .read_ns
                        .iter()
                        .map(|v| v.load(Ordering::Relaxed) as f64 / 1e6)
                        .sum()
                })
            } else {
                None
            };
            let metadata_calls = if detailed { scan.files.len() } else { 0 };
            println!(
                "{}",
                serde_json::json!({"completed":true,"profile":timed.then(||metrics.trace.snapshot()),"strategy":strategy,"workers":parallelism.workers(),"detailed":detailed,"elapsed_ms":started.elapsed().as_secs_f64()*1e3,"metadata_ms":metadata_ms,"metadata_calls":metadata_calls,"files":scan.files.len(),"folders":scan.folders.len(),"bytes":scan.folders[0].logical_bytes,"errors":scan.error_count,"metadata_errors":scan.metadata_error_count,"reparse_excluded":scan.skipped_reparse_points})
            );
        }
        Err(e) => {
            let p = counts.borrow();
            let measured = matches!(strategy, "reference" | "timings");
            println!(
                "{}",
                serde_json::json!({"completed":false,"strategy":strategy,"workers":parallelism.workers(),"detailed":detailed,"elapsed_ms":started.elapsed().as_secs_f64()*1e3,"metadata_ms":measured.then(||probe_ns.load(Ordering::Relaxed) as f64/1e6),"metadata_calls":measured.then(||calls.load(Ordering::Relaxed)),"files":p.files_scanned,"folders":p.folders_scanned,"error":e.to_string()})
            );
            return Err(e);
        }
    }
    Ok(())
}
