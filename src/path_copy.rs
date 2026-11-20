use std::{io, path::PathBuf};

pub fn copy_to_clipboard(ctx: &eframe::egui::Context, paths: &[PathBuf]) -> io::Result<()> {
    copy_with(paths, |text| {
        ctx.copy_text(text);
        Ok(())
    })
}

/// 파일 접근 없이 선택 경로를 그대로 클립보드 전달 경계에 제공.
pub fn copy_with(
    paths: &[PathBuf],
    clipboard: impl FnOnce(String) -> io::Result<()>,
) -> io::Result<()> {
    if paths.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "복사할 경로가 없습니다",
        ));
    }
    let mut text = Vec::with_capacity(paths.len());
    for path in paths {
        let value = path
            .to_str()
            .filter(|s| !s.contains(['\0', '\r', '\n']))
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "텍스트로 정확하게 복사할 수 없는 경로입니다",
                )
            })?;
        if !path.is_absolute() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "전체 경로가 필요합니다",
            ));
        }
        text.push(value);
    }
    clipboard(text.join("\r\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn selected_paths_are_copied_exactly_and_failures_are_returned() {
        let paths = [
            PathBuf::from("C:\\자료 폴더\\파일.txt"),
            PathBuf::from("D:\\models"),
        ];
        let mut captured = String::new();
        copy_with(&paths, |text| {
            captured = text;
            Ok(())
        })
        .unwrap();
        assert_eq!(captured, "C:\\자료 폴더\\파일.txt\r\nD:\\models");
        assert!(copy_with(&[], |_| panic!("빈 선택 전달 금지")).is_err());
        assert_eq!(
            copy_with(&paths, |_| Err(io::Error::other("clipboard failed")))
                .unwrap_err()
                .to_string(),
            "clipboard failed"
        );
        assert!(copy_with(&[PathBuf::from("relative")], |_| panic!(
            "잘못된 입력 전달 금지"
        ))
        .is_err());
    }
}
