use std::fs::File;
use std::io;
use std::os::windows::{
    fs::{MetadataExt, OpenOptionsExt},
    io::AsRawHandle,
};
use std::path::Path;

const FILE_READ_ATTRIBUTES: u32 = 0x80;
const SHARE_READ_WRITE_DELETE: u32 = 7;
const OPEN_REPARSE_WITH_BACKUP_SEMANTICS: u32 = 0x02200000;
const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
const COMPRESSED_OR_SPARSE: u32 = 0xA00;
const FILE_STANDARD_INFO: i32 = 1;
const FILE_COMPRESSION_INFO: i32 = 8;
const FILE_ID_INFO: i32 = 18;

pub struct ScanSettings {
    pub detailed: bool,
}
impl Default for ScanSettings {
    fn default() -> Self {
        Self { detailed: true }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Missing {
    NotCollected,
    Unsupported,
    Failed,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Field<T> {
    Value(T),
    Missing(Missing),
}
impl<T> Default for Field<T> {
    fn default() -> Self {
        Self::Missing(Missing::NotCollected)
    }
}
impl<T: Copy> Field<T> {
    pub fn value(self) -> Option<T> {
        match self {
            Self::Value(v) => Some(v),
            Self::Missing(_) => None,
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct Identity {
    pub volume: u64,
    pub file: [u8; 16],
}
impl std::fmt::Display for Identity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:016X}:", self.volume)?;
        for byte in self.file {
            write!(f, "{byte:02X}")?;
        }
        Ok(())
    }
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Extra {
    pub allocation: Field<u64>,
    pub identity: Field<Identity>,
    pub links: Field<u32>,
}
pub struct Report {
    pub values: Extra,
    pub issue: Option<io::Error>,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct Allocation {
    pub bytes: u64,
    pub known: u64,
    pub not_collected: u64,
    pub unsupported: u64,
    pub failed: u64,
}
impl Allocation {
    pub fn missing(&self) -> u64 {
        self.not_collected + self.unsupported + self.failed
    }
    pub fn add(&mut self, value: Field<u64>) -> io::Result<()> {
        match value {
            Field::Value(bytes) => {
                self.bytes = self
                    .bytes
                    .checked_add(bytes)
                    .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "할당 합계 초과"))?;
                self.known += 1;
            }
            Field::Missing(Missing::NotCollected) => self.not_collected += 1,
            Field::Missing(Missing::Unsupported) => self.unsupported += 1,
            Field::Missing(Missing::Failed) => self.failed += 1,
        }
        Ok(())
    }
    pub fn merge(&mut self, other: Self) -> io::Result<()> {
        self.bytes = self
            .bytes
            .checked_add(other.bytes)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "할당 합계 초과"))?;
        self.known += other.known;
        self.not_collected += other.not_collected;
        self.unsupported += other.unsupported;
        self.failed += other.failed;
        Ok(())
    }
}
fn missing(error: &io::Error) -> Missing {
    match error.raw_os_error() {
        Some(1 | 50 | 87 | 120 | 124) => Missing::Unsupported,
        _ => Missing::Failed,
    }
}
impl Report {
    pub fn unavailable(error: io::Error) -> Self {
        let state = missing(&error);
        Self {
            values: Extra {
                allocation: Field::Missing(state),
                identity: Field::Missing(state),
                links: Field::Missing(state),
            },
            issue: Some(error),
        }
    }
}

#[repr(C)]
struct Standard {
    allocation: i64,
    eof: i64,
    links: u32,
    delete_pending: u8,
    directory: u8,
}
#[repr(C)]
struct Compression {
    bytes: i64,
    format: u16,
    unit: u8,
    chunk: u8,
    cluster: u8,
    reserved: [u8; 3],
}
#[repr(C)]
struct FileId {
    volume: u64,
    id: [u8; 16],
}
#[link(name = "kernel32")]
extern "system" {
    fn GetFileInformationByHandleEx(
        handle: *mut std::ffi::c_void,
        class: i32,
        buffer: *mut std::ffi::c_void,
        size: u32,
    ) -> i32;
}

fn query<T>(file: &File, class: i32) -> io::Result<T> {
    let mut value = std::mem::MaybeUninit::<T>::zeroed();
    // 고정 정수/바이트 배열로만 구성된 세 Win32 구조체에 한정한 버퍼. 성공 확인 뒤 초기화된 값 사용.
    let success = unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle(),
            class,
            value.as_mut_ptr().cast(),
            std::mem::size_of::<T>() as u32,
        )
    };
    if success == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(unsafe { value.assume_init() })
    }
}
fn field<T>(result: io::Result<T>, issue: &mut Option<io::Error>) -> Field<T> {
    match result {
        Ok(value) => Field::Value(value),
        Err(error) => {
            let state = missing(&error);
            if issue.is_none() {
                *issue = Some(error);
            }
            Field::Missing(state)
        }
    }
}
fn nonnegative(bytes: i64) -> io::Result<u64> {
    u64::try_from(bytes).map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "음수 할당 크기"))
}

/// 기본 데이터 스트림의 메타데이터 조회. 파일 내용·ADS·디렉터리 인덱스는 조회 대상에서 제외.
pub fn read(path: &Path) -> Report {
    let file = match File::options()
        .access_mode(FILE_READ_ATTRIBUTES)
        .share_mode(SHARE_READ_WRITE_DELETE)
        .custom_flags(OPEN_REPARSE_WITH_BACKUP_SEMANTICS)
        .open(path)
    {
        Ok(file) => file,
        Err(e) => return Report::unavailable(e),
    };
    let metadata = match file.metadata() {
        Ok(m) => m,
        Err(e) => return Report::unavailable(e),
    };
    if !metadata.is_file() || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Report::unavailable(io::Error::new(
            io::ErrorKind::InvalidInput,
            "추가 조회 대상의 종류가 변경됐거나 Reparse Point임",
        ));
    }
    let mut issue = None;
    let standard = query::<Standard>(&file, FILE_STANDARD_INFO);
    let (links, ordinary) = match standard {
        Ok(s) => (
            Field::Value(s.links),
            field(nonnegative(s.allocation), &mut issue),
        ),
        Err(e) => {
            let state = missing(&e);
            issue = Some(e);
            (Field::Missing(state), Field::Missing(state))
        }
    };
    let allocation = if metadata.file_attributes() & COMPRESSED_OR_SPARSE != 0 {
        field(
            query::<Compression>(&file, FILE_COMPRESSION_INFO).and_then(|c| nonnegative(c.bytes)),
            &mut issue,
        )
    } else {
        ordinary
    };
    let identity = field(
        query::<FileId>(&file, FILE_ID_INFO).map(|id| Identity {
            volume: id.volume,
            file: id.id,
        }),
        &mut issue,
    );
    Report {
        values: Extra {
            allocation,
            identity,
            links,
        },
        issue,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    #[ignore = "임시 NTFS 파일만 Sparse/압축 처리하는 별도 검증"]
    fn ntfs_sparse_compression_and_ads_are_measured_without_reading_file_contents() {
        use std::io::{Seek, SeekFrom, Write};
        use std::os::windows::ffi::OsStrExt;
        #[link(name = "kernel32")]
        extern "system" {
            fn DeviceIoControl(
                handle: *mut std::ffi::c_void,
                code: u32,
                input: *const std::ffi::c_void,
                input_size: u32,
                output: *mut std::ffi::c_void,
                output_size: u32,
                returned: *mut u32,
                overlapped: *mut std::ffi::c_void,
            ) -> i32;
            fn GetCompressedFileSizeW(path: *const u16, high: *mut u32) -> u32;
        }
        fn control(file: &File, code: u32, input: Option<&u16>) {
            let mut returned = 0;
            let (data, size) = input.map_or((std::ptr::null(), 0), |p| {
                (p as *const u16 as *const std::ffi::c_void, 2)
            });
            // 시험용 파일 핸들에 한정한 FSCTL 변경. 모든 입력/출력 버퍼는 호출 동안 유지.
            assert_ne!(
                unsafe {
                    DeviceIoControl(
                        file.as_raw_handle(),
                        code,
                        data,
                        size,
                        std::ptr::null_mut(),
                        0,
                        &mut returned,
                        std::ptr::null_mut(),
                    )
                },
                0,
                "{}",
                io::Error::last_os_error()
            );
        }
        let root =
            std::env::temp_dir().join(format!("diskweave-special-metadata-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let sparse = root.join("sparse.bin");
        let mut f = File::options()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&sparse)
            .unwrap();
        control(&f, 0x900c4, None);
        f.set_len(64 * 1024 * 1024).unwrap();
        f.write_all(&[7; 4096]).unwrap();
        f.sync_all().unwrap();
        drop(f);
        let compressed = root.join("compressed.bin");
        let mut f = File::options()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&compressed)
            .unwrap();
        control(&f, 0x9c040, Some(&2));
        f.write_all(&vec![0; 1024 * 1024]).unwrap();
        f.seek(SeekFrom::Start(0)).unwrap();
        f.sync_all().unwrap();
        drop(f);
        for path in [&sparse, &compressed] {
            let report = read(path);
            assert!(report.issue.is_none(), "{:?}", report.issue);
            let allocated = report.values.allocation.value().unwrap();
            assert!(allocated < std::fs::metadata(path).unwrap().len());
            let name: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
            let mut high = 0;
            // 별도 경로 기반 OS 조회와 핸들 기반 수집 결과 대조. 시험 파일 크기는 4 GiB 미만.
            let low = unsafe { GetCompressedFileSizeW(name.as_ptr(), &mut high) };
            assert_ne!(low, u32::MAX);
            assert_eq!(allocated, ((high as u64) << 32) | low as u64);
        }
        std::fs::write(root.join("sparse.bin:extra"), vec![1; 16384]).unwrap();
        let scan = crate::scanner::scan_with_options(&root, true, || false, |_| {}).unwrap();
        assert_eq!(scan.files.len(), 2);
        assert_eq!(scan.folders[0].logical_bytes, 65 * 1024 * 1024);
        assert_eq!(
            scan.files
                .iter()
                .find(|f| f.name == "sparse.bin")
                .unwrap()
                .extra
                .allocation
                .value(),
            read(&sparse).values.allocation.value()
        );
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn metadata_identifies_hard_links_and_keeps_allocation_separate_from_length() {
        let root = std::env::temp_dir().join(format!("diskweave-metadata-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("한글 파일.bin");
        std::fs::write(&path, vec![7; 8193]).unwrap();
        std::fs::hard_link(&path, root.join("alias.bin")).unwrap();
        std::fs::write(root.join("other.bin"), vec![7; 8193]).unwrap();
        let report = read(&path);
        assert!(report.issue.is_none(), "{:?}", report.issue);
        let id = report.values.identity.value().unwrap();
        assert_eq!(
            read(&root.join("alias.bin")).values.identity.value(),
            Some(id)
        );
        assert_ne!(
            read(&root.join("other.bin")).values.identity.value(),
            Some(id)
        );
        assert_eq!(report.values.links.value(), Some(2));
        assert!(report.values.allocation.value().unwrap() >= 8193);
        assert!(matches!(
            Extra::default().allocation,
            Field::Missing(Missing::NotCollected)
        ));
        let failed = read(&root.join("missing"));
        assert!(matches!(
            failed.values.allocation,
            Field::Missing(Missing::Failed)
        ));
        assert!(failed.issue.is_some());
        std::fs::remove_dir_all(root).unwrap();
    }
}
