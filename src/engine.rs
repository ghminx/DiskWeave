use crate::scanner::{ScanProgress, ScanResult};
use serde::{Deserialize, Serialize};
use std::{io, path::Path};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub enum Method {
    #[default]
    Basic,
    Ntfs,
}
impl Method {
    pub fn label(self) -> &'static str {
        match self {
            Self::Basic => "기본 스캔",
            Self::Ntfs => "NTFS 고속 스캔",
        }
    }
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub enum FastMode {
    #[default]
    Auto,
    Legacy,
    Mft,
}
impl FastMode {
    pub fn label(self) -> &'static str {
        match self {
            Self::Auto => "자동",
            Self::Legacy => "기존 NTFS 방식",
            Self::Mft => "MFT 수집 (비교용)",
        }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum Collector {
    Basic,
    NtfsRecords,
    MftRaw,
    MftMixed,
}
impl Collector {
    pub fn label(self) -> &'static str {
        match self {
            Self::Basic => "기본 폴더 탐색",
            Self::NtfsRecords => "NTFS 레코드 열거",
            Self::MftRaw => "MFT 직접 수집",
            Self::MftMixed => "MFT·폴더 탐색 혼합",
        }
    }
}
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct Report {
    pub requested: Method,
    pub actual: Method,
    pub fallback: Option<String>,
    #[serde(default)]
    pub fast_mode: Option<FastMode>,
    #[serde(default)]
    pub collector: Option<Collector>,
}
impl Report {
    pub fn label(&self) -> String {
        match &self.fallback {
            Some(reason) => format!(
                "{} 요청 → {} 사용 · {}",
                self.requested.label(),
                self.actual.label(),
                reason
            ),
            None => format!(
                "{} 사용",
                self.collector.map_or(self.actual.label(), Collector::label)
            ),
        }
    }
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub enum Stage {
    #[default]
    Basic,
    Authorizing,
    Enumerating,
    Metadata,
    Transferring,
    Fallback,
}
impl Stage {
    pub fn label(self) -> &'static str {
        match self {
            Self::Basic => "파일·폴더 확인 중",
            Self::Authorizing => "Windows 권한 승인 대기",
            Self::Enumerating => "NTFS 항목 수집 중",
            Self::Metadata => "크기·경로 확인 중",
            Self::Transferring => "결과 전달 중",
            Self::Fallback => "기본 방식으로 다시 스캔 중",
        }
    }
}
#[derive(Clone, Copy, Debug, Default)]
pub struct Progress {
    pub stage: Stage,
    pub counts: ScanProgress,
}
impl Progress {
    pub fn label(&self, seconds: f64) -> String {
        match self.stage {
            Stage::Authorizing | Stage::Transferring => {
                format!("{} · {seconds:.1}초", self.stage.label())
            }
            Stage::Enumerating => format!(
                "{} · 항목 {} · {seconds:.1}초",
                self.stage.label(),
                self.counts.files_scanned
            ),
            _ => format!(
                "{} · 파일 {} · 폴더 {} · {seconds:.1}초",
                self.stage.label(),
                self.counts.files_scanned,
                self.counts.folders_scanned
            ),
        }
    }
}
pub struct Outcome {
    pub result: ScanResult,
    pub report: Report,
}
/// 요청한 수집기를 실행하고 신뢰할 수 없는 고속 결과를 기본 스캔으로 대체.
///
/// Args:
///     fast: 권한 승인·IPC·볼륨 수집을 포함하는 OS 실행 경계.
///
/// Returns:
///     완료 결과와 실제 방식/전환 사유. 사용자 취소는 기본 재스캔 없이 오류 반환.
#[allow(dead_code, reason = "기존 CLI·개발 도구의 공개 수집 경계 유지")]
pub fn scan_with(
    root: &Path,
    method: Method,
    detailed: bool,
    cancelled: impl Fn() -> bool,
    progress: impl FnMut(Progress),
    fast: impl FnMut(&Path, bool, &dyn Fn() -> bool, &mut dyn FnMut(Progress)) -> io::Result<ScanResult>,
) -> io::Result<Outcome> {
    scan_with_parallelism(
        root,
        method,
        detailed,
        Default::default(),
        cancelled,
        progress,
        fast,
    )
}
#[allow(dead_code, reason = "기존 CLI·개발 도구의 공개 수집 경계 유지")]
pub fn scan_with_parallelism(
    root: &Path,
    method: Method,
    detailed: bool,
    parallelism: crate::scanner::Parallelism,
    cancelled: impl Fn() -> bool,
    progress: impl FnMut(Progress),
    mut fast: impl FnMut(
        &Path,
        bool,
        &dyn Fn() -> bool,
        &mut dyn FnMut(Progress),
    ) -> io::Result<ScanResult>,
) -> io::Result<Outcome> {
    scan_observed(
        root,
        method,
        detailed,
        parallelism,
        cancelled,
        progress,
        None,
        |root, detailed, cancel, progress, _| fast(root, detailed, cancel, progress),
    )
}
#[allow(
    clippy::too_many_arguments,
    reason = "기존 수집 인자와 선택적 미리보기 관찰 경계 유지"
)]
#[allow(dead_code, reason = "기존 도구·OS 경계 대조 호출 유지")]
pub fn scan_observed(
    root: &Path,
    method: Method,
    detailed: bool,
    parallelism: crate::scanner::Parallelism,
    cancelled: impl Fn() -> bool,
    progress: impl FnMut(Progress),
    preview: Option<&mut crate::scanner::preview::Observer<'_>>,
    mut fast: impl FnMut(
        &Path,
        bool,
        &dyn Fn() -> bool,
        &mut dyn FnMut(Progress),
        &mut crate::scanner::preview::Observer<'_>,
    ) -> io::Result<ScanResult>,
) -> io::Result<Outcome> {
    scan_selected(
        root,
        &crate::metadata::ScanSettings {
            method,
            detailed,
            parallelism,
            ..Default::default()
        },
        cancelled,
        progress,
        preview,
        |root, detailed, mode, cancel, progress, preview| {
            Ok(Outcome {
                result: fast(root, detailed, cancel, progress, preview)?,
                report: Report {
                    requested: Method::Ntfs,
                    actual: Method::Ntfs,
                    fallback: None,
                    fast_mode: Some(mode),
                    collector: Some(Collector::NtfsRecords),
                },
            })
        },
    )
}
pub fn scan_selected(
    root: &Path,
    settings: &crate::metadata::ScanSettings,
    cancelled: impl Fn() -> bool,
    mut progress: impl FnMut(Progress),
    mut preview: Option<&mut crate::scanner::preview::Observer<'_>>,
    mut fast: impl FnMut(
        &Path,
        bool,
        FastMode,
        &dyn Fn() -> bool,
        &mut dyn FnMut(Progress),
        &mut crate::scanner::preview::Observer<'_>,
    ) -> io::Result<Outcome>,
) -> io::Result<Outcome> {
    let crate::metadata::ScanSettings {
        method,
        detailed,
        parallelism,
        fast_mode,
    } = *settings;
    crate::output::check(&cancelled)?;
    let mut fallback = None;
    if method == Method::Ntfs {
        match fast(
            root,
            detailed,
            fast_mode,
            &cancelled,
            &mut progress,
            &mut |batch| {
                if let Some(preview) = preview.as_mut() {
                    preview(batch)
                } else {
                    Ok(())
                }
            },
        ) {
            Ok(outcome) => {
                crate::output::check(&cancelled)?;
                if outcome.result.detailed == detailed
                    && outcome
                        .result
                        .folders
                        .first()
                        .is_some_and(|f| f.path == root)
                    && outcome.report.requested == method
                    && outcome.report.actual == Method::Ntfs
                    && outcome.report.fast_mode == Some(fast_mode)
                    && outcome.report.fallback.is_none()
                    && match fast_mode {
                        FastMode::Mft => matches!(
                            outcome.report.collector,
                            Some(Collector::MftRaw | Collector::MftMixed)
                        ),
                        _ => outcome.report.collector == Some(Collector::NtfsRecords),
                    }
                {
                    return Ok(outcome);
                }
                fallback = Some("고속 수집 결과의 대상·옵션·실제 방식이 일치하지 않습니다".into());
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => return Err(e),
            Err(e) => fallback = Some(e.to_string()),
        }
    }
    crate::output::check(&cancelled)?;
    let stage = if fallback.is_some() {
        Stage::Fallback
    } else {
        Stage::Basic
    };
    progress(Progress {
        stage,
        counts: Default::default(),
    });
    let result = if let Some(preview) = preview.as_mut() {
        crate::scanner::scan_preview(
            root,
            detailed,
            parallelism,
            cancelled,
            |counts| progress(Progress { stage, counts }),
            &mut |batch| preview(batch),
        )?
    } else {
        crate::scanner::scan_configured(root, detailed, parallelism, cancelled, |counts| {
            progress(Progress { stage, counts })
        })?
    };
    Ok(Outcome {
        result,
        report: Report {
            requested: method,
            actual: Method::Basic,
            fallback,
            fast_mode: (method == Method::Ntfs).then_some(fast_mode),
            collector: Some(Collector::Basic),
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn mismatched_product_collector_is_rejected_and_cancellation_does_not_retry() {
        let root = std::env::temp_dir().join(format!("diskweave-selected-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("known"), b"123").unwrap();
        let settings = crate::metadata::ScanSettings {
            method: Method::Ntfs,
            fast_mode: FastMode::Mft,
            detailed: false,
            ..Default::default()
        };
        let result = scan_selected(
            &root,
            &settings,
            || false,
            |_| {},
            None,
            |path, detailed, mode, cancel, _, _| {
                Ok(Outcome {
                    result: crate::scanner::scan_with_options(path, detailed, cancel, |_| {})?,
                    report: Report {
                        requested: Method::Ntfs,
                        actual: Method::Ntfs,
                        fast_mode: Some(mode),
                        collector: Some(Collector::NtfsRecords),
                        ..Default::default()
                    },
                })
            },
        )
        .unwrap();
        assert_eq!(result.report.actual, Method::Basic);
        assert_eq!(result.report.collector, Some(Collector::Basic));
        assert!(result.report.fallback.unwrap().contains("일치하지"));
        assert_eq!(result.result.folders[0].logical_bytes, 3);
        let mut stages = vec![];
        let error = scan_selected(
            &root,
            &settings,
            || false,
            |p| stages.push(p.stage),
            None,
            |_, _, _, _, _, _| Err(io::Error::new(io::ErrorKind::Interrupted, "취소")),
        )
        .err()
        .unwrap();
        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
        assert!(!stages.contains(&Stage::Fallback));
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn explicit_mft_selection_reaches_the_product_engine_and_records_the_actual_collector() {
        let root =
            std::env::temp_dir().join(format!("diskweave-product-mft-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("known"), b"12345").unwrap();
        let settings = crate::metadata::ScanSettings {
            method: Method::Ntfs,
            fast_mode: FastMode::Mft,
            detailed: false,
            ..Default::default()
        };
        let mut seen = None;
        let outcome = scan_selected(
            &root,
            &settings,
            || false,
            |_| {},
            None,
            |path, detailed, mode, cancel, _, _| {
                seen = Some(mode);
                Ok(Outcome {
                    result: crate::scanner::scan_with_options(path, detailed, cancel, |_| {})?,
                    report: Report {
                        requested: Method::Ntfs,
                        actual: Method::Ntfs,
                        fallback: None,
                        fast_mode: Some(mode),
                        collector: Some(Collector::MftRaw),
                    },
                })
            },
        )
        .unwrap();
        assert_eq!(seen, Some(FastMode::Mft));
        assert_eq!(outcome.report.collector, Some(Collector::MftRaw));
        assert_eq!(outcome.result.folders[0].logical_bytes, 5);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn fallback_discards_failed_ntfs_preview_before_basic_discovery() {
        use crate::scanner::preview::{Batch, Model, Update};
        let root =
            std::env::temp_dir().join(format!("diskweave-preview-fallback-{}", std::process::id()));
        std::fs::create_dir_all(root.join("actual")).unwrap();
        let mut model = Model::new(root.clone());
        let mut resets = 0;
        let outcome = scan_observed(
            &root,
            Method::Ntfs,
            false,
            Default::default(),
            || false,
            |_| {},
            Some(&mut |batch: Batch| {
                resets += usize::from(batch.reset);
                model.apply(batch)
            }),
            |_, _, _, _, preview| {
                preview(Batch {
                    reset: true,
                    updates: vec![
                        Update::Folder {
                            id: 0,
                            parent: None,
                            name: vec![],
                        },
                        Update::Folder {
                            id: 1,
                            parent: Some(0),
                            name: "failed".encode_utf16().collect(),
                        },
                    ],
                })?;
                Err(io::Error::other("도우미 실패"))
            },
        )
        .unwrap();
        assert_eq!(resets, 2);
        assert_eq!(outcome.report.actual, Method::Basic);
        assert_eq!(model.folders.len(), 2);
        assert_eq!(model.folders[1].path, root.join("actual"));
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn display_keeps_unknown_work_totals_and_record_counts_distinct() {
        let p = Progress {
            stage: Stage::Enumerating,
            counts: ScanProgress {
                files_scanned: 123,
                folders_scanned: 0,
            },
        };
        assert_eq!(p.label(1.5), "NTFS 항목 수집 중 · 항목 123 · 1.5초");
        assert_eq!(
            Progress {
                stage: Stage::Authorizing,
                ..Default::default()
            }
            .label(2.0),
            "Windows 권한 승인 대기 · 2.0초"
        );
        assert_eq!(
            Progress {
                stage: Stage::Metadata,
                counts: ScanProgress {
                    files_scanned: 7,
                    folders_scanned: 2
                }
            }
            .label(3.0),
            "크기·경로 확인 중 · 파일 7 · 폴더 2 · 3.0초"
        );
    }
    #[test]
    fn failed_fast_scan_falls_back_but_cancellation_never_restarts() {
        let root = std::env::temp_dir().join(format!("diskweave-engine-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("known"), b"12345").unwrap();
        let mut stages = Vec::new();
        let outcome = scan_with(
            &root,
            Method::Ntfs,
            false,
            || false,
            |p| stages.push(p.stage),
            |_, _, _, _| {
                Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "권한 승인 취소",
                ))
            },
        )
        .unwrap();
        assert_eq!(outcome.report.requested, Method::Ntfs);
        assert_eq!(outcome.report.actual, Method::Basic);
        assert!(outcome.report.fallback.unwrap().contains("권한 승인 취소"));
        assert_eq!(outcome.result.folders[0].logical_bytes, 5);
        assert!(stages.contains(&Stage::Fallback));
        let e = scan_with(
            &root,
            Method::Ntfs,
            false,
            || false,
            |_| {},
            |_, _, _, _| Err(io::Error::new(io::ErrorKind::Interrupted, "취소")),
        )
        .err()
        .unwrap();
        assert_eq!(e.kind(), io::ErrorKind::Interrupted);
        std::fs::remove_dir_all(root).unwrap();
    }
}
