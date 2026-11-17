use std::{
    fs, io,
    os::windows::{
        fs::{MetadataExt, OpenOptionsExt},
        io::AsRawHandle,
    },
    path::Path,
};
use windows_sys::Win32::Storage::FileSystem::{
    FileIdInfo, GetFileInformationByHandleEx, FILE_ID_INFO,
};

/// 실패 기록의 두 경로에 한정한 읽기 전용 ID·해결 경로 대조.
pub fn run(request: &Path, output: &Path) -> io::Result<()> {
    let owned = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/rust-recovery");
    if fs::canonicalize(
        output
            .parent()
            .ok_or_else(|| io::Error::other("결과 경로 오류"))?,
    )? != fs::canonicalize(owned)?
    {
        return Err(io::Error::other("진단 결과 경로 범위 오류"));
    }
    let paths: Vec<std::path::PathBuf> =
        serde_json::from_reader(fs::File::open(request)?).map_err(crate::output::json_error)?;
    if paths.len() != 2 || paths.iter().any(|p| !p.is_absolute()) {
        return Err(io::Error::other("절대 경로 두 개만 진단 가능"));
    }
    let volume = crate::ntfs_native::target(Path::new("C:\\"))?.volume;
    let resolved = crate::ntfs_native::inspect_file(Path::new("C:\\"), false, &volume)?
        .resolved
        .ok_or_else(|| io::Error::other("드라이브 실제 위치 누락"))?;
    let mut rows = Vec::new();
    for path in paths {
        let listed = fs::read_dir(path.parent().unwrap()).and_then(|entries| {
            for entry in entries {
                if entry?.file_name() == path.file_name().unwrap() {
                    return Ok(true);
                }
            }
            Ok(false)
        });
        let inspected = crate::ntfs_native::inspect_file(&path, false, &volume);
        let row = match inspected {
            Ok(info) => {
                let relative = path.components().skip(2).collect::<std::path::PathBuf>();
                let guarded =
                    crate::usn_native::validate_resolved_entry(&resolved, &relative, &info);
                let excluded_as_alias = guarded.as_ref().is_err_and(|e| {
                    e.get_ref()
                        .is_some_and(|cause| cause.is::<crate::usn::RedirectedDirectory>())
                });
                let id = info.reference.map(|crate::ntfs::Id::V2(id)| id);
                let full = (|| -> io::Result<_> {
                    let file = fs::File::options()
                        .access_mode(128)
                        .share_mode(7)
                        .custom_flags(0x02200000)
                        .open(&path)?;
                    let mut data: FILE_ID_INFO = unsafe { std::mem::zeroed() };
                    if unsafe {
                        GetFileInformationByHandleEx(
                            file.as_raw_handle(),
                            FileIdInfo,
                            (&mut data as *mut FILE_ID_INFO).cast(),
                            std::mem::size_of_val(&data) as u32,
                        )
                    } == 0
                    {
                        return Err(io::Error::last_os_error());
                    }
                    Ok(
                        serde_json::json!({"volume":data.VolumeSerialNumber,"file":data.FileId.Identifier}),
                    )
                })();
                serde_json::json!({"path":path,"listed":listed.map_err(|e|e.to_string()),"legacy_id":id,"resolved":info.resolved,"attributes":info.attributes,"path_attributes":fs::symlink_metadata(&path).map(|m|m.file_attributes()).map_err(|e|e.to_string()),"full_id":full.map_err(|e|e.to_string()),"excluded_as_alias":excluded_as_alias,"guard_error":guarded.err().map(|e|e.to_string())})
            }
            Err(error) => {
                serde_json::json!({"path":path,"listed":listed.map_err(|e|e.to_string()),"error":error.to_string(),"os_error":error.raw_os_error()})
            }
        };
        rows.push(row);
    }
    serde_json::to_writer(
        fs::File::create(output)?,
        &serde_json::json!({"rows":rows,"existing_files_modified":false}),
    )
    .map_err(crate::output::json_error)
}
