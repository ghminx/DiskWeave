use crate::file_ops::{self, Action, Collision, Request};
use std::{
    fs, io,
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
};
fn plan(action: Action, paths: Vec<PathBuf>, roots: &[PathBuf]) -> io::Result<file_ops::Plan> {
    file_ops::prepare(
        Request {
            action,
            paths,
            protected: roots.to_vec(),
        },
        &|| false,
        &mut |_| {},
    )
}
fn success(report: file_ops::Report, label: &str) -> io::Result<file_ops::Report> {
    if report.cancelled || report.items.iter().any(|i| !i.succeeded()) {
        return Err(io::Error::other(format!("{label}: {:?}", report.items)));
    }
    Ok(report)
}
pub fn run(volume: &Path, owned: &Path) -> io::Result<()> {
    if unsafe { windows_sys::Win32::UI::Shell::IsUserAnAdmin() } != 0 {
        return Err(io::Error::other(
            "파일 관리 시험은 일반 사용자 프로세스에서 실행해야 함",
        ));
    }
    let suffix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let source = owned.join(format!("client-files-{suffix}"));
    let destination = volume.join(format!("FileOperations-{suffix}"));
    fs::create_dir(&source)?;
    fs::create_dir(&destination)?;
    let roots = vec![source.clone(), destination.clone()];
    let mut cases = vec![];
    eprintln!("file-management: cross-volume copy");
    fs::write(source.join("data.bin"), b"original bytes")?;
    fs::write(source.join("data.bin:note"), b"owned ADS")?;
    let copy = plan(
        Action::Copy {
            destination: destination.clone(),
        },
        vec![source.join("data.bin")],
        &roots,
    )?;
    success(file_ops::execute(copy, &|| false, &mut |_| {}), "copy")?;
    if fs::read(destination.join("data.bin"))? != b"original bytes"
        || fs::read(destination.join("data.bin:note"))? != b"owned ADS"
    {
        return Err(io::Error::other("복사 내용·ADS 불일치"));
    }
    cases.push(serde_json::json!({"case":"cross-volume-copy-and-ads","matched":true}));
    fs::write(source.join("data.bin"), b"updated bytes")?;
    let mut copy = plan(
        Action::Copy {
            destination: destination.clone(),
        },
        vec![source.join("data.bin")],
        &roots,
    )?;
    copy.resolve(0, Collision::Replace)?;
    let replaced = file_ops::execute(copy, &|| false, &mut |_| {});
    let expected_bytes: &[u8];
    if replaced.items.iter().all(|i| i.succeeded()) {
        expected_bytes = b"updated bytes";
        cases.push(serde_json::json!({"case":"confirmed-file-replacement","matched":true,"outcome":"completed"}));
    } else {
        use std::os::windows::ffi::OsStrExt;
        let old = destination.join("reference-old");
        let new = destination.join("reference-new");
        fs::write(&old, b"reference old")?;
        fs::write(&new, b"reference new")?;
        let old_wide: Vec<_> = old.as_os_str().encode_wide().chain(Some(0)).collect();
        let new_wide: Vec<_> = new.as_os_str().encode_wide().chain(Some(0)).collect();
        let native = unsafe {
            windows_sys::Win32::Storage::FileSystem::ReplaceFileW(
                old_wide.as_ptr(),
                new_wide.as_ptr(),
                std::ptr::null(),
                0,
                std::ptr::null(),
                std::ptr::null(),
            )
        };
        let code = io::Error::last_os_error().raw_os_error();
        let denied = replaced.items.iter().all(|item| matches!(&item.outcome,file_ops::Outcome::Failed(error) if error.contains(&io::Error::from_raw_os_error(5).to_string())));
        if !denied
            || native != 0
            || code != Some(5)
            || fs::read(source.join("data.bin"))? != b"updated bytes"
            || fs::read(destination.join("data.bin"))? != b"original bytes"
        {
            return Err(io::Error::other(format!(
                "파일 교체 실패가 Windows 기준과 다름: {:?}",
                replaced.items
            )));
        }
        expected_bytes = b"original bytes";
        cases.push(serde_json::json!({"case":"confirmed-file-replacement","matched":true,"outcome":"denied-consistent-with-Windows-reference","windows_error":5,"source_and_target_preserved":true}));
    }
    if fs::read(destination.join("data.bin"))? != expected_bytes {
        return Err(io::Error::other("파일 바꾸기 결과 불일치"));
    }
    eprintln!("file-management: cross-volume move in both directions");
    fs::create_dir_all(source.join("moving/deep"))?;
    fs::write(source.join("moving/deep/keep"), b"move tree")?;
    let moving = plan(
        Action::Move {
            destination: destination.clone(),
        },
        vec![source.join("moving")],
        &roots,
    )?;
    success(
        file_ops::execute(moving, &|| false, &mut |_| {}),
        "move-out",
    )?;
    if source.join("moving").exists()
        || fs::read(destination.join("moving/deep/keep"))? != b"move tree"
    {
        return Err(io::Error::other("폴더 이동 불일치"));
    }
    let moving = plan(
        Action::Move {
            destination: source.clone(),
        },
        vec![destination.join("moving")],
        &roots,
    )?;
    success(
        file_ops::execute(moving, &|| false, &mut |_| {}),
        "move-back",
    )?;
    if destination.join("moving").exists()
        || fs::read(source.join("moving/deep/keep"))? != b"move tree"
    {
        return Err(io::Error::other("복귀 이동 불일치"));
    }
    cases.push(
        serde_json::json!({"case":"cross-volume-folder-move-both-directions","matched":true}),
    );
    let large = source.join("cancel.bin");
    fs::File::create(&large)?.set_len(16 * 1024 * 1024)?;
    let moving = plan(
        Action::Move {
            destination: destination.clone(),
        },
        vec![large.clone()],
        &roots,
    )?;
    let cancel = AtomicBool::new(false);
    let result = file_ops::execute(moving, &|| cancel.load(Ordering::Relaxed), &mut |p| {
        if p.bytes_done > 0 {
            cancel.store(true, Ordering::Relaxed);
        }
    });
    if !result.cancelled || !large.exists() || destination.join("cancel.bin").exists() {
        return Err(io::Error::other(format!(
            "교차 볼륨 취소 원본 보존 실패: cancelled={} source={} destination={} items={:?}",
            result.cancelled,
            large.exists(),
            destination.join("cancel.bin").exists(),
            result.items
        )));
    }
    cases.push(serde_json::json!({"case":"cross-volume-cancel-preserves-source","matched":true}));
    eprintln!("file-management: actual recycle-only file and folder");
    let discarded = destination.join("discarded");
    fs::create_dir_all(discarded.join("inside"))?;
    fs::write(discarded.join("inside/keep"), b"recycled folder bytes")?;
    let old_file = destination.join("data.bin");
    let recycling = plan(
        Action::Recycle,
        vec![old_file.clone(), discarded.clone()],
        &roots,
    )?;
    let result = success(
        file_ops::execute(recycling, &|| false, &mut |_| {}),
        "recycle-only",
    )?;
    if old_file.exists() || discarded.exists() {
        return Err(io::Error::other("휴지통 이동 후 원본이 남았습니다"));
    }
    for item in &result.items {
        let source = item.source.as_ref().unwrap();
        if source == &old_file {
            if fs::read(&item.destination)? != expected_bytes {
                return Err(io::Error::other("휴지통 파일 내용 불일치"));
            }
        } else if fs::read(item.destination.join("inside/keep"))? != b"recycled folder bytes" {
            return Err(io::Error::other("휴지통 폴더 내용 불일치"));
        }
        if !item.destination.starts_with(volume) {
            return Err(io::Error::other("시험 볼륨 밖의 휴지통 응답"));
        }
    }
    cases.push(serde_json::json!({"case":"recycle-file-and-folder-content-preserved","matched":true,"items":result.items.len()}));
    println!(
        "{}",
        serde_json::json!({"cases":cases,"verified":"owned file-management fixtures only","elevated":false})
    );
    Ok(())
}
