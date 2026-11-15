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
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Report {
    pub requested: Method,
    pub actual: Method,
    pub fallback: Option<String>,
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
            None => format!("{} 사용", self.actual.label()),
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
pub fn scan_with(
    root: &Path,
    method: Method,
    detailed: bool,
    cancelled: impl Fn() -> bool,
    mut progress: impl FnMut(Progress),
    mut fast: impl FnMut(
        &Path,
        bool,
        &dyn Fn() -> bool,
        &mut dyn FnMut(Progress),
    ) -> io::Result<ScanResult>,
) -> io::Result<Outcome> {
    crate::output::check(&cancelled)?;
    let mut fallback = None;
    if method == Method::Ntfs {
        match fast(root, detailed, &cancelled, &mut progress) {
            Ok(result) => {
                crate::output::check(&cancelled)?;
                return Ok(Outcome {
                    result,
                    report: Report {
                        requested: method,
                        actual: Method::Ntfs,
                        fallback: None,
                    },
                });
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
    let result = crate::scanner::scan_with_options(root, detailed, cancelled, |counts| {
        progress(Progress { stage, counts })
    })?;
    Ok(Outcome {
        result,
        report: Report {
            requested: method,
            actual: Method::Basic,
            fallback,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
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
