use std::{
    fs,
    process::Command,
    time::{SystemTime, UNIX_EPOCH},
};

#[test]
fn cli_distinguishes_empty_and_unreadable_folders_without_fabricating_zero_totals() {
    let root = std::env::temp_dir().join(format!(
        "diskweave-cli-status-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir(&root).unwrap();
    let denied = root.join("denied");
    let empty = root.join("empty");
    fs::create_dir(&denied).unwrap();
    fs::create_dir(&empty).unwrap();
    fs::write(denied.join("payload"), b"123").unwrap();
    fs::write(root.join("known"), b"12345").unwrap();
    let user = format!(
        "{}\\{}",
        std::env::var("USERDOMAIN").unwrap(),
        std::env::var("USERNAME").unwrap()
    );
    assert!(Command::new("icacls")
        .arg(&denied)
        .args(["/deny", &format!("{user}:(RD)")])
        .output()
        .unwrap()
        .status
        .success());
    let outcome = std::panic::catch_unwind(|| {
        let output = Command::new(env!("CARGO_BIN_EXE_diskweave"))
            .arg(&root)
            .output()
            .unwrap();
        assert!(output.status.success());
        let text = String::from_utf8(output.stdout).unwrap();
        let rows: Vec<Vec<_>> = text
            .lines()
            .skip(1)
            .map(|line| line.split('\t').collect())
            .collect();
        let blocked = rows
            .iter()
            .find(|row| row[3].ends_with("\\denied"))
            .unwrap();
        assert_eq!(&blocked[..3], &["", "", ""]);
        assert_eq!(blocked[4], "접근 불가");
        let known = rows.iter().find(|row| row[3].ends_with("\\empty")).unwrap();
        assert_eq!(&known[..3], &["0", "0", "0"]);
        assert_eq!(known[4], "정상");
        let partial = rows
            .iter()
            .find(|row| row[3] == root.to_str().unwrap())
            .unwrap();
        assert_eq!(&partial[..3], &["5", "1", "2"]);
        assert_eq!(partial[4], "일부 누락");
        assert_eq!(text.lines().next().unwrap().split('\t').count(), 5);
        assert!(String::from_utf8(output.stderr)
            .unwrap()
            .contains("결과 일부 누락"));
    });
    assert!(Command::new("icacls")
        .arg(&denied)
        .args(["/remove:d", &user])
        .output()
        .unwrap()
        .status
        .success());
    fs::remove_file(denied.join("payload")).unwrap();
    fs::remove_file(root.join("known")).unwrap();
    fs::remove_dir(denied).unwrap();
    fs::remove_dir(empty).unwrap();
    fs::remove_dir(root).unwrap();
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic);
    }
}
