//! 제품 경로와 분리된 파일 배치 일괄 수집 실험.

use std::{collections::HashSet, ffi::OsString, io, os::windows::ffi::OsStringExt};

pub use crate::ntfs_tree::{scan_with_probe, Name, Record, Stream};
fn invalid() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "파일 배치 응답의 길이·버전·연결이 잘못됐습니다",
    )
}
fn bytes(data: &[u8], offset: usize, size: usize) -> io::Result<&[u8]> {
    data.get(offset..offset.checked_add(size).ok_or_else(invalid)?)
        .ok_or_else(invalid)
}
fn u32_at(data: &[u8], offset: usize) -> io::Result<u32> {
    Ok(u32::from_le_bytes(
        bytes(data, offset, 4)?.try_into().unwrap(),
    ))
}
fn u64_at(data: &[u8], offset: usize) -> io::Result<u64> {
    Ok(u64::from_le_bytes(
        bytes(data, offset, 8)?.try_into().unwrap(),
    ))
}
fn text(data: &[u8], offset: usize, size: usize) -> io::Result<OsString> {
    if !size.is_multiple_of(2) {
        return Err(invalid());
    }
    let units: Vec<_> = bytes(data, offset, size)?
        .as_chunks::<2>()
        .0
        .iter()
        .map(|v| u16::from_le_bytes([v[0], v[1]]))
        .collect();
    if units.contains(&0) {
        return Err(invalid());
    }
    Ok(OsString::from_wide(&units))
}
fn next_slice(data: &[u8], start: usize, next: usize, minimum: usize) -> io::Result<&[u8]> {
    if start < minimum || !start.is_multiple_of(8) {
        return Err(invalid());
    }
    let end = if next == 0 {
        data.len()
    } else {
        if next < minimum || !next.is_multiple_of(8) {
            return Err(invalid());
        }
        start.checked_add(next).ok_or_else(invalid)?
    };
    data.get(start..end).ok_or_else(invalid)
}
fn decode(data: &[u8]) -> io::Result<Vec<Record>> {
    bytes(data, 0, 16)?;
    let count = u32_at(data, 0)? as usize;
    if count == 0 || count > data.len() / 40 {
        return Err(invalid());
    }
    let mut offset = u32_at(data, 4)? as usize;
    let mut records = Vec::with_capacity(count);
    for index in 0..count {
        let next = u32_at(data, offset + 4)? as usize;
        if (next == 0) != (index + 1 == count) {
            return Err(invalid());
        }
        let entry = next_slice(data, offset, next, 16)?;
        bytes(entry, 0, 40)?;
        if u32_at(entry, 0)? != 1 {
            return Err(invalid());
        }
        let mut record = Record {
            reference: u64_at(entry, 16)?,
            attributes: u32_at(entry, 12)?,
            names: Vec::new(),
            streams: Vec::new(),
            times: None,
        };
        let mut name = u32_at(entry, 24)? as usize;
        while name != 0 {
            let next = u32_at(entry, name)? as usize;
            let item = next_slice(entry, name, next, 24)?;
            let size = u32_at(item, 16)? as usize;
            record.names.push(Name {
                cached: None,
                parent: u64_at(item, 8)?,
                flags: u32_at(item, 4)?,
                name: text(item, 24, size)?,
            });
            if next == 0 {
                break;
            }
            name += next;
        }
        let mut stream = u32_at(entry, 28)? as usize;
        while stream != 0 {
            let next = u32_at(entry, stream + 4)? as usize;
            let item = next_slice(entry, stream, next, 40)?;
            bytes(item, 0, 48)?;
            // 실제 시험에서 반환된 V1도 동일한 48 Byte 고정 접두부 사용. 미확인 버전 거부.
            if ![1, 2].contains(&u32_at(item, 0)?) {
                return Err(invalid());
            }
            let allocation = u64_at(item, 16)?;
            let size = u64_at(item, 24)?;
            if allocation > i64::MAX as u64 || size > i64::MAX as u64 {
                return Err(invalid());
            }
            record.streams.push(Stream {
                kind: u32_at(item, 36)?,
                allocation,
                bytes: size,
                name: text(item, 48, u32_at(item, 44)? as usize)?,
            });
            if next == 0 {
                break;
            }
            stream += next;
        }
        let extra = u32_at(entry, 32)? as usize;
        if extra != 0 {
            let size = u32_at(entry, 36)? as usize;
            // RS5 이전의 길이 0은 SDK 계약상 Usn까지의 정보가 있음을 의미.
            let size = if size == 0 { 56 } else { size };
            let info = bytes(entry, extra, size)?;
            if size < 32 {
                return Err(invalid());
            }
            record.times = Some((u64_at(info, 0)? as i64, u64_at(info, 16)? as i64));
        }
        records.push(record);
        offset += next;
    }
    Ok(records)
}
/// 공개 일괄 수집 경계. 취소/손상/중복 레코드에서 부분 결과 게시 금지.
pub fn collect(
    page: &mut impl FnMut() -> io::Result<Option<Vec<u8>>>,
    cancelled: impl Fn() -> bool,
) -> io::Result<Vec<Record>> {
    let mut records = Vec::new();
    let mut seen = HashSet::new();
    loop {
        if cancelled() {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "파일 배치 수집 취소",
            ));
        }
        let Some(data) = page()? else {
            break;
        };
        for row in decode(&data)? {
            if cancelled() {
                return Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    "파일 배치 수집 취소",
                ));
            }
            if !seen.insert(row.reference) {
                return Err(invalid());
            }
            records.push(row);
        }
    }
    Ok(records)
}

pub struct Native {
    volume: std::fs::File,
    first: bool,
    pages: u64,
    buffer: Vec<u64>,
}
impl Native {
    pub fn open(target: &crate::ntfs_native::Target) -> io::Result<Self> {
        use std::os::windows::io::FromRawHandle;
        use windows_sys::Win32::{Foundation::*, Storage::FileSystem::*};
        if crate::ntfs_native::target(std::path::Path::new(&target.root))?.volume != target.volume {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "시험 볼륨 변경",
            ));
        }
        let device: Vec<_> = target
            .volume
            .trim_end_matches('\\')
            .encode_utf16()
            .chain(Some(0))
            .collect();
        // 검증한 볼륨에 GENERIC_READ만 요청. 변경 제어 코드는 호출하지 않음.
        let handle = unsafe {
            CreateFileW(
                device.as_ptr(),
                GENERIC_READ,
                7,
                std::ptr::null(),
                OPEN_EXISTING,
                0,
                std::ptr::null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            volume: unsafe { std::fs::File::from_raw_handle(handle) },
            first: true,
            pages: 0,
            buffer: vec![0u64; 131072],
        })
    }
    pub fn page(&mut self) -> io::Result<Option<Vec<u8>>> {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::{
            Foundation::ERROR_HANDLE_EOF,
            System::{Ioctl::*, IO::DeviceIoControl},
        };
        let request = QUERY_FILE_LAYOUT_INPUT {
            Flags: QUERY_FILE_LAYOUT_INCLUDE_NAMES
                | QUERY_FILE_LAYOUT_INCLUDE_STREAMS
                | QUERY_FILE_LAYOUT_INCLUDE_EXTRA_INFO
                | QUERY_FILE_LAYOUT_INCLUDE_STREAMS_WITH_NO_CLUSTERS_ALLOCATED
                | if self.first {
                    QUERY_FILE_LAYOUT_RESTART
                } else {
                    0
                },
            ..Default::default()
        };
        let mut returned = 0;
        // SDK ABI 입력과 8 Byte 정렬 버퍼를 전달하고 OS가 보고한 유효 길이만 해석.
        let ok = unsafe {
            DeviceIoControl(
                self.volume.as_raw_handle(),
                FSCTL_QUERY_FILE_LAYOUT,
                (&request as *const QUERY_FILE_LAYOUT_INPUT).cast(),
                std::mem::size_of_val(&request) as u32,
                self.buffer.as_mut_ptr().cast(),
                (self.buffer.len() * 8) as u32,
                &mut returned,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(ERROR_HANDLE_EOF as i32) {
                return Ok(None);
            }
            return Err(error);
        }
        self.first = false;
        self.pages += 1;
        if returned as usize > self.buffer.len() * 8 {
            return Err(invalid());
        }
        // u64 배정 범위 안의 초기화된 바이트에 한정한 읽기.
        Ok(Some(
            unsafe {
                std::slice::from_raw_parts(self.buffer.as_ptr().cast::<u8>(), returned as usize)
            }
            .to_vec(),
        ))
    }
}

/// 소유 표식·GUID를 확인한 검증 실행기에서 호출하는 실제 API 가능성 시험.
pub fn validate(root: &std::path::Path, target: &crate::ntfs_native::Target) -> io::Result<()> {
    let started = std::time::Instant::now();
    let mut native = Native::open(target)?;
    let records = collect(&mut || native.page(), || false)?;
    let examples: Vec<_> = records.iter().filter(|r| r.names.iter().any(|n| ["regular.bin", "sparse.bin", "compressed.bin", "empty"].iter().any(|v| n.name == std::ffi::OsStr::new(v))))
        .map(|r| serde_json::json!({"reference":r.reference,"attributes":r.attributes,"times":r.times,"names":r.names.iter().map(|n|serde_json::json!({"parent":n.parent,"name":n.name.to_string_lossy(),"flags":n.flags})).collect::<Vec<_>>(),"streams":r.streams.iter().map(|s|serde_json::json!({"kind":s.kind,"name":s.name.to_string_lossy(),"size":s.bytes,"allocation":s.allocation})).collect::<Vec<_>>()})).collect();
    let raw_ms = started.elapsed().as_secs_f64() * 1000.0;
    let mut trials = Vec::new();
    let mut eligible = true;
    for detailed in [false, true] {
        for round in 1..=3 {
            let at = std::time::Instant::now();
            let reference = crate::scanner::scan_with_strategy(
                &root.join("Cases"),
                detailed,
                crate::scanner::Parallelism::Eight,
                crate::scanner::directory::Strategy::Combined,
                || false,
                |_| {},
                None,
            )?;
            let reference_ms = at.elapsed().as_secs_f64() * 1000.0;
            let at = std::time::Instant::now();
            let candidate = scan(&root.join("Cases"), target, detailed, || false)?;
            let candidate_ms = at.elapsed().as_secs_f64() * 1000.0;
            let diff = differences(&reference, &candidate);
            if !diff.is_empty() {
                eligible = false;
            }
            if candidate
                .files
                .iter()
                .find(|f| f.name == "regular.bin")
                .is_none_or(|f| {
                    f.logical_bytes != 13
                        || detailed && f.extra.links != crate::metadata::Field::Value(3)
                })
            {
                return Err(io::Error::other(
                    "범위 밖 Hard Link를 포함한 독립 manifest 불일치",
                ));
            }
            trials.push(serde_json::json!({"detailed":detailed,"round":round,"reference_ms":reference_ms,"candidate_ms":candidate_ms,"files":candidate.files.len(),"folders":candidate.folders.len(),"errors":candidate.error_count,"equal":diff.is_empty(),"differences":diff.into_iter().take(5).collect::<Vec<_>>()}));
        }
    }
    let cancel = std::cell::Cell::new(0usize);
    let mut source = Native::open(target)?;
    let error = collect(
        &mut || {
            let page = source.page()?;
            cancel.set(cancel.get() + 1);
            Ok(page)
        },
        || cancel.get() >= 1,
    )
    .unwrap_err();
    if error.kind() != io::ErrorKind::Interrupted {
        return Err(error);
    }
    println!(
        "{}",
        serde_json::json!({"root":root,"records":records.len(),"pages":native.pages,"raw_ms":raw_ms,"examples":examples,"trials":trials,"eligible_for_performance":eligible,"cancel_after_first_page":true,"identity":"supplemented by existing API","global_link_count":"supplemented by existing API"})
    );
    Ok(())
}

/// 볼륨 일괄 열거·폴더 범위 추출·추가 조회·집계를 모두 포함한 공개 후보 스캔.
pub fn scan(
    root: &std::path::Path,
    target: &crate::ntfs_native::Target,
    detailed: bool,
    cancelled: impl Fn() -> bool,
) -> io::Result<crate::scanner::ScanResult> {
    crate::output::check(&cancelled)?;
    let mut source = Native::open(target)?;
    let records = collect(&mut || source.page(), &cancelled)?;
    let result = scan_records(root, target, records, detailed, &cancelled)?;
    if crate::ntfs_native::target(std::path::Path::new(&target.root))?.volume != target.volume {
        return Err(invalid());
    }
    crate::output::check(&cancelled)?;
    Ok(result)
}

struct Checked<'a> {
    source: &'a crate::metadata::Named,
    row: &'a Record,
    size: u64,
}
impl crate::metadata::StatSource for Checked<'_> {
    fn stat(&self, path: &std::path::Path) -> io::Result<Option<crate::metadata::Stat>> {
        let Some(stat) = self.source.stat(path)? else {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "후보에 필요한 통합 정보 클래스가 지원되지 않습니다",
            ));
        };
        if stat.file_id as u64 != self.row.reference
            || stat.eof < 0
            || stat.eof as u64 != self.size
            || stat.attributes != self.row.attributes
            || Some((stat.created, stat.modified)) != self.row.times
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "일괄 열거 후 파일 메타데이터가 변경됐습니다",
            ));
        }
        Ok(Some(stat))
    }
}
/// 일괄 레코드와 기존 상세 조회를 조합한 공개 개발용 후보. 제품 기본값에는 미연결.
pub fn scan_records(
    root: &std::path::Path,
    target: &crate::ntfs_native::Target,
    records: Vec<Record>,
    detailed: bool,
    cancelled: impl Fn() -> bool,
) -> io::Result<crate::scanner::ScanResult> {
    let named = crate::metadata::Named::for_root(root)?;
    scan_with_probe(
        root,
        target,
        records,
        detailed,
        cancelled,
        &|path, row, size| {
            let report = crate::metadata::read_combined(
                path,
                &Checked {
                    source: &named,
                    row,
                    size,
                },
                None,
            );
            if let Some(error) = report.issue {
                return Err(error);
            }
            Ok(report.values)
        },
    )
}
/// 필드별 비교. 합계만 일치하는 경우를 동등한 결과로 판정하지 않음.
pub fn differences(
    expected: &crate::scanner::ScanResult,
    observed: &crate::scanner::ScanResult,
) -> Vec<String> {
    use crate::metadata::Extra;
    use std::{collections::BTreeMap, path::PathBuf, time::SystemTime};
    type Row = (
        bool,
        u64,
        u32,
        Option<SystemTime>,
        Option<SystemTime>,
        Extra,
        u64,
        u64,
        (u64, u64, u64, u64, u64),
    );
    fn rows(scan: &crate::scanner::ScanResult) -> BTreeMap<PathBuf, Row> {
        let mut rows = BTreeMap::new();
        for f in &scan.folders {
            let a = &f.allocation;
            rows.insert(
                f.path.clone(),
                (
                    true,
                    f.logical_bytes,
                    f.attributes,
                    f.created,
                    f.modified,
                    Extra::default(),
                    f.file_count,
                    f.folder_count,
                    (a.bytes, a.known, a.not_collected, a.unsupported, a.failed),
                ),
            );
        }
        for f in &scan.files {
            rows.insert(
                scan.folders[f.parent].path.join(&f.name),
                (
                    false,
                    f.logical_bytes,
                    f.attributes,
                    f.created,
                    f.modified,
                    f.extra,
                    0,
                    0,
                    (0, 0, 0, 0, 0),
                ),
            );
        }
        rows
    }
    let left = rows(expected);
    let right = rows(observed);
    let keys: std::collections::BTreeSet<_> = left.keys().chain(right.keys()).collect();
    let mut diff = Vec::new();
    for key in keys {
        if left.get(key) != right.get(key) {
            diff.push(format!(
                "{}: {:?} / {:?}",
                key.display(),
                left.get(key),
                right.get(key)
            ));
        }
    }
    if (
        expected.error_count,
        expected.metadata_error_count,
        expected.skipped_reparse_points,
    ) != (
        observed.error_count,
        observed.metadata_error_count,
        observed.skipped_reparse_points,
    ) {
        diff.push("오류/메타데이터 오류/Reparse 제외 개수 불일치".into());
    }
    let issues = |items: &[crate::scanner::ScanIssue]| {
        let mut rows: Vec<_> = items
            .iter()
            .map(|issue| {
                (
                    issue.path.clone(),
                    format!("{:?}", issue.error.kind()),
                    issue.error.raw_os_error(),
                )
            })
            .collect();
        rows.sort();
        rows
    };
    if issues(&expected.issues) != issues(&observed.issues)
        || issues(&expected.metadata_issues) != issues(&observed.metadata_issues)
    {
        diff.push("오류 경로·종류·OS 코드 불일치".into());
    }
    diff
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Vec<u8> {
        // SDK ABI에 따른 독립 응답: 파일 42, 부모 5, 이름 a, 기본 스트림 13 Byte.
        let mut page = vec![0u8; 176];
        for (offset, value) in [
            (0, 1u32),
            (4, 16),
            (16, 1),
            (40, 40),
            (44, 72),
            (48, 128),
            (52, 32),
            (72, 2),
            (88, 2),
            (92, 0),
            (96, 1),
            (140, 0x80),
        ] {
            page[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
        }
        page[32..40].copy_from_slice(&42u64.to_le_bytes());
        page[64..72].copy_from_slice(&5u64.to_le_bytes());
        page[80..82].copy_from_slice(&97u16.to_le_bytes());
        page[104..112].copy_from_slice(&4096i64.to_le_bytes());
        page[112..120].copy_from_slice(&13i64.to_le_bytes());
        page[124..128].copy_from_slice(&0x80u32.to_le_bytes());
        page
    }
    #[test]
    fn collection_preserves_names_parent_stream_and_times_without_inventing_ids() {
        let page = sample();
        let mut source = [Some(page), None].into_iter();
        let rows = collect(&mut || Ok(source.next().unwrap()), || false).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].reference, 42);
        assert_eq!(rows[0].names[0].parent, 5);
        assert_eq!(rows[0].names[0].name, std::ffi::OsStr::new("a"));
        assert_eq!(rows[0].streams[0].bytes, 13);
        assert_eq!(rows[0].streams[0].allocation, 4096);
    }

    #[test]
    fn collection_accepts_observed_v1_stream_prefix_and_rejects_unknown_version() {
        let mut page = sample();
        page[88..92].copy_from_slice(&1u32.to_le_bytes());
        let mut source = [Some(page.clone()), None].into_iter();
        assert_eq!(
            collect(&mut || Ok(source.next().unwrap()), || false).unwrap()[0].streams[0].bytes,
            13
        );
        page[88..92].copy_from_slice(&3u32.to_le_bytes());
        assert_eq!(
            collect(&mut || Ok(Some(page.clone())), || false)
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn candidate_scan_preserves_reference_fields_and_external_link_count() {
        let root =
            std::env::temp_dir().join(format!("diskweave-layout-candidate-{}", std::process::id()));
        std::fs::create_dir_all(root.join("scope/sub")).unwrap();
        std::fs::write(root.join("scope/a.bin"), b"1234567890123").unwrap();
        std::fs::hard_link(root.join("scope/a.bin"), root.join("outside.bin")).unwrap();
        let target = crate::ntfs_native::target(root.ancestors().last().unwrap()).unwrap();
        let records = fixture_records(&root.join("scope"));
        let expected = crate::scanner::scan_with_strategy(
            &root.join("scope"),
            true,
            crate::scanner::Parallelism::Eight,
            crate::scanner::directory::Strategy::Combined,
            || false,
            |_| {},
            None,
        )
        .unwrap();
        let observed = scan_records(&root.join("scope"), &target, records, true, || false).unwrap();
        assert!(differences(&expected, &observed).is_empty());
        assert_eq!(
            observed.files[0].extra.links,
            crate::metadata::Field::Value(2)
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    fn fixture_records(root: &std::path::Path) -> Vec<Record> {
        use std::os::windows::fs::MetadataExt;
        fn walk(path: &std::path::Path, parent: u64, rows: &mut Vec<Record>) {
            let file = std::fs::File::options()
                .access_mode(0x80)
                .custom_flags(0x2200000)
                .share_mode(7)
                .open(path)
                .unwrap();
            let mut info =
                windows_sys::Win32::Storage::FileSystem::BY_HANDLE_FILE_INFORMATION::default();
            use std::os::windows::{fs::OpenOptionsExt, io::AsRawHandle};
            unsafe {
                assert_ne!(
                    windows_sys::Win32::Storage::FileSystem::GetFileInformationByHandle(
                        file.as_raw_handle(),
                        &mut info
                    ),
                    0
                );
            }
            let id = (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow);
            let m = file.metadata().unwrap();
            rows.push(Record {
                reference: id,
                attributes: m.file_attributes(),
                names: vec![Name {
                    cached: None,
                    parent,
                    flags: 0,
                    name: path.file_name().unwrap().into(),
                }],
                streams: if m.is_file() {
                    vec![Stream {
                        kind: 128,
                        name: OsString::new(),
                        allocation: 999,
                        bytes: m.len(),
                    }]
                } else {
                    vec![]
                },
                times: Some((m.creation_time() as i64, m.last_write_time() as i64)),
            });
            if m.is_dir() && m.file_attributes() & 0x400 == 0 {
                for child in std::fs::read_dir(path).unwrap() {
                    walk(&child.unwrap().path(), id, rows);
                }
            }
        }
        let mut rows = Vec::new();
        walk(root, 0, &mut rows);
        rows
    }

    #[test]
    fn candidate_uses_bounded_parallel_queries_and_joins_workers_on_cancel() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let root =
            std::env::temp_dir().join(format!("diskweave-layout-workers-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        for i in 0..128 {
            std::fs::write(root.join(format!("{i}.bin")), b"123").unwrap();
        }
        let target = crate::ntfs_native::target(root.ancestors().last().unwrap()).unwrap();
        let active = AtomicUsize::new(0);
        let peak = AtomicUsize::new(0);
        let calls = AtomicUsize::new(0);
        let probe = |_: &std::path::Path, _: &Record, _| {
            let n = active.fetch_add(1, Ordering::SeqCst) + 1;
            peak.fetch_max(n, Ordering::SeqCst);
            calls.fetch_add(1, Ordering::SeqCst);
            std::thread::sleep(std::time::Duration::from_millis(2));
            active.fetch_sub(1, Ordering::SeqCst);
            Ok(crate::metadata::Extra::default())
        };
        let result = scan_with_probe(
            &root,
            &target,
            fixture_records(&root),
            true,
            || false,
            &probe,
        )
        .unwrap();
        assert_eq!(result.files.len(), 128);
        assert!((2..=8).contains(&peak.load(Ordering::SeqCst)));
        calls.store(0, Ordering::SeqCst);
        assert_eq!(
            scan_with_probe(
                &root,
                &target,
                fixture_records(&root),
                true,
                || calls.load(Ordering::SeqCst) >= 8,
                &probe
            )
            .err()
            .unwrap()
            .kind(),
            io::ErrorKind::Interrupted
        );
        assert_eq!(active.load(Ordering::SeqCst), 0);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn collection_preserves_multiple_pages_full_reference_and_nonunicode_name() {
        let mut a = sample();
        let mut b = sample();
        a[32..40].copy_from_slice(&0xfedcba9876543210u64.to_le_bytes());
        a[80..82].copy_from_slice(&0xd800u16.to_le_bytes());
        b[32..40].copy_from_slice(&43u64.to_le_bytes());
        let mut source = [Some(a), Some(b), None].into_iter();
        let records = collect(&mut || Ok(source.next().unwrap()), || false).unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].reference, 0xfedcba9876543210);
        assert_eq!(records[0].names[0].name, OsString::from_wide(&[0xd800]));
    }

    #[test]
    fn collection_rejects_duplicates_and_corrupt_lengths_without_partial_success() {
        let mut source = [Some(sample()), Some(sample()), None].into_iter();
        assert_eq!(
            collect(&mut || Ok(source.next().unwrap()), || false)
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
        for (offset, value) in [
            (4, 0u32),
            (4, u32::MAX),
            (20, 1),
            (72, 3),
            (52, u32::MAX),
            (88, 99),
        ] {
            let mut page = sample();
            page[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
            assert!(
                collect(&mut || Ok(Some(page.clone())), || false).is_err(),
                "offset={offset}"
            );
        }
        for size in 0..176 {
            let mut source = [Some(sample()[..size].to_vec()), None].into_iter();
            assert!(
                collect(&mut || Ok(source.next().unwrap()), || false).is_err(),
                "size={size}"
            );
        }
    }

    #[test]
    fn collection_preserves_os_failure_and_interrupts_after_receiving_page() {
        for code in [5, 50] {
            assert_eq!(
                collect(&mut || Err(io::Error::from_raw_os_error(code)), || false)
                    .unwrap_err()
                    .raw_os_error(),
                Some(code)
            );
        }
        let received = std::cell::Cell::new(false);
        assert_eq!(
            collect(
                &mut || {
                    received.set(true);
                    Ok(Some(sample()))
                },
                || received.get()
            )
            .unwrap_err()
            .kind(),
            io::ErrorKind::Interrupted
        );
        assert_eq!(
            collect(&mut || panic!("취소 뒤 조회 금지"), || true)
                .unwrap_err()
                .kind(),
            io::ErrorKind::Interrupted
        );
    }

    #[test]
    fn comparison_detects_timestamp_id_link_and_missing_allocation_changes() {
        let root =
            std::env::temp_dir().join(format!("diskweave-layout-diff-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("a"), b"123").unwrap();
        let reference = crate::scanner::scan_with_options(&root, true, || false, |_| {}).unwrap();
        for field in 0..4 {
            let mut observed =
                crate::scanner::scan_with_options(&root, true, || false, |_| {}).unwrap();
            match field {
                0 => observed.files[0].modified = Some(std::time::UNIX_EPOCH),
                1 => {
                    observed.files[0].extra.identity =
                        crate::metadata::Field::Missing(crate::metadata::Missing::Unsupported)
                }
                2 => observed.files[0].extra.links = crate::metadata::Field::Value(999),
                _ => {
                    observed.files[0].extra.allocation =
                        crate::metadata::Field::Missing(crate::metadata::Missing::Failed)
                }
            }
            assert_eq!(differences(&reference, &observed).len(), 1);
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn candidate_failure_joins_workers_and_never_returns_partial_result() {
        let root =
            std::env::temp_dir().join(format!("diskweave-layout-failure-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("a"), b"123").unwrap();
        let target = crate::ntfs_native::target(root.ancestors().last().unwrap()).unwrap();
        for panic in [false, true] {
            let error = scan_with_probe(
                &root,
                &target,
                fixture_records(&root),
                true,
                || false,
                &|_, _, _| {
                    if panic {
                        panic!("시험 OS 경계 실패");
                    }
                    Err(io::Error::from_raw_os_error(5))
                },
            )
            .err()
            .unwrap();
            assert_eq!(
                error.kind(),
                if panic {
                    io::ErrorKind::Other
                } else {
                    io::ErrorKind::PermissionDenied
                }
            );
        }
        let mut rows = fixture_records(&root);
        rows.iter_mut()
            .find(|r| r.attributes & 0x10 == 0)
            .unwrap()
            .streams[0]
            .bytes = 0;
        assert_eq!(
            scan_records(&root, &target, rows, true, || false)
                .err()
                .unwrap()
                .kind(),
            io::ErrorKind::InvalidData
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn candidate_rejects_unresolved_parent_instead_of_silently_losing_file() {
        let root =
            std::env::temp_dir().join(format!("diskweave-layout-parent-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("a"), b"123").unwrap();
        let target = crate::ntfs_native::target(root.ancestors().last().unwrap()).unwrap();
        let mut records = fixture_records(&root);
        records
            .iter_mut()
            .find(|r| r.attributes & 0x10 == 0)
            .unwrap()
            .names[0]
            .parent = u64::MAX;
        assert_eq!(
            scan_records(&root, &target, records, false, || false)
                .err()
                .unwrap()
                .kind(),
            io::ErrorKind::InvalidData
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn candidate_preserves_real_long_and_nonunicode_paths() {
        let root =
            std::env::temp_dir().join(format!("diskweave-layout-long-{}", std::process::id()));
        let mut nested = root.clone();
        for _ in 0..4 {
            nested.push("긴경로_".to_owned() + &"x".repeat(70));
        }
        std::fs::create_dir_all(&nested).unwrap();
        let mut name = OsString::from_wide(&[0xd800]);
        name.push(".bin");
        std::fs::write(nested.join(name), b"1234567").unwrap();
        let target = crate::ntfs_native::target(root.ancestors().last().unwrap()).unwrap();
        let reference = crate::scanner::scan_with_options(&root, true, || false, |_| {}).unwrap();
        let candidate =
            scan_records(&root, &target, fixture_records(&root), true, || false).unwrap();
        assert!(differences(&reference, &candidate).is_empty());
        assert_eq!(candidate.folders[0].logical_bytes, 7);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn comparison_does_not_hide_different_errors_with_same_count() {
        let root =
            std::env::temp_dir().join(format!("diskweave-layout-errors-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let mut expected = crate::scanner::scan(&root, || false).unwrap();
        let mut observed = crate::scanner::scan(&root, || false).unwrap();
        expected.record_error(root.join("a"), io::Error::from_raw_os_error(5));
        observed.record_error(root.join("a"), io::Error::from_raw_os_error(2));
        assert!(!differences(&expected, &observed).is_empty());
        std::fs::remove_dir_all(root).unwrap();
    }
}
