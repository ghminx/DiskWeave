use crate::file_ops::Target;
use std::{
    io,
    os::windows::ffi::OsStrExt,
    path::{Path, PathBuf},
};
use windows::{
    core::PCWSTR,
    Win32::{
        System::Com::{CoInitializeEx, CoTaskMemFree, CoUninitialize, COINIT_APARTMENTTHREADED},
        UI::Shell::*,
    },
};
fn error(e: windows::core::Error) -> io::Error {
    io::Error::other(format!("Windows 휴지통 처리 실패: {e}"))
}
struct Apartment;
impl Drop for Apartment {
    fn drop(&mut self) {
        unsafe {
            CoUninitialize();
        }
    }
}
/// 영구 제거 API로 전환하지 않고 Windows의 명시적 휴지통 동작만 요청.
pub fn send(target: &Target) -> io::Result<PathBuf> {
    target.verify()?;
    let ordinary = crate::file_ops::comparable_path(&target.path)?;
    let root = ordinary
        .ancestors()
        .last()
        .ok_or_else(|| io::Error::other("휴지통 볼륨 확인 실패"))?;
    crate::ntfs_native::target(root).map_err(|e| {
        io::Error::new(
            io::ErrorKind::Unsupported,
            format!("휴지통 이동은 로컬 NTFS 경로만 지원합니다. {e}"),
        )
    })?;
    let root_wide: Vec<_> = root.as_os_str().encode_wide().chain(Some(0)).collect();
    if unsafe { windows_sys::Win32::Storage::FileSystem::GetDriveTypeW(root_wide.as_ptr()) } != 3 {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "이 위치의 휴지통 지원을 확인할 수 없습니다",
        ));
    }
    unsafe {
        CoInitializeEx(None, COINIT_APARTMENTTHREADED)
            .ok()
            .map_err(error)?;
    }
    let _apartment = Apartment;
    let path: Vec<_> = target
        .path
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect();
    // STA 스레드 안에서만 COM 참조를 보유하며 셸 공급자의 RecycleItem만 호출.
    let recycled = unsafe {
        let source: IShellItem =
            SHCreateItemFromParsingName(PCWSTR(path.as_ptr()), None).map_err(error)?;
        let parent = source.GetParent().map_err(error)?;
        let transfer: ITransferSource =
            parent.BindToHandler(None, &BHID_Transfer).map_err(error)?;
        let bin: IShellItem =
            SHGetKnownFolderItem(&FOLDERID_RecycleBinFolder, KF_FLAG_DEFAULT, None)
                .map_err(error)?;
        let item = transfer
            .RecycleItem(&source, &bin, TSF_NORMAL.0 as u32)
            .map_err(error)?;
        let text = item.GetDisplayName(SIGDN_FILESYSPATH).map_err(error)?;
        let len = text.len();
        use std::os::windows::ffi::OsStringExt;
        let path = PathBuf::from(std::ffi::OsString::from_wide(std::slice::from_raw_parts(
            text.0, len,
        )));
        CoTaskMemFree(Some(text.0.cast()));
        path
    };
    if recycled == target.path || Path::new(&target.path).try_exists()? {
        return Err(io::Error::other("휴지통 이동 완료를 확인하지 못했습니다"));
    }
    Ok(recycled)
}
