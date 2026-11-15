use std::fs::{self, OpenOptions};
use std::io::{self, BufWriter, Write};
use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT: AtomicU64 = AtomicU64::new(0);

pub fn check(cancelled: &impl Fn() -> bool) -> io::Result<()> {
    if cancelled() {
        Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "작업을 취소했습니다",
        ))
    } else {
        Ok(())
    }
}

pub fn json_error(error: serde_json::Error) -> io::Error {
    io::Error::new(
        error.io_error_kind().unwrap_or(io::ErrorKind::InvalidData),
        error,
    )
}

pub fn protect(
    path: &Path,
    scan: &crate::scanner::ScanResult,
    cancelled: &impl Fn() -> bool,
) -> io::Result<()> {
    let target = match fs::canonicalize(path) {
        Ok(path) => path,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    let root = &scan.folders[0].path;
    let canonical_root = fs::canonicalize(root).unwrap_or_else(|_| root.clone());
    for file in &scan.files {
        check(cancelled)?;
        let original = scan.folders[file.parent].path.join(&file.name);
        let normalized =
            canonical_root.join(original.strip_prefix(root).map_err(io::Error::other)?);
        if crate::locations::contains_path(&target, &normalized)?
            && crate::locations::contains_path(&normalized, &target)?
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "스캔한 원본 파일에 덮어쓸 수 없습니다. 다른 이름으로 저장하세요.",
            ));
        }
    }
    Ok(())
}

struct CheckedWriter<'a, W, C> {
    writer: W,
    cancelled: &'a C,
}
impl<W: Write, C: Fn() -> bool> Write for CheckedWriter<'_, W, C> {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        // write_all은 Interrupted를 재시도하므로 직렬화 중 취소는 Other로 중단.
        if (self.cancelled)() {
            return Err(io::Error::other("작업을 취소했습니다"));
        }
        self.writer.write(buffer)
    }
    fn flush(&mut self) -> io::Result<()> {
        if (self.cancelled)() {
            return Err(io::Error::other("작업을 취소했습니다"));
        }
        self.writer.flush()
    }
}

#[link(name = "kernel32")]
extern "system" {
    fn MoveFileExW(from: *const u16, to: *const u16, flags: u32) -> i32;
}

/// 동일 디렉터리의 임시 파일 완성 후 최종 경로로 교체. 취소·실패 시 기존 대상 유지.
pub fn write(
    path: &Path,
    overwrite: bool,
    cancelled: impl Fn() -> bool,
    body: impl FnOnce(&mut dyn Write) -> io::Result<()>,
) -> io::Result<()> {
    check(&cancelled)?;
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let temporary = parent.join(format!(
        ".diskweave-{}-{}.tmp",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)?;
    let result = (|| {
        let mut writer = CheckedWriter {
            writer: BufWriter::new(file),
            cancelled: &cancelled,
        };
        body(&mut writer)?;
        writer.flush()?;
        writer.writer.get_ref().sync_all()?;
        drop(writer);
        check(&cancelled)?;
        let from: Vec<_> = temporary.as_os_str().encode_wide().chain(Some(0)).collect();
        let to: Vec<_> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        if from[..from.len() - 1].contains(&0) || to[..to.len() - 1].contains(&0) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "저장 경로에 NUL 문자를 사용할 수 없습니다",
            ));
        }
        // NUL 종료 UTF-16 버퍼를 호출 동안 유지하고 교체 허용 여부만 플래그로 전달.
        if unsafe { MoveFileExW(from.as_ptr(), to.as_ptr(), 8 | u32::from(overwrite)) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    if result.is_err() && cancelled() {
        check(&cancelled)
    } else {
        result
    }
}
