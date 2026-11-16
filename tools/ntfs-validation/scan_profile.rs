use std::{
    cell::{Cell, RefCell},
    path::Path,
    sync::atomic::{AtomicU64, Ordering},
    time::Instant,
};
pub fn parse_strategy(value: &str) -> std::io::Result<crate::scanner::directory::Strategy> {
    use crate::scanner::directory::{Experiment, Strategy};
    match value {
        "reference" | "timings" => Ok(Strategy::Reference),
        "large" => Ok(Strategy::LargeFetch),
        "bulk" => Ok(Strategy::Bulk),
        "combined" => Ok(Strategy::Combined),
        _ if value.starts_with("bulk+") => {
            let mut experiment = Experiment::default();
            let mut buffer_selected = false;
            for flag in value[5..].split('+') {
                match flag {
                    "handle" if !experiment.shared_handle => experiment.shared_handle = true,
                    "reuse" if !experiment.reuse_buffer => experiment.reuse_buffer = true,
                    "64k" | "1m" | "4m" if !buffer_selected => {
                        experiment.buffer_bytes = match flag {
                            "64k" => 65536,
                            "1m" => 1048576,
                            _ => 4194304,
                        };
                        buffer_selected = true;
                    }
                    _ => return Err(std::io::Error::other(
                        "실험 옵션은 handle/reuse와 64k/1m/4m 중 하나를 중복 없이 지정해야 합니다",
                    )),
                }
            }
            Ok(Strategy::Experimental(experiment))
        }
        _ => Err(std::io::Error::other(
            "수집 경로는 reference/large/bulk/combined 또는 bulk+옵션 형식이어야 합니다",
        )),
    }
}
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
            "--profile-scan <폴더> <basic|detailed> [제한 초] [auto|1|2|4|8|12|16] [reference|large|bulk|combined|bulk+옵션] [timings] 형식 필요",
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
    let selected_strategy = parse_strategy(strategy)?;
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
            selected_strategy,
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

#[cfg(test)]
mod experiment_tests {
    #[test]
    fn buffer_sizes_are_explicit_and_do_not_enable_other_experiments() {
        use crate::scanner::directory::{Experiment, Strategy};
        for (name, buffer_bytes) in [
            ("bulk+64k", 65536),
            ("bulk+1m", 1048576),
            ("bulk+4m", 4194304),
        ] {
            assert_eq!(
                super::parse_strategy(name).unwrap(),
                Strategy::Experimental(Experiment {
                    buffer_bytes,
                    ..Default::default()
                })
            );
        }
        assert_eq!(
            super::parse_strategy("bulk+handle+reuse+1m").unwrap(),
            Strategy::Experimental(Experiment {
                shared_handle: true,
                reuse_buffer: true,
                buffer_bytes: 1048576
            })
        );
        for name in ["bulk+128k", "bulk+1m+4m", "bulk+handle+handle", "bulk+"] {
            assert!(super::parse_strategy(name).is_err());
        }
    }
    #[test]
    fn page_reuse_is_independent_of_the_shared_handle_switch() {
        use crate::scanner::directory::{Experiment, Strategy};
        assert_eq!(
            super::parse_strategy("bulk+reuse").unwrap(),
            Strategy::Experimental(Experiment {
                reuse_buffer: true,
                ..Default::default()
            })
        );
        assert_eq!(
            super::parse_strategy("bulk+handle+reuse").unwrap(),
            Strategy::Experimental(Experiment {
                reuse_buffer: true,
                shared_handle: true,
                ..Default::default()
            })
        );
        assert!(super::parse_strategy("bulk+reuse+typo").is_err());
    }
    #[test]
    fn explicit_shared_handle_candidate_preserves_baseline_and_rejects_unknown_modes() {
        use crate::scanner::directory::{Experiment, Strategy};
        assert_eq!(
            super::parse_strategy("combined").unwrap(),
            Strategy::Combined
        );
        assert_eq!(super::parse_strategy("bulk").unwrap(), Strategy::Bulk);
        assert_eq!(
            super::parse_strategy("bulk+handle").unwrap(),
            Strategy::Experimental(Experiment {
                shared_handle: true,
                ..Default::default()
            })
        );
        assert!(super::parse_strategy("bulk+typo").is_err());
    }
}
