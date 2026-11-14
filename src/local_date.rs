use std::io;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[repr(C)]
#[derive(Clone, Copy, Default, PartialEq, Eq)]
struct Calendar {
    year: u16,
    month: u16,
    weekday: u16,
    day: u16,
    hour: u16,
    minute: u16,
    second: u16,
    milliseconds: u16,
}

#[repr(C)]
struct DynamicTimeZone {
    bias: i32,
    standard_name: [u16; 32],
    standard_date: Calendar,
    standard_bias: i32,
    daylight_name: [u16; 32],
    daylight_date: Calendar,
    daylight_bias: i32,
    key_name: [u16; 128],
    dynamic_daylight_disabled: u8,
}
#[repr(C)]
#[derive(Default)]
struct FileTime {
    low: u32,
    high: u32,
}

#[link(name = "kernel32")]
extern "system" {
    fn GetDynamicTimeZoneInformation(zone: *mut DynamicTimeZone) -> u32;
    fn TzSpecificLocalTimeToSystemTimeEx(
        zone: *const DynamicTimeZone,
        local: *const Calendar,
        utc: *mut Calendar,
    ) -> i32;
    fn SystemTimeToTzSpecificLocalTimeEx(
        zone: *const DynamicTimeZone,
        utc: *const Calendar,
        local: *mut Calendar,
    ) -> i32;
    fn SystemTimeToFileTime(utc: *const Calendar, file: *mut FileTime) -> i32;
    fn FileTimeToSystemTime(file: *const FileTime, utc: *mut Calendar) -> i32;
}

fn invalid() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        "날짜는 유효한 YYYY-MM-DD 형식(1601~9998년)이어야 합니다",
    )
}

fn month_days(year: u16, month: u16) -> u16 {
    match month {
        2 if year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400)) => {
            29
        }
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

fn parse(text: &str) -> io::Result<Option<Calendar>> {
    let text = text.trim();
    if text.is_empty() {
        return Ok(None);
    }
    let b = text.as_bytes();
    if b.len() != 10
        || b[4] != b'-'
        || b[7] != b'-'
        || b.iter()
            .enumerate()
            .any(|(i, byte)| i != 4 && i != 7 && !byte.is_ascii_digit())
    {
        return Err(invalid());
    }
    let year = text[..4].parse::<u16>().map_err(|_| invalid())?;
    let month = text[5..7].parse::<u16>().map_err(|_| invalid())?;
    let day = text[8..].parse::<u16>().map_err(|_| invalid())?;
    if !(1601..=9998).contains(&year)
        || !(1..=12).contains(&month)
        || day == 0
        || day > month_days(year, month)
    {
        return Err(invalid());
    }
    Ok(Some(Calendar {
        year,
        month,
        day,
        ..Default::default()
    }))
}

fn midnight(calendar: Calendar, zone: &DynamicTimeZone) -> io::Result<SystemTime> {
    let mut utc = Calendar::default();
    let mut round_trip = Calendar::default();
    let mut file = FileTime::default();
    // C ABI 버퍼 수명을 호출 동안 유지하고 반환 상태 확인. DST로 존재하지 않는 자정은 왕복 검증으로 거부.
    unsafe {
        if TzSpecificLocalTimeToSystemTimeEx(zone, &calendar, &mut utc) == 0
            || SystemTimeToTzSpecificLocalTimeEx(zone, &utc, &mut round_trip) == 0
            || SystemTimeToFileTime(&utc, &mut file) == 0
        {
            return Err(io::Error::last_os_error());
        }
    }
    if (
        round_trip.year,
        round_trip.month,
        round_trip.day,
        round_trip.hour,
        round_trip.minute,
    ) != (calendar.year, calendar.month, calendar.day, 0, 0)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "해당 시간대에 존재하지 않는 날짜 경계입니다",
        ));
    }
    let ticks = (u64::from(file.high) << 32) | u64::from(file.low);
    const UNIX_TICKS: u64 = 116_444_736_000_000_000;
    let delta = ticks.abs_diff(UNIX_TICKS);
    let duration = Duration::new(delta / 10_000_000, ((delta % 10_000_000) * 100) as u32);
    let time = if ticks >= UNIX_TICKS {
        UNIX_EPOCH.checked_add(duration)
    } else {
        UNIX_EPOCH.checked_sub(duration)
    };
    time.ok_or_else(invalid)
}

/// 로컬 날짜 양끝 포함 조건을 UTC의 시작 이상·종료 미만 범위로 변환.
pub fn range(from: &str, through: &str) -> io::Result<(Option<SystemTime>, Option<SystemTime>)> {
    let start = parse(from)?;
    let end = parse(through)?;
    if start
        .zip(end)
        .is_some_and(|(a, b)| (a.year, a.month, a.day) > (b.year, b.month, b.day))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "시작 날짜가 종료 날짜보다 늦습니다",
        ));
    }
    if start.is_none() && end.is_none() {
        return Ok((None, None));
    }
    // 숫자·고정 배열로만 구성된 Windows ABI 출력 구조체 초기화.
    let mut zone: DynamicTimeZone = unsafe { std::mem::zeroed() };
    if unsafe { GetDynamicTimeZoneInformation(&mut zone) } == u32::MAX {
        return Err(io::Error::last_os_error());
    }
    let start = start.map(|d| midnight(d, &zone)).transpose()?;
    let end = end
        .map(|mut d| {
            d.day += 1;
            if d.day > month_days(d.year, d.month) {
                d.day = 1;
                d.month += 1;
            }
            if d.month > 12 {
                d.month = 1;
                d.year += 1;
            }
            midnight(d, &zone)
        })
        .transpose()?;
    Ok((start, end))
}

pub fn display(time: Option<SystemTime>) -> String {
    let Some(time) = time else {
        return "미수집".into();
    };
    let formatted = (|| -> io::Result<String> {
        const UNIX_TICKS: u64 = 116_444_736_000_000_000;
        let (duration, after) = match time.duration_since(UNIX_EPOCH) {
            Ok(d) => (d, true),
            Err(e) => (e.duration(), false),
        };
        let delta = duration
            .as_secs()
            .checked_mul(10_000_000)
            .and_then(|v| v.checked_add(u64::from(duration.subsec_nanos() / 100)))
            .ok_or_else(invalid)?;
        let ticks = if after {
            UNIX_TICKS.checked_add(delta)
        } else {
            UNIX_TICKS.checked_sub(delta)
        }
        .ok_or_else(invalid)?;
        let file = FileTime {
            low: ticks as u32,
            high: (ticks >> 32) as u32,
        };
        let mut utc = Calendar::default();
        let mut local = Calendar::default();
        // 현재 시스템 시간대에 따른 표시용 변환. 날짜 필터의 원본 UTC 값은 유지.
        let mut zone: DynamicTimeZone = unsafe { std::mem::zeroed() };
        unsafe {
            if GetDynamicTimeZoneInformation(&mut zone) == u32::MAX
                || FileTimeToSystemTime(&file, &mut utc) == 0
                || SystemTimeToTzSpecificLocalTimeEx(&zone, &utc, &mut local) == 0
            {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(format!(
            "{:04}-{:02}-{:02} {:02}:{:02}",
            local.year, local.month, local.day, local.hour, local.minute
        ))
    })();
    formatted.unwrap_or_else(|_| "표시 불가".into())
}
