//! 공개 NTFS 형식의 읽기 전용 수집·검증·결과 조립.

use std::{ffi::OsString, io, os::windows::ffi::OsStringExt};
#[derive(Clone, Copy, Default)]
pub struct Tuning {
    pub in_place: bool,
    pub compact: bool,
    pub move_names: bool,
}
#[derive(Clone, Copy, Default)]
pub struct Options<'a> {
    pub tuning: Tuning,
    pub metrics: Option<&'a crate::scanner::ScanMetrics>,
}
#[derive(Clone, Copy)]
pub enum Framing {
    OnDisk,
    Ioctl,
}
#[derive(Clone, Debug)]
pub struct Name {
    pub parent: u64,
    pub name: OsString,
    pub namespace: u8,
    pub times: (i64, i64),
    pub bytes: u64,
    pub attributes: u32,
}
#[derive(Clone, Debug)]
pub struct Data {
    pub bytes: u64,
    pub allocation: u64,
    pub lowest: u64,
    pub runs: Vec<Run>,
}
#[derive(Clone, Debug)]
pub struct Run {
    pub vcn: u64,
    pub lcn: Option<u64>,
    pub clusters: u64,
}
#[derive(Debug)]
pub struct Parsed {
    pub reference: u64,
    pub base: u64,
    pub links: u16,
    pub directory: bool,
    pub attributes: Option<u32>,
    pub times: Option<(i64, i64)>,
    pub names: Vec<Name>,
    pub data: Option<Data>,
    pub external_list: Option<Data>,
    pub references: Vec<u64>,
}
fn bad() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "MFT 레코드·속성·fixup 범위 오류",
    )
}
fn span(b: &[u8], o: usize, n: usize) -> io::Result<&[u8]> {
    b.get(o..o.checked_add(n).ok_or_else(bad)?).ok_or_else(bad)
}
fn w(b: &[u8], o: usize) -> io::Result<u16> {
    Ok(u16::from_le_bytes(span(b, o, 2)?.try_into().unwrap()))
}
fn d(b: &[u8], o: usize) -> io::Result<u32> {
    Ok(u32::from_le_bytes(span(b, o, 4)?.try_into().unwrap()))
}
fn q(b: &[u8], o: usize) -> io::Result<u64> {
    Ok(u64::from_le_bytes(span(b, o, 8)?.try_into().unwrap()))
}
fn nonnegative(v: u64) -> io::Result<u64> {
    if v > i64::MAX as u64 {
        Err(bad())
    } else {
        Ok(v)
    }
}
fn string(b: &[u8], o: usize, n: usize) -> io::Result<OsString> {
    let bytes = span(b, o, n.checked_mul(2).ok_or_else(bad)?)?;
    let units: Vec<_> = bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|v| u16::from_le_bytes(*v))
        .collect();
    if units.contains(&0) {
        return Err(bad());
    }
    Ok(OsString::from_wide(&units))
}
fn runs(attr: &[u8]) -> io::Result<Vec<Run>> {
    let mut offset = w(attr, 32)? as usize;
    let mut vcn = q(attr, 16)?;
    let highest = q(attr, 24)?;
    let mut lcn = 0i64;
    let mut output = Vec::new();
    if offset < 64 {
        return Err(bad());
    }
    loop {
        let code = *span(attr, offset, 1)?.first().unwrap();
        offset += 1;
        if code == 0 {
            break;
        }
        let length = (code & 15) as usize;
        let delta = (code >> 4) as usize;
        if !(1..=8).contains(&length) || delta > 8 {
            return Err(bad());
        }
        let mut count = [0u8; 8];
        count[..length].copy_from_slice(span(attr, offset, length)?);
        offset += length;
        let clusters = u64::from_le_bytes(count);
        if clusters == 0 {
            return Err(bad());
        }
        let location = if delta == 0 {
            None
        } else {
            let raw = span(attr, offset, delta)?;
            let mut value = [if raw[delta - 1] & 0x80 != 0 { 255 } else { 0 }; 8];
            value[..delta].copy_from_slice(raw);
            offset += delta;
            lcn = lcn.checked_add(i64::from_le_bytes(value)).ok_or_else(bad)?;
            Some(u64::try_from(lcn).map_err(|_| bad())?)
        };
        output.push(Run {
            vcn,
            lcn: location,
            clusters,
        });
        vcn = vcn.checked_add(clusters).ok_or_else(bad)?;
    }
    if highest.checked_add(1) != Some(vcn)
        && !(highest == u64::MAX
            && vcn == 0
            && output.is_empty()
            && q(attr, 40)? == 0
            && q(attr, 48)? == 0)
    {
        return Err(bad());
    }
    Ok(output)
}
fn list_references(value: &[u8]) -> io::Result<Vec<u64>> {
    let mut refs = Vec::new();
    let mut pos = 0;
    while pos < value.len() {
        if d(value, pos)? == u32::MAX {
            break;
        }
        let length = w(value, pos + 4)? as usize;
        if length < 26 {
            return Err(bad());
        }
        let item = span(value, pos, length)?;
        if item[6] != 0 {
            string(item, item[7] as usize, item[6] as usize)?;
        }
        refs.push(q(item, 16)?);
        pos = pos.checked_add(length).ok_or_else(bad)?;
    }
    Ok(refs)
}
/// 단일 FILE 레코드 해석. 디스크 전송의 USA 검증과 속성 경계 확인 후에만 값 반환.
pub fn decode(
    number: u64,
    input: &[u8],
    sector: usize,
    framing: Framing,
) -> io::Result<Option<Parsed>> {
    let Some(flags) = active_header(input, sector)? else {
        return Ok(None);
    };
    decode_checked(number, &mut input.to_vec(), sector, framing, flags)
}
/// 검증한 입력 버퍼에만 USA 복원을 적용하며 레코드 전체 복사 생략.
pub fn decode_in_place(
    number: u64,
    record: &mut [u8],
    sector: usize,
    framing: Framing,
) -> io::Result<Option<Parsed>> {
    let Some(flags) = active_header(record, sector)? else {
        return Ok(None);
    };
    decode_checked(number, record, sector, framing, flags)
}
fn active_header(input: &[u8], sector: usize) -> io::Result<Option<u16>> {
    if input.iter().all(|b| *b == 0) {
        return Ok(None);
    }
    if input.len() < 48
        || input.len() > 65536
        || &input[..4] != b"FILE"
        || sector < 512
        || !sector.is_power_of_two()
        || !input.len().is_multiple_of(sector)
    {
        return Err(bad());
    }
    let flags = w(input, 22)?;
    Ok((flags & 1 != 0).then_some(flags))
}
fn decode_checked(
    number: u64,
    record: &mut [u8],
    sector: usize,
    framing: Framing,
    flags: u16,
) -> io::Result<Option<Parsed>> {
    let usa = w(record, 4)? as usize;
    let count = w(record, 6)? as usize;
    if count != record.len() / sector + 1 || usa < 42 || count > 129 {
        return Err(bad());
    }
    span(record, usa, count.checked_mul(2).ok_or_else(bad)?)?;
    let token = w(record, usa)?;
    // USA 배열과 섹터 꼬리가 겹친 손상 입력에서도 검증 전 원본 대체 값 유지.
    let mut replacements = [0u16; 129];
    for (i, replacement) in replacements.iter_mut().enumerate().take(count).skip(1) {
        *replacement = w(record, usa + i * 2)?;
        let observed = w(record, i * sector - 2)?;
        if observed != token && !(matches!(framing, Framing::Ioctl) && observed == *replacement) {
            return Err(bad());
        }
    }
    for (i, replacement) in replacements.iter().enumerate().take(count).skip(1) {
        record[i * sector - 2..i * sector].copy_from_slice(&replacement.to_le_bytes());
    }
    let used = d(record, 24)? as usize;
    let allocated = d(record, 28)? as usize;
    let mut offset = w(record, 20)? as usize;
    if allocated != record.len()
        || used > allocated
        || offset < usa + count * 2
        || !offset.is_multiple_of(8)
        || number > 0x0000ffffffffffff
    {
        return Err(bad());
    }
    let record = span(record, 0, used)?;
    let mut parsed = Parsed {
        reference: number | (u64::from(w(record, 16)?) << 48),
        base: q(record, 32)?,
        links: w(record, 18)?,
        directory: flags & 2 != 0,
        attributes: None,
        times: None,
        names: vec![],
        data: None,
        external_list: None,
        references: vec![],
    };
    loop {
        let kind = d(record, offset)?;
        if kind == u32::MAX {
            break;
        }
        let size = d(record, offset + 4)? as usize;
        if size < 24 || !size.is_multiple_of(8) {
            return Err(bad());
        }
        let attr = span(record, offset, size)?;
        let named = attr[9] != 0;
        if named {
            string(attr, w(attr, 10)? as usize, attr[9] as usize)?;
        }
        if matches!(kind, 0x10 | 0x20 | 0x30 | 0x80) && !named {
            if attr[8] == 1 {
                if !matches!(kind, 0x20 | 0x80) {
                    return Err(bad());
                }
                let lowest = q(attr, 16)?;
                let allocation = if lowest != 0 {
                    0
                } else if w(attr, 12)? & 0x80ff != 0 {
                    q(attr, 64)?
                } else {
                    q(attr, 40)?
                };
                let data = Data {
                    bytes: if lowest == 0 {
                        nonnegative(q(attr, 48)?)?
                    } else {
                        0
                    },
                    allocation: nonnegative(allocation)?,
                    lowest,
                    runs: runs(attr)?,
                };
                if (if kind == 0x80 {
                    &mut parsed.data
                } else {
                    &mut parsed.external_list
                })
                .replace(data)
                .is_some()
                {
                    return Err(bad());
                }
                offset = offset.checked_add(size).ok_or_else(bad)?;
                continue;
            } else if attr[8] != 0 {
                return Err(bad());
            }
            let value_offset = w(attr, 20)? as usize;
            if value_offset < 24 {
                return Err(bad());
            }
            let value = span(attr, value_offset, d(attr, 16)? as usize)?;
            match kind {
                0x10 => {
                    if parsed.attributes.is_some() {
                        return Err(bad());
                    }
                    parsed.attributes = Some(d(value, 32)?);
                    parsed.times = Some((q(value, 0)? as i64, q(value, 8)? as i64));
                }
                0x30 => {
                    span(value, 0, 66)?;
                    if value[65] > 3 {
                        return Err(bad());
                    }
                    parsed.names.push(Name {
                        parent: q(value, 0)?,
                        name: string(value, 66, value[64] as usize)?,
                        namespace: value[65],
                        times: (q(value, 8)? as i64, q(value, 16)? as i64),
                        bytes: q(value, 48)?,
                        attributes: d(value, 56)?,
                    });
                }
                0x80 => {
                    if parsed.data.is_some() {
                        return Err(bad());
                    }
                    parsed.data = Some(Data {
                        bytes: value.len() as u64,
                        allocation: ((value.len() + 7) & !7) as u64,
                        lowest: 0,
                        runs: vec![],
                    });
                }
                _ => parsed.references.extend(list_references(value)?),
            }
        }
        offset = offset.checked_add(size).ok_or_else(bad)?;
    }
    Ok(Some(parsed))
}

/// 확장 속성을 실제 base reference/sequence와 연결하고 누락 참조 거부.
pub fn merge(parts: Vec<Parsed>) -> io::Result<Vec<Parsed>> {
    use std::collections::HashMap;
    let mut owner = HashMap::new();
    for part in &parts {
        if owner.insert(part.reference, part.base).is_some() {
            return Err(bad());
        }
    }
    for part in &parts {
        let base = if part.base == 0 {
            part.reference
        } else {
            part.base
        };
        for reference in &part.references {
            let Some(&target) = owner.get(reference) else {
                return Err(bad());
            };
            if *reference != base && target != base {
                return Err(bad());
            }
        }
    }
    let mut bases = HashMap::new();
    let mut extensions = Vec::new();
    for part in parts {
        if part.base == 0 {
            bases.insert(part.reference, part);
        } else {
            extensions.push(part);
        }
    }
    for mut part in extensions {
        let base = bases.get_mut(&part.base).ok_or_else(bad)?;
        if let Some(attributes) = part.attributes {
            if base.attributes.replace(attributes).is_some() {
                return Err(bad());
            }
            base.times = part.times;
        }
        base.names.append(&mut part.names);
        if let Some(mut data) = part.data {
            if let Some(existing) = &mut base.data {
                if existing.lowest == 0 && data.lowest == 0 {
                    return Err(bad());
                }
                if data.lowest == 0 {
                    data.runs.append(&mut existing.runs);
                    *existing = data;
                } else {
                    existing.runs.append(&mut data.runs);
                }
            } else {
                base.data = Some(data);
            }
        }
    }
    let mut result: Vec<_> = bases.into_values().collect();
    for base in &mut result {
        if let Some(data) = &mut base.data {
            if data.lowest != 0 {
                return Err(bad());
            }
            data.runs.sort_unstable_by_key(|r| r.vcn);
            let mut next = 0;
            for run in &data.runs {
                if run.vcn != next {
                    return Err(bad());
                }
                next = next.checked_add(run.clusters).ok_or_else(bad)?;
            }
        }
    }
    result.sort_unstable_by_key(|p| p.reference);
    Ok(result)
}

#[derive(Clone, Copy, Debug)]
pub struct Geometry {
    pub sector: usize,
    pub cluster: u64,
    pub record: usize,
    pub valid: u64,
    pub clusters: u64,
    pub mft_lcn: u64,
    pub volume: u64,
}
pub trait Source {
    fn geometry(&mut self) -> io::Result<Geometry>;
    fn record(&mut self, number: u64) -> io::Result<Vec<u8>>;
    fn read_at(&mut self, offset: u64, bytes: &mut [u8]) -> io::Result<()>;
}
#[derive(Clone, Copy)]
struct Payload {
    bytes: u64,
    allocation: u64,
}
struct Stored {
    reference: u64,
    links: u16,
    directory: bool,
    attributes: Option<u32>,
    times: Option<(i64, i64)>,
    names: Vec<Name>,
    data: Option<Payload>,
}
#[derive(Clone, Copy)]
struct RecordView<'a> {
    reference: u64,
    links: u16,
    directory: bool,
    attributes: Option<u32>,
    times: Option<(i64, i64)>,
    names: &'a [Name],
    data: Option<Payload>,
}
impl Parsed {
    fn view(&self) -> RecordView<'_> {
        RecordView {
            reference: self.reference,
            links: self.links,
            directory: self.directory,
            attributes: self.attributes,
            times: self.times,
            names: &self.names,
            data: self.data.as_ref().map(|d| Payload {
                bytes: d.bytes,
                allocation: d.allocation,
            }),
        }
    }
}
impl Stored {
    fn from_parsed(p: Parsed) -> Self {
        Self {
            reference: p.reference,
            links: p.links,
            directory: p.directory,
            attributes: p.attributes,
            times: p.times,
            names: p.names,
            data: p.data.map(|d| Payload {
                bytes: d.bytes,
                allocation: d.allocation,
            }),
        }
    }
    fn view(&self) -> RecordView<'_> {
        RecordView {
            reference: self.reference,
            links: self.links,
            directory: self.directory,
            attributes: self.attributes,
            times: self.times,
            names: &self.names,
            data: self.data,
        }
    }
}
pub struct Catalog {
    small: Option<Vec<Stored>>,
    pub records: Vec<Parsed>,
    pub geometry: Geometry,
    pub bytes: u64,
    pub reads: u64,
    pub ioctl_records: u64,
    pub supplemental: std::sync::atomic::AtomicU64,
    pub metadata_queries: std::sync::atomic::AtomicU64,
    pub decoded_in_place: u64,
    pub decoded_copies: u64,
    pub moved_names: std::sync::atomic::AtomicU64,
}
impl Catalog {
    pub fn record_count(&self) -> usize {
        self.records.len() + self.small.as_ref().map_or(0, Vec::len)
    }
    pub fn compacted_records(&self) -> usize {
        self.small.as_ref().map_or(0, Vec::len)
    }
    fn at(&self, index: usize) -> Option<RecordView<'_>> {
        if index < self.records.len() {
            self.records.get(index).map(Parsed::view)
        } else {
            self.small
                .as_ref()?
                .get(index - self.records.len())
                .map(Stored::view)
        }
    }
    fn entries(&self) -> impl Iterator<Item = RecordView<'_>> {
        self.records.iter().map(Parsed::view).chain(
            self.small
                .as_deref()
                .unwrap_or(&[])
                .iter()
                .map(Stored::view),
        )
    }
}
/// 병합·참조·VCN 검증이 끝난 레코드만 표시·집계에 필요한 형태로 축소.
pub fn compact_catalog(mut catalog: Catalog, cancelled: impl Fn() -> bool) -> io::Result<Catalog> {
    crate::output::check(&cancelled)?;
    if catalog.small.is_some() {
        return Ok(catalog);
    }
    let parts = std::mem::take(&mut catalog.records);
    let mut small = Vec::with_capacity(parts.len());
    for part in parts {
        crate::output::check(&cancelled)?;
        small.push(Stored::from_parsed(part));
    }
    catalog.small = Some(small);
    Ok(catalog)
}
struct Aligned {
    pointer: std::ptr::NonNull<u8>,
    layout: std::alloc::Layout,
}
impl Aligned {
    fn new(size: usize, alignment: usize) -> io::Result<Self> {
        let layout = std::alloc::Layout::from_size_align(size, alignment).map_err(|_| bad())?;
        // 원시 볼륨 읽기를 위한 명시적 섹터 정렬. Drop에서 동일 Layout으로 해제.
        let pointer = std::ptr::NonNull::new(unsafe { std::alloc::alloc_zeroed(layout) })
            .ok_or_else(|| io::Error::other("정렬 MFT 버퍼 할당 실패"))?;
        Ok(Self { pointer, layout })
    }
    fn bytes(&mut self) -> &mut [u8] {
        unsafe { std::slice::from_raw_parts_mut(self.pointer.as_ptr(), self.layout.size()) }
    }
}
impl Drop for Aligned {
    fn drop(&mut self) {
        unsafe {
            std::alloc::dealloc(self.pointer.as_ptr(), self.layout);
        }
    }
}

fn read_external_list(
    source: &mut impl Source,
    g: &Geometry,
    data: &Data,
    cancel: &impl Fn() -> bool,
) -> io::Result<Vec<u64>> {
    if data.lowest != 0 || data.bytes > 16 * 1024 * 1024 {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "시험 MFT 속성 목록 범위 초과",
        ));
    }
    if data.bytes == 0 {
        return Ok(vec![]);
    }
    let rounded = data
        .bytes
        .div_ceil(g.sector as u64)
        .checked_mul(g.sector as u64)
        .ok_or_else(bad)?;
    let mut next = 0;
    for run in &data.runs {
        if run.vcn != next
            || run.lcn.is_none_or(|lcn| {
                lcn.checked_add(run.clusters)
                    .is_none_or(|end| end > g.clusters)
            })
        {
            return Err(bad());
        }
        next = next.checked_add(run.clusters).ok_or_else(bad)?;
    }
    if next.checked_mul(g.cluster).is_none_or(|end| end < rounded) {
        return Err(bad());
    }
    let mut buffer = Aligned::new(rounded as usize, 4096usize.max(g.sector))?;
    let mut pos = 0;
    while pos < rounded {
        crate::output::check(cancel)?;
        let run = data
            .runs
            .iter()
            .find(|r| pos / g.cluster >= r.vcn && pos / g.cluster < r.vcn + r.clusters)
            .ok_or_else(bad)?;
        let inside = pos - run.vcn * g.cluster;
        let size = (run.clusters * g.cluster - inside).min(rounded - pos) as usize;
        let physical = run
            .lcn
            .unwrap()
            .checked_mul(g.cluster)
            .and_then(|v| v.checked_add(inside))
            .ok_or_else(bad)?;
        source.read_at(
            physical,
            &mut buffer.bytes()[pos as usize..pos as usize + size],
        )?;
        pos += size as u64;
    }
    list_references(&buffer.bytes()[..data.bytes as usize])
}

/// 검증한 $MFT run 안에서만 읽기. 물리적 연속 배치를 가정하지 않고 논리 블록 조립.
pub fn collect(source: &mut impl Source, cancelled: impl Fn() -> bool) -> io::Result<Catalog> {
    collect_tuned(source, Options::default(), cancelled, |_| {})
}
pub fn collect_tuned(
    source: &mut impl Source,
    options: Options<'_>,
    cancelled: impl Fn() -> bool,
    mut progress: impl FnMut(crate::scanner::ScanProgress),
) -> io::Result<Catalog> {
    use crate::scanner::profile::{scope, Stage};
    let trace = options.metrics.map(|m| &m.trace);
    let read_phase = scope(trace, Stage::MftRead);
    use std::collections::HashSet;
    crate::output::check(&cancelled)?;
    let geometry = source.geometry()?;
    if geometry.sector < 512
        || !geometry.sector.is_power_of_two()
        || !geometry.cluster.is_power_of_two()
        || geometry.cluster < geometry.sector as u64
        || !geometry.record.is_power_of_two()
        || !(512..=65536).contains(&geometry.record)
        || !geometry.record.is_multiple_of(geometry.sector)
        || geometry.valid == 0
        || !geometry.valid.is_multiple_of(geometry.record as u64)
    {
        return Err(bad());
    }
    let first = source.record(0)?;
    let mut first = decode(0, &first, geometry.sector, Framing::Ioctl)
        .map_err(|e| io::Error::new(e.kind(), format!("MFT bootstrap: {e}")))?
        .ok_or_else(bad)?;
    if let Some(list) = &first.external_list {
        first
            .references
            .extend(read_external_list(source, &geometry, list, &cancelled)?);
    }
    let root_id = first.reference;
    let mut references = first.references.clone();
    let mut bootstrap = vec![first];
    let mut seen = HashSet::from([root_id]);
    let mut controls = 1;
    while let Some(reference) = references.pop() {
        crate::output::check(&cancelled)?;
        if !seen.insert(reference) {
            continue;
        }
        let data = source.record(reference & 0x0000ffffffffffff)?;
        let mut part = decode(
            reference & 0x0000ffffffffffff,
            &data,
            geometry.sector,
            Framing::Ioctl,
        )?
        .ok_or_else(bad)?;
        if part.reference != reference || part.base != root_id {
            return Err(bad());
        }
        if let Some(list) = &part.external_list {
            part.references
                .extend(read_external_list(source, &geometry, list, &cancelled)?);
        }
        references.extend(&part.references);
        bootstrap.push(part);
        controls += 1;
    }
    let mut merged = merge(bootstrap)?;
    if merged.len() != 1 {
        return Err(bad());
    }
    let data = merged.pop().unwrap().data.ok_or_else(bad)?;
    if data.bytes < geometry.valid
        || data
            .runs
            .first()
            .is_none_or(|r| r.vcn != 0 || r.lcn != Some(geometry.mft_lcn))
    {
        return Err(bad());
    }
    let mut coverage = 0;
    for run in &data.runs {
        let lcn = run.lcn.ok_or_else(bad)?;
        if run.vcn != coverage
            || lcn
                .checked_add(run.clusters)
                .is_none_or(|end| end > geometry.clusters)
        {
            return Err(bad());
        }
        coverage = coverage.checked_add(run.clusters).ok_or_else(bad)?;
    }
    if coverage
        .checked_mul(geometry.cluster)
        .is_none_or(|end| end < geometry.valid)
    {
        return Err(bad());
    }
    let mut buffer = Aligned::new(4 * 1024 * 1024, 4096usize.max(geometry.sector))?;
    let mut logical = 0;
    let mut parts = Vec::new();
    let mut reads = 0;
    let mut decoded_in_place = 0;
    let mut decoded_copies = 0;
    while logical < geometry.valid {
        crate::output::check(&cancelled)?;
        let size = ((geometry.valid - logical).min(4 * 1024 * 1024)) as usize;
        let chunk = &mut buffer.bytes()[..size];
        let mut filled = 0;
        while filled < size {
            crate::output::check(&cancelled)?;
            let position = logical + filled as u64;
            let run = data
                .runs
                .iter()
                .find(|r| {
                    position / geometry.cluster >= r.vcn
                        && position / geometry.cluster < r.vcn + r.clusters
                })
                .ok_or_else(bad)?;
            let inside = position
                .checked_sub(run.vcn.checked_mul(geometry.cluster).ok_or_else(bad)?)
                .ok_or_else(bad)?;
            let take = (run.clusters.checked_mul(geometry.cluster).ok_or_else(bad)? - inside)
                .min((size - filled) as u64) as usize;
            let disk = run
                .lcn
                .unwrap()
                .checked_mul(geometry.cluster)
                .and_then(|v| v.checked_add(inside))
                .ok_or_else(bad)?;
            source.read_at(disk, &mut chunk[filled..filled + take])?;
            reads += 1;
            filled += take;
        }
        for (index, record) in chunk.chunks_exact_mut(geometry.record).enumerate() {
            crate::output::check(&cancelled)?;
            let number = logical / geometry.record as u64 + index as u64;
            if let Some(part) = (if options.tuning.in_place {
                decode_in_place(number, record, geometry.sector, Framing::OnDisk)
            } else {
                decode(number, record, geometry.sector, Framing::OnDisk)
            })
            .map_err(|e| io::Error::new(e.kind(), format!("MFT record {number}: {e}")))?
            {
                if options.tuning.in_place {
                    decoded_in_place += 1;
                } else {
                    decoded_copies += 1;
                }
                parts.push(part);
            }
        }
        logical += size as u64;
        progress(crate::scanner::ScanProgress {
            files_scanned: parts.len() as u64,
            folders_scanned: 0,
        });
    }
    drop(read_phase);
    let records = {
        let _phase = scope(trace, Stage::MftMerge);
        merge(parts)?
    };
    crate::output::check(&cancelled)?;
    let catalog = Catalog {
        records,
        geometry,
        bytes: logical,
        reads,
        ioctl_records: controls,
        supplemental: Default::default(),
        metadata_queries: Default::default(),
        decoded_in_place,
        decoded_copies,
        moved_names: Default::default(),
        small: None,
    };
    if options.tuning.compact {
        let _phase = scope(trace, Stage::MftCompact);
        compact_catalog(catalog, &cancelled)
    } else {
        Ok(catalog)
    }
}

pub struct Native {
    volume: std::fs::File,
    geometry: Option<Geometry>,
}
impl Native {
    pub fn open(target: &crate::ntfs_native::Target) -> io::Result<Self> {
        use std::os::windows::io::FromRawHandle;
        use windows_sys::Win32::{Foundation::*, Storage::FileSystem::*};
        if crate::ntfs_native::target(std::path::Path::new(&target.root))?.volume != target.volume {
            return Err(bad());
        }
        let device: Vec<_> = target
            .volume
            .trim_end_matches('\\')
            .encode_utf16()
            .chain(Some(0))
            .collect();
        // 변경 권한·볼륨 잠금 없이 메타데이터 제어 코드와 검증한 MFT extent만 읽기.
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
            geometry: None,
        })
    }
    fn control(&self, code: u32, input: &[u8], size: usize) -> io::Result<Vec<u8>> {
        use std::os::windows::io::AsRawHandle;
        let mut buffer = vec![0u64; size.div_ceil(8)];
        let mut bytes = 0;
        // 호출자가 고정한 읽기 전용 코드·정렬 버퍼와 유효 반환 길이에 한정.
        let ok = unsafe {
            windows_sys::Win32::System::IO::DeviceIoControl(
                self.volume.as_raw_handle(),
                code,
                if input.is_empty() {
                    std::ptr::null()
                } else {
                    input.as_ptr().cast()
                },
                input.len() as u32,
                buffer.as_mut_ptr().cast(),
                (buffer.len() * 8) as u32,
                &mut bytes,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        if bytes as usize > buffer.len() * 8 {
            return Err(bad());
        }
        Ok(
            unsafe { std::slice::from_raw_parts(buffer.as_ptr().cast::<u8>(), bytes as usize) }
                .to_vec(),
        )
    }
}
impl Source for Native {
    fn geometry(&mut self) -> io::Result<Geometry> {
        if let Some(value) = self.geometry {
            return Ok(value);
        }
        let b = self.control(
            windows_sys::Win32::System::Ioctl::FSCTL_GET_NTFS_VOLUME_DATA,
            &[],
            512,
        )?;
        if w(&b, 100)? != 3 || w(&b, 102)? != 1 {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "시험 MFT 파서는 NTFS 3.1에서만 활성화됩니다",
            ));
        }
        let value = Geometry {
            volume: q(&b, 0)?,
            clusters: nonnegative(q(&b, 16)?)?,
            sector: d(&b, 40)? as usize,
            cluster: u64::from(d(&b, 44)?),
            record: d(&b, 48)? as usize,
            valid: nonnegative(q(&b, 56)?)?,
            mft_lcn: nonnegative(q(&b, 64)?)?,
        };
        self.geometry = Some(value);
        Ok(value)
    }
    fn record(&mut self, number: u64) -> io::Result<Vec<u8>> {
        let g = self.geometry()?;
        let b = self.control(
            windows_sys::Win32::System::Ioctl::FSCTL_GET_NTFS_FILE_RECORD,
            &number.to_le_bytes(),
            g.record + 32,
        )?;
        if q(&b, 0)? & 0x0000ffffffffffff != number || d(&b, 8)? as usize != g.record {
            return Err(bad());
        }
        Ok(span(&b, 12, g.record)?.to_vec())
    }
    fn read_at(&mut self, offset: u64, bytes: &mut [u8]) -> io::Result<()> {
        use std::io::{Read, Seek, SeekFrom};
        let g = self.geometry()?;
        if !offset.is_multiple_of(g.sector as u64)
            || !bytes.len().is_multiple_of(g.sector)
            || offset.checked_add(bytes.len() as u64).is_none_or(|end| {
                g.clusters
                    .checked_mul(g.cluster)
                    .is_none_or(|limit| end > limit)
            })
        {
            return Err(bad());
        }
        self.volume.seek(SeekFrom::Start(offset))?;
        self.volume.read_exact(bytes)
    }
}

fn file_extra(record: &Parsed, volume: u64) -> Option<crate::metadata::Extra> {
    file_extra_view(record.view(), volume)
}
fn file_extra_view(record: RecordView<'_>, volume: u64) -> Option<crate::metadata::Extra> {
    use crate::metadata::{Extra, Field, Identity, Missing};
    let data = record.data.as_ref()?;
    let mut id = [0; 16];
    id[..8].copy_from_slice(&record.reference.to_le_bytes());
    Some(Extra {
        allocation: Field::Value(data.allocation),
        identity: Field::Value(Identity { volume, file: id }),
        links: if usize::from(record.links) == record.names.len() {
            Field::Value(record.names.iter().filter(|n| n.namespace != 2).count() as u32)
        } else {
            Field::Missing(Missing::Failed)
        },
    })
}

/// 현재 병렬 열거의 이름/크기/시각을 유지하고 일치하는 MFT 상세 정보만 재사용.
pub fn scan_bulk(
    root: &std::path::Path,
    catalog: &Catalog,
    detailed: bool,
    cancelled: impl Fn() -> bool,
) -> io::Result<(crate::scanner::ScanResult, serde_json::Value)> {
    scan_bulk_observed(root, catalog, detailed, cancelled, |_| {}, None)
}
pub fn scan_bulk_observed<'a>(
    root: &std::path::Path,
    catalog: &Catalog,
    detailed: bool,
    cancelled: impl Fn() -> bool,
    progress: impl FnMut(crate::scanner::ScanProgress),
    observer: Option<&'a mut crate::scanner::preview::Observer<'a>>,
) -> io::Result<(crate::scanner::ScanResult, serde_json::Value)> {
    use crate::{
        metadata::{self, StatSource},
        scanner::{self, MetadataProbe, MetadataRequest},
    };
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    crate::output::check(&cancelled)?;
    let source = metadata::Named::for_root(root)?;
    let before = source
        .stat(root)?
        .ok_or_else(|| io::Error::new(io::ErrorKind::Unsupported, "MFT 대조용 루트 ID 미지원"))?;
    let mut id = [0; 16];
    id[..8].copy_from_slice(&before.file_id.to_le_bytes());
    if before.volume != catalog.geometry.volume
        || before.id != id
        || !catalog
            .entries()
            .any(|r| r.reference == before.file_id as u64 && r.directory)
    {
        return Err(bad());
    }
    struct Probe<'a> {
        records: std::collections::HashMap<u64, usize>,
        catalog: &'a Catalog,
        volume: u64,
        source: &'a metadata::Named,
        hits: AtomicU64,
        fallbacks: AtomicU64,
    }
    impl MetadataProbe for Probe<'_> {
        fn read_batch(
            &self,
            requests: &[MetadataRequest],
            stop: &AtomicBool,
        ) -> Vec<metadata::Report> {
            requests
                .iter()
                .take_while(|_| !stop.load(Ordering::Relaxed))
                .map(|request| {
                    let value = request
                        .identity
                        .filter(|id| {
                            id.volume == self.volume && id.file[8..].iter().all(|v| *v == 0)
                        })
                        .and_then(|id| {
                            self.records
                                .get(&u64::from_le_bytes(id.file[..8].try_into().unwrap()))
                                .and_then(|id| self.catalog.at(*id))
                        })
                        .filter(|r| {
                            r.data.as_ref().is_some_and(|d| {
                                d.bytes == request.logical_bytes
                                    && Some(d.allocation) == request.allocation
                            })
                        })
                        .and_then(|r| file_extra_view(r, self.volume))
                        .filter(|e| e.links.value().is_some_and(|n| n > 0));
                    if let Some(values) = value {
                        self.hits.fetch_add(1, Ordering::Relaxed);
                        return metadata::Report {
                            values,
                            issue: None,
                        };
                    }
                    self.fallbacks.fetch_add(1, Ordering::Relaxed);
                    let report = metadata::read_combined(&request.path, self.source, None);
                    if report.issue.is_none()
                        && request.identity.is_some()
                        && report.values.identity.value() != request.identity
                    {
                        return metadata::Report::unavailable(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "MFT 보완 조회 중 파일이 다른 객체로 변경됐습니다",
                        ));
                    }
                    report
                })
                .collect()
        }
    }
    let probe = Probe {
        records: catalog
            .entries()
            .enumerate()
            .filter(|(_, r)| !r.directory && r.data.is_some())
            .map(|(id, r)| (r.reference, id))
            .collect(),
        catalog,
        volume: catalog.geometry.volume,
        source: &source,
        hits: AtomicU64::new(0),
        fallbacks: AtomicU64::new(0),
    };
    let scan = scanner::scan_metadata_observed(
        root,
        detailed,
        scanner::Parallelism::Eight,
        &cancelled,
        progress,
        &probe,
        observer,
    )?;
    let after = source.stat(root)?.ok_or_else(bad)?;
    if after.id != before.id || after.volume != before.volume {
        return Err(bad());
    }
    crate::output::check(&cancelled)?;
    Ok((
        scan,
        serde_json::json!({"mft_files":probe.hits.load(Ordering::Relaxed),"os_fallbacks":probe.fallbacks.load(Ordering::Relaxed)}),
    ))
}

pub fn normalize(
    catalog: &Catalog,
) -> io::Result<(
    Vec<crate::ntfs_tree::Record>,
    std::collections::HashMap<u64, crate::metadata::Extra>,
)> {
    use crate::ntfs_tree::{CachedName, Name as LayoutName, Record, Stream};
    let mut rows = Vec::new();
    let mut extras = std::collections::HashMap::new();
    for record in catalog.entries() {
        let mut attributes = record.attributes.ok_or_else(bad)? & 0x0fffffff;
        if record.directory {
            attributes |= 16;
        }
        let names = record
            .names
            .iter()
            .map(|name| LayoutName {
                parent: name.parent,
                name: name.name.clone(),
                flags: name.namespace as u32,
                cached: Some(CachedName {
                    created: name.times.0,
                    modified: name.times.1,
                    bytes: name.bytes,
                    attributes: name.attributes & 0x0fffffff,
                }),
            })
            .collect();
        let streams = record
            .data
            .as_ref()
            .map(|data| {
                vec![Stream {
                    kind: 128,
                    name: OsString::new(),
                    bytes: data.bytes,
                    allocation: data.allocation,
                }]
            })
            .unwrap_or_default();
        rows.push(Record {
            reference: record.reference,
            attributes,
            names,
            streams,
            times: record.times,
        });
        if let Some(extra) = file_extra_view(record, catalog.geometry.volume) {
            extras.insert(record.reference, extra);
        }
    }
    Ok((rows, extras))
}

/// 변환 완료 후 원본 레코드를 비워 이름의 이중 보관을 제거하는 소유권 이전.
pub fn normalize_owned(
    catalog: &mut Catalog,
    cancelled: impl Fn() -> bool,
) -> io::Result<(
    Vec<crate::ntfs_tree::Record>,
    std::collections::HashMap<u64, crate::metadata::Extra>,
)> {
    use crate::ntfs_tree::{CachedName, Name as LayoutName, Record, Stream};
    crate::output::check(&cancelled)?;
    let full = std::mem::take(&mut catalog.records);
    let compact = catalog.small.take().unwrap_or_default();
    let mut rows = Vec::new();
    let mut extras = std::collections::HashMap::new();
    for record in full.into_iter().map(Stored::from_parsed).chain(compact) {
        crate::output::check(&cancelled)?;
        let mut attributes = record.attributes.ok_or_else(bad)? & 0x0fffffff;
        if record.directory {
            attributes |= 16;
        }
        if let Some(extra) = file_extra_view(record.view(), catalog.geometry.volume) {
            extras.insert(record.reference, extra);
        }
        catalog.moved_names.fetch_add(
            record.names.len() as u64,
            std::sync::atomic::Ordering::Relaxed,
        );
        let names = record
            .names
            .into_iter()
            .map(|n| LayoutName {
                parent: n.parent,
                flags: n.namespace as u32,
                name: n.name,
                cached: Some(CachedName {
                    created: n.times.0,
                    modified: n.times.1,
                    bytes: n.bytes,
                    attributes: n.attributes & 0x0fffffff,
                }),
            })
            .collect();
        let streams = record
            .data
            .map(|d| {
                vec![Stream {
                    kind: 128,
                    name: OsString::new(),
                    bytes: d.bytes,
                    allocation: d.allocation,
                }]
            })
            .unwrap_or_default();
        rows.push(Record {
            reference: record.reference,
            attributes,
            names,
            streams,
            times: record.times,
        });
    }
    Ok((rows, extras))
}
pub fn scan_catalog(
    root: &std::path::Path,
    target: &crate::ntfs_native::Target,
    catalog: &Catalog,
    detailed: bool,
    cancelled: impl Fn() -> bool,
) -> io::Result<crate::scanner::ScanResult> {
    scan_catalog_profiled(root, target, catalog, detailed, cancelled, None)
}
pub fn scan_catalog_profiled(
    root: &std::path::Path,
    target: &crate::ntfs_native::Target,
    catalog: &Catalog,
    detailed: bool,
    cancelled: impl Fn() -> bool,
    metrics: Option<&crate::scanner::ScanMetrics>,
) -> io::Result<crate::scanner::ScanResult> {
    crate::output::check(&cancelled)?;
    let (source, before) = bind_catalog(root, catalog)?;
    let (rows, extra) = {
        let _phase = crate::scanner::profile::scope(
            metrics.map(|m| &m.trace),
            crate::scanner::profile::Stage::MftNormalize,
        );
        normalize(catalog)?
    };
    assemble_catalog(
        root,
        target,
        catalog,
        detailed,
        cancelled,
        Prepared {
            source,
            before,
            rows,
            extra,
            metrics,
        },
    )
}
pub fn scan_catalog_tuned(
    root: &std::path::Path,
    target: &crate::ntfs_native::Target,
    catalog: &mut Catalog,
    detailed: bool,
    cancelled: impl Fn() -> bool,
    options: Options<'_>,
) -> io::Result<crate::scanner::ScanResult> {
    crate::output::check(&cancelled)?;
    let (source, before) = bind_catalog(root, catalog)?;
    let (rows, extra) = {
        let _phase = crate::scanner::profile::scope(
            options.metrics.map(|m| &m.trace),
            crate::scanner::profile::Stage::MftNormalize,
        );
        if options.tuning.move_names {
            normalize_owned(catalog, &cancelled)?
        } else {
            normalize(catalog)?
        }
    };
    assemble_catalog(
        root,
        target,
        catalog,
        detailed,
        cancelled,
        Prepared {
            source,
            before,
            rows,
            extra,
            metrics: options.metrics,
        },
    )
}
fn bind_catalog(
    root: &std::path::Path,
    catalog: &Catalog,
) -> io::Result<(crate::metadata::Named, crate::metadata::Stat)> {
    use crate::metadata::StatSource;
    let source = crate::metadata::Named::for_root(root)?;
    let before = source.stat(root)?.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::Unsupported,
            "독립 full128 루트 대조에 필요한 정보 클래스 미지원",
        )
    })?;
    let mut expected = [0u8; 16];
    expected[..8].copy_from_slice(&before.file_id.to_le_bytes());
    if before.id != expected || before.volume != catalog.geometry.volume {
        return Err(bad());
    }
    Ok((source, before))
}
struct Prepared<'a> {
    source: crate::metadata::Named,
    before: crate::metadata::Stat,
    rows: Vec<crate::ntfs_tree::Record>,
    extra: std::collections::HashMap<u64, crate::metadata::Extra>,
    metrics: Option<&'a crate::scanner::ScanMetrics>,
}
fn assemble_catalog(
    root: &std::path::Path,
    target: &crate::ntfs_native::Target,
    catalog: &Catalog,
    detailed: bool,
    cancelled: impl Fn() -> bool,
    prepared: Prepared<'_>,
) -> io::Result<crate::scanner::ScanResult> {
    use crate::metadata::StatSource;
    let Prepared {
        source,
        before,
        mut rows,
        extra,
        metrics,
    } = prepared;
    let _graph = crate::scanner::profile::scope(
        metrics.map(|m| &m.trace),
        crate::scanner::profile::Stage::MftGraph,
    );
    // MFT의 $FILE_NAME 캐시는 부모 디렉터리 항목의 관찰값을 대신하지 못함.
    for row in &mut rows {
        for name in &mut row.names {
            name.cached = None;
        }
    }
    use crate::scanner::directory;
    use std::{
        collections::HashMap,
        path::PathBuf,
        sync::{Arc, Mutex, OnceLock},
    };
    type Entries = HashMap<OsString, directory::Entry>;
    type Stored = Result<Entries, (io::ErrorKind, Option<i32>, String)>;
    let parents: Mutex<HashMap<PathBuf, Arc<OnceLock<Stored>>>> = Mutex::new(HashMap::new());
    let stopped = std::sync::atomic::AtomicBool::new(false);
    let cancel = || {
        let value = cancelled();
        if value {
            stopped.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        value
    };
    let mut result = crate::ntfs_tree::scan_with_probe(
        root,
        target,
        rows,
        true,
        cancel,
        &|path, row, _| {
            let mut value = extra.get(&row.reference).copied().ok_or_else(bad)?;
            if value.links.value().is_none() {
                catalog
                    .metadata_queries
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let report = crate::metadata::read_combined(path, &source, None);
                if let Some(error) = report.issue {
                    return Err(error);
                }
                if report.values.identity != value.identity {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "MFT 이름과 현재 파일 ID가 다릅니다",
                    ));
                }
                value = report.values;
            }
            if value.links.value()
                != Some(row.names.iter().filter(|name| name.flags & 3 != 2).count() as u32)
            {
                return Err(io::Error::new(io::ErrorKind::InvalidData,format!("MFT 이름 완전성 확인 실패: {} reference={:x}, API/header links={:?}, names={}, namespaces={:?}",path.display(),row.reference,value.links.value(),row.names.len(),row.names.iter().map(|n|n.flags).collect::<Vec<_>>())));
            }
            // 같은 비트가 MFT 내부 EA와 Win32 Recall 상태에 쓰이므로 실제 목록으로 보완.
            if value.links.value().unwrap_or(0) > 1 || row.attributes & 0x40000 != 0 {
                let parent = path.parent().ok_or_else(bad)?;
                let cell = parents
                    .lock()
                    .unwrap()
                    .entry(parent.to_owned())
                    .or_default()
                    .clone();
                let entries = cell.get_or_init(|| {
                    catalog
                        .supplemental
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    let mut entries = HashMap::new();
                    let read = directory::collect(
                        parent,
                        directory::Strategy::Bulk,
                        &|| stopped.load(std::sync::atomic::Ordering::Relaxed),
                        &mut |batch| {
                            for item in batch {
                                let item = item.map_err(|(_, e)| e)?;
                                entries.insert(item.name.clone(), item);
                            }
                            Ok(())
                        },
                    );
                    match read {
                        Ok(()) => Ok(entries),
                        Err(e) => Err((e.kind(), e.raw_os_error(), e.to_string())),
                    }
                });
                let entries = match entries {
                    Ok(v) => v,
                    Err((kind, code, text)) => {
                        return Err(code
                            .map(io::Error::from_raw_os_error)
                            .unwrap_or_else(|| io::Error::new(*kind, text.clone())))
                    }
                };
                let entry = entries
                    .get(path.file_name().ok_or_else(bad)?)
                    .ok_or_else(bad)?;
                if entry.extra.identity != value.identity {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "Hard Link 경로가 다른 객체로 변경됐습니다",
                    ));
                }
            }
            Ok(value)
        },
    )?;
    for file in &mut result.files {
        if file.extra.links.value().unwrap_or(0) > 1 || file.attributes & 0x40000 != 0 {
            let parent = &result.folders[file.parent].path;
            let parents = parents.lock().unwrap();
            let entries = parents
                .get(parent)
                .and_then(|v| v.get())
                .and_then(|v| v.as_ref().ok())
                .ok_or_else(bad)?;
            let entry = entries.get(&file.name).ok_or_else(bad)?;
            file.logical_bytes = entry.logical_bytes;
            file.created = entry.created;
            file.modified = entry.modified;
            file.attributes = entry.attributes;
        }
        if !detailed {
            file.extra = Default::default();
        }
    }
    result.detailed = detailed;
    for folder in &mut result.folders {
        folder.logical_bytes = 0;
        folder.file_count = 0;
        folder.folder_count = 0;
        folder.allocation = Default::default();
    }
    for file in &result.files {
        let folder = &mut result.folders[file.parent];
        folder.logical_bytes = folder
            .logical_bytes
            .checked_add(file.logical_bytes)
            .ok_or_else(bad)?;
        folder.file_count += 1;
        folder.allocation.add(file.extra.allocation)?;
    }
    crate::scanner::finish(&mut result)?;
    let after = source.stat(root)?.ok_or_else(bad)?;
    if after.id != before.id || after.volume != before.volume {
        return Err(bad());
    }
    crate::output::check(&cancelled)?;
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bulk_hybrid_preserves_current_listing_and_falls_back_for_changed_stream_metadata() {
        use crate::metadata::{Stat, StatSource};
        let owned = std::env::temp_dir().join(format!("diskweave-mft-bulk-{}", std::process::id()));
        let root = owned.join("scope");
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("a.bin");
        std::fs::write(&path, b"1234567890123").unwrap();
        std::fs::hard_link(&path, root.join("alias.bin")).unwrap();
        std::fs::hard_link(&path, owned.join("outside.bin")).unwrap();
        let source = crate::metadata::Named::for_root(&root).unwrap();
        let rs = source.stat(&root).unwrap().unwrap();
        let fs = source.stat(&path).unwrap().unwrap();
        let part = |stat: &Stat, parent: u64, name: &str, directory: bool| Parsed {
            reference: stat.file_id as u64,
            base: 0,
            links: if directory { 1 } else { 3 },
            directory,
            attributes: Some(stat.attributes),
            times: Some((stat.created, stat.modified)),
            names: vec![Name {
                parent,
                name: name.into(),
                namespace: 1,
                times: (stat.created, stat.modified),
                bytes: if directory { 0 } else { 13 },
                attributes: stat.attributes,
            }],
            data: if directory {
                None
            } else {
                Some(Data {
                    bytes: 13,
                    allocation: 16,
                    lowest: 0,
                    runs: vec![],
                })
            },
            external_list: None,
            references: vec![],
        };
        let mut file = part(&fs, rs.file_id as u64, "a.bin", false);
        for name in ["alias.bin", "outside.bin"] {
            let mut n = file.names[0].clone();
            n.name = name.into();
            file.names.push(n);
        }
        let mut catalog = Catalog {
            records: vec![part(&rs, 0, ".", true), file],
            geometry: Geometry {
                sector: 512,
                cluster: 4096,
                record: 1024,
                valid: 1024,
                clusters: 100,
                mft_lcn: 20,
                volume: rs.volume,
            },
            bytes: 0,
            reads: 0,
            ioctl_records: 0,
            supplemental: Default::default(),
            metadata_queries: Default::default(),
            decoded_in_place: 0,
            decoded_copies: 0,
            moved_names: Default::default(),
            small: None,
        };
        drop(crate::scanner::scan_with_options(&root, true, || false, |_| {}).unwrap());
        let expected = crate::scanner::scan_with_options(&root, true, || false, |_| {}).unwrap();
        let (result, calls) = scan_bulk(&root, &catalog, true, || false).unwrap();
        assert!(crate::layout::differences(&expected, &result).is_empty());
        assert_eq!(calls, serde_json::json!({"mft_files":2,"os_fallbacks":0}));
        let mut preview = crate::scanner::preview::Model::new(root.clone());
        let (observed, _) = scan_bulk_observed(
            &root,
            &catalog,
            true,
            || false,
            |_| {},
            Some(&mut |batch| preview.apply(batch)),
        )
        .unwrap();
        assert!(crate::layout::differences(&expected, &observed).is_empty());
        assert_eq!(
            (preview.folders[0].bytes, preview.folders[0].files),
            (26, 2)
        );
        assert_eq!(
            preview.folders[0].state(),
            crate::scanner::preview::State::Complete
        );
        catalog.records[1].data.as_mut().unwrap().bytes = 99;
        let (result, calls) = scan_bulk(&root, &catalog, true, || false).unwrap();
        assert!(crate::layout::differences(&expected, &result).is_empty());
        assert_eq!(calls["os_fallbacks"], 2);
        assert_eq!(
            scan_bulk(&root, &catalog, true, || true)
                .err()
                .unwrap()
                .kind(),
            io::ErrorKind::Interrupted
        );
        std::fs::remove_dir_all(owned).unwrap();
    }
    #[test]
    fn ambiguous_raw_attributes_use_the_current_directory_observation() {
        use crate::metadata::{Stat, StatSource};
        let root =
            std::env::temp_dir().join(format!("diskweave-mft-attributes-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("a.bin"), b"1234567890123").unwrap();
        let source = crate::metadata::Named::for_root(&root).unwrap();
        let rs = source.stat(&root).unwrap().unwrap();
        let fs = source.stat(&root.join("a.bin")).unwrap().unwrap();
        let part = |stat: &Stat, parent: u64, name: &str, directory: bool| Parsed {
            reference: stat.file_id as u64,
            base: 0,
            links: 1,
            directory,
            attributes: Some(stat.attributes | if directory { 0 } else { 0x40000 }),
            times: Some((stat.created, stat.modified)),
            names: vec![Name {
                parent,
                name: name.into(),
                namespace: 1,
                times: (stat.created, stat.modified),
                bytes: if directory { 0 } else { 13 },
                attributes: stat.attributes,
            }],
            data: if directory {
                None
            } else {
                Some(Data {
                    bytes: 13,
                    allocation: 16,
                    lowest: 0,
                    runs: vec![],
                })
            },
            external_list: None,
            references: vec![],
        };
        let catalog = Catalog {
            records: vec![
                part(&rs, 0, ".", true),
                part(&fs, rs.file_id as u64, "a.bin", false),
            ],
            geometry: Geometry {
                sector: 512,
                cluster: 4096,
                record: 1024,
                valid: 1024,
                clusters: 100,
                mft_lcn: 20,
                volume: rs.volume,
            },
            bytes: 0,
            reads: 0,
            ioctl_records: 0,
            supplemental: Default::default(),
            metadata_queries: Default::default(),
            decoded_in_place: 0,
            decoded_copies: 0,
            moved_names: Default::default(),
            small: None,
        };
        let target = crate::ntfs_native::target(root.ancestors().last().unwrap()).unwrap();
        let expected = crate::scanner::scan_with_options(&root, true, || false, |_| {}).unwrap();
        let observed = scan_catalog(&root, &target, &catalog, true, || false).unwrap();
        assert!(crate::layout::differences(&expected, &observed).is_empty());
        assert_eq!(
            catalog
                .supplemental
                .load(std::sync::atomic::Ordering::Relaxed),
            1
        );
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn resident_value_cannot_overlap_attribute_header() {
        let mut b = record();
        put16(&mut b, 56 + 20, 0);
        assert!(decode(42, &b, 512, Framing::OnDisk).is_err());
    }
    #[test]
    fn fragmented_mft_reads_separate_extents_and_cancellation_never_publishes_catalog() {
        use std::{cell::Cell, rc::Rc};
        struct Disk {
            record: Vec<u8>,
            reads: usize,
            stop: Rc<Cell<bool>>,
            interrupt: bool,
        }
        impl Source for Disk {
            fn geometry(&mut self) -> io::Result<Geometry> {
                Ok(Geometry {
                    sector: 512,
                    cluster: 4096,
                    record: 1024,
                    valid: 8192,
                    clusters: 100,
                    mft_lcn: 20,
                    volume: 123,
                })
            }
            fn record(&mut self, n: u64) -> io::Result<Vec<u8>> {
                assert_eq!(n, 0);
                Ok(self.record.clone())
            }
            fn read_at(&mut self, offset: u64, bytes: &mut [u8]) -> io::Result<()> {
                assert_eq!(bytes.len(), 4096);
                assert_eq!(offset, [20 * 4096, 40 * 4096][self.reads]);
                bytes.fill(0);
                if self.reads == 0 {
                    bytes[..1024].copy_from_slice(&self.record);
                }
                self.reads += 1;
                if self.interrupt {
                    self.stop.set(true);
                }
                Ok(())
            }
        }
        let mut b = record();
        let mut attr = vec![0; 72];
        put32(&mut attr, 0, 128);
        put32(&mut attr, 4, 72);
        attr[8] = 1;
        put64(&mut attr, 24, 1);
        put16(&mut attr, 32, 64);
        put64(&mut attr, 40, 8192);
        put64(&mut attr, 48, 8192);
        put64(&mut attr, 56, 8192);
        attr[64..71].copy_from_slice(&[0x11, 1, 20, 0x11, 1, 20, 0]);
        b[248..320].copy_from_slice(&attr);
        put32(&mut b, 320, u32::MAX);
        put32(&mut b, 24, 328);
        let stop = Rc::new(Cell::new(false));
        let mut disk = Disk {
            record: b,
            reads: 0,
            stop: stop.clone(),
            interrupt: true,
        };
        assert_eq!(
            collect(&mut disk, || stop.get()).err().unwrap().kind(),
            io::ErrorKind::Interrupted
        );
        assert_eq!(disk.reads, 1);
        stop.set(false);
        disk.reads = 0;
        disk.interrupt = false;
        let catalog = collect(&mut disk, || false).unwrap();
        assert_eq!(disk.reads, 2);
        assert_eq!(catalog.bytes, 8192);
        assert_eq!(catalog.records.len(), 1);
        disk.reads = 0;
        let metrics = crate::scanner::ScanMetrics::default();
        let candidate = collect_tuned(
            &mut disk,
            Options {
                tuning: Tuning {
                    in_place: true,
                    ..Default::default()
                },
                metrics: Some(&metrics),
            },
            || false,
            |_| {},
        )
        .unwrap();
        assert_eq!(candidate.records[0].reference, catalog.records[0].reference);
        assert_eq!((candidate.bytes, candidate.reads), (8192, 2));
        let phases = metrics.trace.snapshot();
        assert!(phases
            .stages
            .iter()
            .any(|s| s.stage == "mft_read" && s.calls == 1));
        assert!(phases
            .stages
            .iter()
            .any(|s| s.stage == "mft_merge" && s.calls == 1));
    }
    #[test]
    fn in_place_records_keep_reference_fields_and_reuse_a_buffer_after_a_bad_record() {
        let original = record();
        let expected = decode(42, &original, 512, Framing::OnDisk)
            .unwrap()
            .unwrap();
        let mut buffer = original.clone();
        let observed = decode_in_place(42, &mut buffer, 512, Framing::OnDisk)
            .unwrap()
            .unwrap();
        assert_eq!(
            (
                observed.reference,
                observed.links,
                observed.times,
                observed.attributes
            ),
            (
                expected.reference,
                expected.links,
                expected.times,
                expected.attributes
            )
        );
        assert_eq!(observed.names[0].name, expected.names[0].name);
        assert_eq!(
            observed.data.as_ref().map(|d| (d.bytes, d.allocation)),
            expected.data.as_ref().map(|d| (d.bytes, d.allocation))
        );
        buffer.copy_from_slice(&original);
        buffer[510] ^= 1;
        assert!(decode_in_place(42, &mut buffer, 512, Framing::OnDisk).is_err());
        buffer.copy_from_slice(&original);
        assert_eq!(
            decode_in_place(43, &mut buffer, 512, Framing::OnDisk)
                .unwrap()
                .unwrap()
                .reference,
            43 | (7 << 48)
        );
        assert_eq!(&original[510..512], &0x55aau16.to_le_bytes());
    }
    #[test]
    fn mft_filename_retains_unpaired_utf16_without_lossy_conversion() {
        use std::os::windows::ffi::OsStrExt;
        let mut b = record();
        put32(&mut b, 152 + 16, 72);
        b[152 + 24 + 64] = 3;
        for (i, v) in [0x97d3u16, 0xd800, 0x41].iter().enumerate() {
            put16(&mut b, 152 + 24 + 66 + i * 2, *v);
        }
        let parsed = decode(42, &b, 512, Framing::OnDisk).unwrap().unwrap();
        assert_eq!(
            parsed.names[0].name.encode_wide().collect::<Vec<_>>(),
            vec![0x97d3, 0xd800, 0x41]
        );
    }
    #[test]
    fn torn_sectors_attribute_overflow_and_unknown_names_never_return_partial_records() {
        let original = record();
        for offset in [4, 6, 20, 24, 28, 60, 510, 1022] {
            let mut bad_record = original.clone();
            bad_record[offset..offset + 2].copy_from_slice(&0xffffu16.to_le_bytes());
            assert!(
                decode(42, &bad_record, 512, Framing::OnDisk).is_err(),
                "offset={offset}"
            );
        }
        let mut invalid_namespace = original.clone();
        invalid_namespace[152 + 24 + 65] = 4;
        assert!(decode(42, &invalid_namespace, 512, Framing::OnDisk).is_err());
        assert!(decode(42, &original[..511], 512, Framing::OnDisk).is_err());
    }

    fn put16(b: &mut [u8], o: usize, v: u16) {
        b[o..o + 2].copy_from_slice(&v.to_le_bytes());
    }
    fn put32(b: &mut [u8], o: usize, v: u32) {
        b[o..o + 4].copy_from_slice(&v.to_le_bytes());
    }
    fn put64(b: &mut [u8], o: usize, v: u64) {
        b[o..o + 8].copy_from_slice(&v.to_le_bytes());
    }
    fn resident(kind: u32, value: &[u8]) -> Vec<u8> {
        let size = (24 + value.len() + 7) & !7;
        let mut b = vec![0; size];
        put32(&mut b, 0, kind);
        put32(&mut b, 4, size as u32);
        put32(&mut b, 16, value.len() as u32);
        put16(&mut b, 20, 24);
        b[24..24 + value.len()].copy_from_slice(value);
        b
    }
    fn record() -> Vec<u8> {
        let mut b = vec![0; 1024];
        b[..4].copy_from_slice(b"FILE");
        put16(&mut b, 4, 48);
        put16(&mut b, 6, 3);
        put16(&mut b, 16, 7);
        put16(&mut b, 18, 3);
        put16(&mut b, 20, 56);
        put16(&mut b, 22, 1);
        put32(&mut b, 28, 1024);
        put32(&mut b, 44, 42);
        let mut standard = vec![0; 72];
        put64(&mut standard, 0, 1000);
        put64(&mut standard, 8, 2000);
        put32(&mut standard, 32, 32);
        let mut name = vec![0; 68];
        put64(&mut name, 0, (1 << 48) | 5);
        put64(&mut name, 8, 1000);
        put64(&mut name, 16, 1500);
        put64(&mut name, 48, 13);
        name[64] = 1;
        name[65] = 1;
        put16(&mut name, 66, 97);
        let mut offset = 56;
        for attr in [
            resident(0x10, &standard),
            resident(0x30, &name),
            resident(0x80, b"1234567890123"),
        ] {
            b[offset..offset + attr.len()].copy_from_slice(&attr);
            offset += attr.len();
        }
        put32(&mut b, offset, u32::MAX);
        put32(&mut b, 24, (offset + 8) as u32);
        put16(&mut b, 48, 0x55aa);
        put16(&mut b, 50, 0xbeef);
        put16(&mut b, 52, 0xfedc);
        put16(&mut b, 510, 0x55aa);
        put16(&mut b, 1022, 0x55aa);
        b
    }
    #[test]
    fn on_disk_record_preserves_link_header_default_stream_and_name_cache() {
        let parsed = decode(42, &record(), 512, Framing::OnDisk)
            .unwrap()
            .unwrap();
        assert_eq!(parsed.reference, (7 << 48) | 42);
        assert_eq!(parsed.links, 3);
        assert_eq!(parsed.times, Some((1000, 2000)));
        assert_eq!(parsed.names[0].times, (1000, 1500));
        assert_eq!(parsed.names[0].name, std::ffi::OsStr::new("a"));
        assert_eq!(parsed.data.as_ref().unwrap().bytes, 13);
        assert_eq!(parsed.data.unwrap().allocation, 16);
    }
    #[test]
    fn nonresident_sparse_data_separates_logical_allocation_and_fragment_mapping() {
        let mut b = record();
        let start = 248;
        let mut attr = vec![0; 80];
        put32(&mut attr, 0, 0x80);
        put32(&mut attr, 4, 80);
        attr[8] = 1;
        put16(&mut attr, 12, 0x8000);
        put64(&mut attr, 24, 3);
        put16(&mut attr, 32, 72);
        put64(&mut attr, 40, 16384);
        put64(&mut attr, 48, 14000);
        put64(&mut attr, 56, 14000);
        put64(&mut attr, 64, 8192);
        attr[72..78].copy_from_slice(&[0x11, 2, 20, 0x01, 2, 0]);
        b[start..start + 80].copy_from_slice(&attr);
        put32(&mut b, start + 80, u32::MAX);
        put32(&mut b, 24, (start + 88) as u32);
        let data = decode(42, &b, 512, Framing::OnDisk)
            .unwrap()
            .unwrap()
            .data
            .unwrap();
        assert_eq!((data.bytes, data.allocation), (14000, 8192));
        assert_eq!(data.runs.len(), 2);
        assert_eq!(data.runs[0].lcn, Some(20));
        assert_eq!(
            (data.runs[1].vcn, data.runs[1].lcn, data.runs[1].clusters),
            (2, None, 2)
        );
    }
    #[test]
    fn extension_records_merge_only_with_matching_base_and_sequence() {
        let mut base = decode(42, &record(), 512, Framing::OnDisk)
            .unwrap()
            .unwrap();
        let mut extension = decode(43, &record(), 512, Framing::OnDisk)
            .unwrap()
            .unwrap();
        extension.base = base.reference;
        extension.attributes = None;
        extension.times = None;
        extension.data = None;
        extension.names[0].name = "alias".into();
        base.references = vec![base.reference, extension.reference];
        let result = merge(vec![base, extension]).unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].names.len(), 2);
        assert_eq!(result[0].data.as_ref().unwrap().bytes, 13);
        let mut extension = decode(43, &record(), 512, Framing::OnDisk)
            .unwrap()
            .unwrap();
        extension.base = (8 << 48) | 42;
        assert!(merge(vec![
            decode(42, &record(), 512, Framing::OnDisk)
                .unwrap()
                .unwrap(),
            extension
        ])
        .is_err());
    }
    #[test]
    fn collector_reads_only_validated_mft_extents_and_stops_on_cancellation() {
        struct Disk {
            record: Vec<u8>,
            reads: usize,
        }
        impl Source for Disk {
            fn geometry(&mut self) -> io::Result<Geometry> {
                Ok(Geometry {
                    sector: 512,
                    cluster: 4096,
                    record: 1024,
                    valid: 1024,
                    clusters: 100,
                    mft_lcn: 20,
                    volume: 123,
                })
            }
            fn record(&mut self, n: u64) -> io::Result<Vec<u8>> {
                assert_eq!(n, 0);
                Ok(self.record.clone())
            }
            fn read_at(&mut self, offset: u64, bytes: &mut [u8]) -> io::Result<()> {
                assert_eq!(offset, 20 * 4096);
                assert_eq!(bytes.len(), 1024);
                bytes.copy_from_slice(&self.record);
                self.reads += 1;
                Ok(())
            }
        }
        let mut record = record();
        let start = 248;
        let mut attr = vec![0; 72];
        put32(&mut attr, 0, 128);
        put32(&mut attr, 4, 72);
        attr[8] = 1;
        put64(&mut attr, 24, 0);
        put16(&mut attr, 32, 64);
        put64(&mut attr, 40, 4096);
        put64(&mut attr, 48, 1024);
        put64(&mut attr, 56, 1024);
        attr[64..68].copy_from_slice(&[0x11, 1, 20, 0]);
        record[start..start + 72].copy_from_slice(&attr);
        put32(&mut record, start + 72, u32::MAX);
        put32(&mut record, 24, (start + 80) as u32);
        let mut disk = Disk { record, reads: 0 };
        let catalog = collect(&mut disk, || false).unwrap();
        assert_eq!(catalog.records.len(), 1);
        assert_eq!(disk.reads, 1);
        assert_eq!(
            collect(&mut disk, || true).err().unwrap().kind(),
            io::ErrorKind::Interrupted
        );
        assert_eq!(disk.reads, 1);
    }
    #[test]
    fn empty_nonresident_stream_accepts_ntfs_minus_one_highest_vcn() {
        let mut b = record();
        let mut attr = vec![0; 72];
        put32(&mut attr, 0, 128);
        put32(&mut attr, 4, 72);
        attr[8] = 1;
        put64(&mut attr, 24, u64::MAX);
        put16(&mut attr, 32, 64);
        b[248..320].copy_from_slice(&attr);
        put32(&mut b, 320, u32::MAX);
        put32(&mut b, 24, 328);
        let data = decode(42, &b, 512, Framing::OnDisk)
            .unwrap()
            .unwrap()
            .data
            .unwrap();
        assert_eq!((data.bytes, data.allocation), (0, 0));
        assert!(data.runs.is_empty());
    }
    #[test]
    fn normalized_catalog_keeps_full_identity_and_name_specific_observation() {
        let geometry = Geometry {
            sector: 512,
            cluster: 4096,
            record: 1024,
            valid: 1024,
            clusters: 100,
            mft_lcn: 20,
            volume: 123,
        };
        let mut part = decode(42, &record(), 512, Framing::OnDisk)
            .unwrap()
            .unwrap();
        let mut alias = part.names[0].clone();
        alias.name = "alias".into();
        part.names.push(alias.clone());
        alias.name = "outside".into();
        part.names.push(alias);
        let catalog = Catalog {
            geometry,
            records: vec![part],
            bytes: 1024,
            reads: 1,
            ioctl_records: 1,
            supplemental: Default::default(),
            metadata_queries: Default::default(),
            decoded_in_place: 0,
            decoded_copies: 0,
            moved_names: Default::default(),
            small: None,
        };
        let (rows, extra) = normalize(&catalog).unwrap();
        let identity = extra[&((7 << 48) | 42)].identity.value().unwrap();
        assert_eq!(identity.volume, 123);
        assert_eq!(
            identity.file,
            [42, 0, 0, 0, 0, 0, 7, 0, 0, 0, 0, 0, 0, 0, 0, 0]
        );
        assert_eq!(
            extra[&((7 << 48) | 42)].links,
            crate::metadata::Field::Value(3)
        );
        assert_eq!(rows[0].names[0].cached.as_ref().unwrap().modified, 1500);
        let mut small = compact_catalog(catalog, || false).unwrap();
        let (compact_rows, compact_extra) = normalize(&small).unwrap();
        assert_eq!(compact_extra, extra);
        assert_eq!(
            (
                compact_rows[0].reference,
                compact_rows[0].attributes,
                compact_rows[0].times
            ),
            (rows[0].reference, rows[0].attributes, rows[0].times)
        );
        assert_eq!(compact_rows[0].names.len(), 3);
        for (a, b) in compact_rows[0].names.iter().zip(&rows[0].names) {
            assert_eq!((&a.name, a.parent, a.flags), (&b.name, b.parent, b.flags));
            assert_eq!(
                a.cached
                    .as_ref()
                    .map(|c| (c.created, c.modified, c.bytes, c.attributes)),
                b.cached
                    .as_ref()
                    .map(|c| (c.created, c.modified, c.bytes, c.attributes))
            );
        }
        assert_eq!(compact_rows[0].streams[0].allocation, 16);
        let (moved_rows, moved_extra) = normalize_owned(&mut small, || false).unwrap();
        assert_eq!(moved_extra, extra);
        assert_eq!(moved_rows[0].names.len(), 3);
        assert_eq!(moved_rows[0].names[1].name, "alias");
        assert_eq!(moved_rows[0].names[2].name, "outside");
        assert_eq!(
            (
                moved_rows[0].streams[0].bytes,
                moved_rows[0].streams[0].allocation
            ),
            (13, 16)
        );
        assert_eq!(
            normalize_owned(&mut small, || true).unwrap_err().kind(),
            io::ErrorKind::Interrupted
        );
    }
    #[test]
    fn collector_bootstraps_nonresident_mft_attribute_list_from_metadata_extents() {
        struct Disk {
            record: Vec<u8>,
            reads: usize,
        }
        impl Source for Disk {
            fn geometry(&mut self) -> io::Result<Geometry> {
                Ok(Geometry {
                    sector: 512,
                    cluster: 4096,
                    record: 1024,
                    valid: 1024,
                    clusters: 100,
                    mft_lcn: 20,
                    volume: 123,
                })
            }
            fn record(&mut self, n: u64) -> io::Result<Vec<u8>> {
                assert_eq!(n, 0);
                Ok(self.record.clone())
            }
            fn read_at(&mut self, offset: u64, bytes: &mut [u8]) -> io::Result<()> {
                self.reads += 1;
                if offset == 30 * 4096 {
                    assert_eq!(bytes.len(), 512);
                    bytes.fill(0);
                    put32(bytes, 0, 128);
                    put16(bytes, 4, 32);
                    put64(bytes, 16, 7 << 48);
                } else {
                    assert_eq!(offset, 20 * 4096);
                    bytes.copy_from_slice(&self.record);
                }
                Ok(())
            }
        }
        fn nonresident(kind: u32, lcn: u8, size: u64) -> Vec<u8> {
            let mut b = vec![0; 72];
            put32(&mut b, 0, kind);
            put32(&mut b, 4, 72);
            b[8] = 1;
            put16(&mut b, 32, 64);
            put64(&mut b, 40, 4096);
            put64(&mut b, 48, size);
            put64(&mut b, 56, size);
            b[64..68].copy_from_slice(&[0x11, 1, lcn, 0]);
            b
        }
        let mut b = record();
        b[248..320].copy_from_slice(&nonresident(32, 30, 32));
        b[320..392].copy_from_slice(&nonresident(128, 20, 1024));
        put32(&mut b, 392, u32::MAX);
        put32(&mut b, 24, 400);
        let mut disk = Disk {
            record: b,
            reads: 0,
        };
        let catalog = collect(&mut disk, || false).unwrap();
        assert_eq!(catalog.records.len(), 1);
        assert_eq!(disk.reads, 2);
    }
    #[test]
    fn dos_name_pair_is_one_windows_link_and_header_counts_both_names() {
        let mut part = decode(42, &record(), 512, Framing::OnDisk)
            .unwrap()
            .unwrap();
        let mut dos = part.names[0].clone();
        dos.namespace = 2;
        dos.name = "A~1".into();
        part.names.push(dos);
        part.links = 2;
        let catalog = Catalog {
            geometry: Geometry {
                sector: 512,
                cluster: 4096,
                record: 1024,
                valid: 1024,
                clusters: 100,
                mft_lcn: 20,
                volume: 123,
            },
            records: vec![part],
            bytes: 1024,
            reads: 1,
            ioctl_records: 1,
            supplemental: Default::default(),
            metadata_queries: Default::default(),
            decoded_in_place: 0,
            decoded_copies: 0,
            moved_names: Default::default(),
            small: None,
        };
        let (_, extra) = normalize(&catalog).unwrap();
        assert_eq!(
            extra[&((7 << 48) | 42)].links,
            crate::metadata::Field::Value(1)
        );
    }
    #[test]
    fn inconsistent_header_uses_os_link_count_only_after_identity_and_complete_names_agree() {
        use crate::metadata::StatSource;
        let root = std::env::temp_dir().join(format!("diskweave-mft-links-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("long-file.bin"), b"1234567890123").unwrap();
        let source = crate::metadata::Named::for_root(&root).unwrap();
        let rs = source.stat(&root).unwrap().unwrap();
        let fs = source.stat(&root.join("long-file.bin")).unwrap().unwrap();
        fn part(s: &crate::metadata::Stat, parent: u64, name: &str, dir: bool) -> Parsed {
            Parsed {
                reference: s.file_id as u64,
                base: 0,
                links: 1,
                directory: dir,
                attributes: Some(s.attributes),
                times: Some((s.created, s.modified)),
                names: vec![Name {
                    parent,
                    name: name.into(),
                    namespace: 1,
                    times: (s.created, s.modified),
                    bytes: if dir { 0 } else { 13 },
                    attributes: s.attributes,
                }],
                data: if dir {
                    None
                } else {
                    Some(Data {
                        bytes: 13,
                        allocation: 16,
                        lowest: 0,
                        runs: vec![],
                    })
                },
                external_list: None,
                references: vec![],
            }
        }
        let directory = part(&rs, 0, ".", true);
        let mut file = part(&fs, rs.file_id as u64, "long-file.bin", false);
        let mut dos = file.names[0].clone();
        dos.name = "LONGFI~1.BIN".into();
        dos.namespace = 2;
        file.names.push(dos);
        let catalog = Catalog {
            geometry: Geometry {
                sector: 512,
                cluster: 4096,
                record: 1024,
                valid: 1024,
                clusters: 100,
                mft_lcn: 20,
                volume: rs.volume,
            },
            records: vec![directory, file],
            bytes: 0,
            reads: 0,
            ioctl_records: 0,
            supplemental: Default::default(),
            metadata_queries: Default::default(),
            decoded_in_place: 0,
            decoded_copies: 0,
            moved_names: Default::default(),
            small: None,
        };
        let target = crate::ntfs_native::target(root.ancestors().last().unwrap()).unwrap();
        let scan = scan_catalog(&root, &target, &catalog, true, || false).unwrap();
        assert_eq!(scan.files[0].extra.links, crate::metadata::Field::Value(1));
        assert_eq!(
            catalog
                .metadata_queries
                .load(std::sync::atomic::Ordering::Relaxed),
            1
        );
        std::fs::remove_dir_all(root).unwrap();
    }
}
