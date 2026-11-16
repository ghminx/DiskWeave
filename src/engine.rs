use crate::scanner::{ScanProgress, ScanResult};
use serde::{Deserialize, Serialize};
use std::{io, path::Path};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub enum Method {
    #[default]
    Basic,
    /// 이전 Snapshot의 수집 이력 해석 전용. 새 요청은 일반 수집으로 전환.
    Ntfs,
}
impl Method {
    pub fn label(self) -> &'static str {
        match self {
            Self::Basic => "일반 스캔",
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
pub fn scan_selected<'a>(
    root: &Path,
    parallelism: crate::scanner::Parallelism,
    cancelled: impl Fn() -> bool,
    mut progress: impl FnMut(Progress),
    preview: Option<&'a mut crate::scanner::preview::Observer<'a>>,
) -> io::Result<Outcome> {
    crate::output::check(&cancelled)?;
    let stage = Stage::Basic;
    progress(Progress {
        stage,
        counts: Default::default(),
    });
    let result = crate::scanner::scan_unified(
        root,
        parallelism,
        cancelled,
        |counts| progress(Progress { stage, counts }),
        preview,
    )?;
    Ok(Outcome {
        result,
        report: Report {
            requested: Method::Basic,
            actual: Method::Basic,
            fallback: None,
            fast_mode: None,
            collector: Some(Collector::Basic),
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn general_scan_preserves_metadata_and_cancellation() {
        let root =
            std::env::temp_dir().join(format!("diskweave-general-only-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("known"), b"123").unwrap();
        let parallelism = crate::scanner::Parallelism::default();
        let out = scan_selected(&root, parallelism, || false, |_| {}, None).unwrap();
        assert_eq!(out.report.actual, Method::Basic);
        assert_eq!(out.report.collector, Some(Collector::Basic));
        assert!(out.report.fallback.is_none());
        assert_eq!(out.result.files.len(), 1);
        assert_eq!(out.result.folders[0].logical_bytes, 3);
        assert_eq!(
            scan_selected(&root, parallelism, || true, |_| {}, None)
                .err()
                .unwrap()
                .kind(),
            io::ErrorKind::Interrupted
        );
        std::fs::remove_dir_all(root).unwrap();
    }
}
