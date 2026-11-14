mod scanner;

use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

static CANCELLED: AtomicBool = AtomicBool::new(false);

#[link(name = "kernel32")]
extern "system" {
    fn SetConsoleCtrlHandler(
        handler: Option<unsafe extern "system" fn(u32) -> i32>,
        add: i32,
    ) -> i32;
}

unsafe extern "system" fn console_control(event: u32) -> i32 {
    if event == 0 || event == 1 {
        CANCELLED.store(true, Ordering::Relaxed);
        1
    } else {
        0
    }
}

fn main() {
    if let Err(error) = run() {
        eprintln!("오류: {error}");
        std::process::exit(if error.kind() == io::ErrorKind::Interrupted {
            130
        } else {
            1
        });
    }
}

fn run() -> io::Result<()> {
    let mut args = std::env::args_os();
    let _program = args.next();
    let root = match (args.next(), args.next()) {
        (Some(path), None) => PathBuf::from(path),
        _ => {
            eprintln!("사용법: diskweave <폴더 경로>");
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "폴더 경로 하나가 필요합니다",
            ));
        }
    };

    // 콘솔 제어 이벤트를 스캔 취소 플래그로 전달하기 위한 Windows API 호출
    if unsafe { SetConsoleCtrlHandler(Some(console_control), 1) } == 0 {
        return Err(io::Error::last_os_error());
    }

    let started = Instant::now();
    let result = scanner::scan(&root, || CANCELLED.load(Ordering::Relaxed))?;
    let mut children = vec![Vec::new(); result.folders.len()];
    for (index, folder) in result.folders.iter().enumerate().skip(1) {
        children[folder.parent.expect("루트 외 폴더의 부모")].push(index);
    }
    for siblings in &mut children {
        siblings.sort_unstable_by(|&a, &b| {
            result.folders[b]
                .logical_bytes
                .cmp(&result.folders[a].logical_bytes)
                .then_with(|| result.folders[a].path.cmp(&result.folders[b].path))
        });
    }

    println!("논리 크기(Byte)\t파일 수\t하위 폴더 수\t경로");
    let mut pending = vec![0];
    while let Some(index) = pending.pop() {
        let folder = &result.folders[index];
        println!(
            "{}\t{}\t{}\t{}",
            folder.logical_bytes,
            folder.file_count,
            folder.folder_count,
            folder.path.display()
        );
        pending.extend(children[index].iter().rev());
    }

    eprintln!(
        "완료: {:.2?}, 폴더 {}개, 재분석 지점 제외 {}개, 오류 {}건{}",
        started.elapsed(),
        result.folders.len(),
        result.skipped_reparse_points,
        result.error_count,
        if result.error_count > 0 {
            " (결과 일부 누락)"
        } else {
            ""
        }
    );
    for issue in &result.issues {
        eprintln!("  {}: {}", issue.path.display(), issue.error);
    }
    if result.error_count > result.issues.len() as u64 {
        eprintln!(
            "  나머지 {}건 생략",
            result.error_count - result.issues.len() as u64
        );
    }
    Ok(())
}
