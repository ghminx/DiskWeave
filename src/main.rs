mod analysis;
mod analysis_panel;
mod comparison;
mod documents;
mod drives;
mod engine;
mod explorer;
mod export;
mod file_open;
mod global_search;
mod gui;
mod index_job;
mod index_store;
mod list_selection;
mod local_date;
mod locations;
mod locations_ui;
mod map_job;
mod metadata;
mod ntfs;
mod ntfs_helper;
mod ntfs_native;
mod output;
mod presentation;
mod results_ui;
mod scan_job;
mod scanner;
mod search;
mod search_job;
mod search_view;
mod snapshot;
mod target_selection;
mod tracking;
mod treemap;
mod treemap_view;
mod ui_style;
mod usn;
mod usn_native;
mod workspace;

use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

static CANCELLED: AtomicBool = AtomicBool::new(false);

enum View {
    Tree,
    Search(String),
    Children(PathBuf),
}

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
    let arguments: Vec<_> = args.collect();
    if arguments.first().is_some_and(|a| a == "--ntfs-worker") {
        return ntfs_helper::worker(&arguments[1..]);
    }
    if arguments.is_empty() {
        return gui::run().map_err(|error| io::Error::other(error.to_string()));
    }
    let (root, view) = match arguments.as_slice() {
        [path] => (PathBuf::from(path), View::Tree),
        [path, option, query] if option == "--find" && !query.is_empty() => (
            PathBuf::from(path),
            View::Search(query.to_string_lossy().into_owned()),
        ),
        [path, option, relative_path] if option == "--at" && !relative_path.is_empty() => (
            PathBuf::from(path),
            View::Children(PathBuf::from(relative_path)),
        ),
        _ => {
            eprintln!("사용법: diskweave <폴더 경로> [--find <폴더 이름 일부> | --at <하위 폴더 상대 경로>]");
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "분석 경로 또는 검색 인자가 올바르지 않습니다",
            ));
        }
    };

    // 콘솔 제어 이벤트를 스캔 취소 플래그로 전달하기 위한 Windows API 호출
    if unsafe { SetConsoleCtrlHandler(Some(console_control), 1) } == 0 {
        return Err(io::Error::last_os_error());
    }

    let started = Instant::now();
    let result = scanner::scan(&root, || CANCELLED.load(Ordering::Relaxed))?;

    let (mut pending, recurse, search_count) = match view {
        View::Tree => (vec![0], true, None),
        View::Search(query) => {
            let matches = result.search_folders(&query);
            let count = matches.len();
            (matches.into_iter().rev().collect(), false, Some(count))
        }
        View::Children(relative_path) => {
            let index = result.folder_index(&relative_path)?;
            let mut pending: Vec<_> = result.children[index].iter().rev().copied().collect();
            pending.push(index);
            (pending, false, None)
        }
    };
    println!("논리 크기(Byte)\t파일 수\t하위 폴더 수\t경로");
    while let Some(index) = pending.pop() {
        let folder = &result.folders[index];
        println!(
            "{}\t{}\t{}\t{}",
            folder.logical_bytes,
            folder.file_count,
            folder.folder_count,
            folder.path.display()
        );
        if recurse {
            pending.extend(result.children[index].iter().rev());
        }
    }

    if let Some(count) = search_count {
        eprintln!("검색 결과: {count}개 폴더");
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
