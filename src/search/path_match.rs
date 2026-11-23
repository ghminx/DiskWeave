use std::{cell::OnceCell, ffi::OsStr, path::Path};

pub(super) struct FolderPath<'a> {
    folder: &'a Path,
    prefix: OnceCell<Option<String>>,
    buffer: &'a mut String,
}

impl<'a> FolderPath<'a> {
    pub(super) fn new(folder: &'a Path, buffer: &'a mut String) -> Self {
        Self {
            folder,
            prefix: OnceCell::new(),
            buffer,
        }
    }

    pub(super) fn contains(&mut self, file_name: &OsStr, query: &str) -> Option<bool> {
        let name = file_name.to_str()?;
        if name.is_empty()
            || name.ends_with('.')
            || !name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
            || reserved_device_name(name)
        {
            return None;
        }
        let prefix = self
            .prefix
            .get_or_init(|| normalized_prefix(self.folder))
            .as_deref()?;
        if prefix.len() + name.len() > 240 {
            return None;
        }

        self.buffer.clear();
        self.buffer.push_str(prefix);
        self.buffer.push_str(name);
        self.buffer.make_ascii_lowercase();
        Some(self.buffer.contains(query))
    }
}

fn normalized_prefix(folder: &Path) -> Option<String> {
    let value = folder.to_str()?;
    let bytes = value.as_bytes();
    // 정규화가 이미 끝난 ASCII 드라이브 절대 경로만 조립을 재사용.
    if bytes.len() < 3
        || !bytes[0].is_ascii_alphabetic()
        || &bytes[1..3] != b":\\"
        || !value.is_ascii()
        || value.contains('/')
        || std::path::absolute(folder).ok()?.as_os_str() != folder.as_os_str()
    {
        return None;
    }
    let mut prefix = value.to_ascii_lowercase();
    if !prefix.ends_with('\\') {
        prefix.push('\\');
    }
    Some(prefix)
}

fn reserved_device_name(name: &str) -> bool {
    let stem = name.split('.').next().unwrap_or_default().as_bytes();
    [b"con", b"prn", b"aux", b"nul"]
        .iter()
        .any(|device| stem.eq_ignore_ascii_case(*device))
        || (stem.len() == 4
            && stem[3].is_ascii_digit()
            && [b"com", b"lpt"]
                .iter()
                .any(|device| stem[..3].eq_ignore_ascii_case(*device)))
}
