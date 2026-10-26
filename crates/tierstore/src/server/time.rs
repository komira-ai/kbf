//! The calendar forms S3 uses: the `Last-Modified` header, the time of an object in a listing,
//! and the `x-amz-date` of a signed request.

const DAY_MILLIS: u64 = 86_400_000;

const WEEKDAYS: &str = "SunMonTueWedThuFriSat";

const MONTHS: &str = "JanFebMarAprMayJunJulAugSepOctNovDec";

/// A time in UTC, to the millisecond.
struct Civil {
    year: i64,
    month: u32,
    day: u32,
    hour: u64,
    minute: u64,
    second: u64,
    millis: u64,
    weekday: usize,
}

/// The three letters at position `index` of `names`.
fn abbreviation(names: &'static str, index: usize) -> &'static str {
    &names[3 * index..3 * index + 3]
}

fn civil(ms: u64) -> Civil {
    let days = ms / DAY_MILLIS;
    let rest = ms % DAY_MILLIS;
    let (year, month, day) = civil_from_days(days as i64);
    Civil {
        year,
        month,
        day,
        hour: rest / 3_600_000,
        minute: rest % 3_600_000 / 60_000,
        second: rest % 60_000 / 1000,
        millis: rest % 1000,
        // 1970-01-01 was a Thursday, the fifth day of a week that starts on Sunday.
        weekday: ((days + 4) % 7) as usize,
    }
}

/// Year, month and day of a count of days since 1970-01-01 (proleptic Gregorian), after
/// Howard Hinnant's `civil_from_days`.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

/// The time in the form of an HTTP date, `Thu, 01 Jan 1970 00:00:00 GMT`.
pub(crate) fn http_date(ms: u64) -> String {
    let c = civil(ms);
    let weekday = abbreviation(WEEKDAYS, c.weekday);
    let month = abbreviation(MONTHS, (c.month - 1) as usize);
    let (day, year) = (c.day, c.year);
    let (hour, minute, second) = (c.hour, c.minute, c.second);
    format!("{weekday}, {day:02} {month} {year:04} {hour:02}:{minute:02}:{second:02} GMT")
}

/// The time in the form of a listing, `1970-01-01T00:00:00.000Z`.
pub(crate) fn iso8601(ms: u64) -> String {
    let c = civil(ms);
    let (year, month, day) = (c.year, c.month, c.day);
    let (hour, minute, second, millis) = (c.hour, c.minute, c.second, c.millis);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{millis:03}Z")
}

/// The time in the form of the `x-amz-date` header, `19700101T000000Z`. Two of them compare
/// in the order of the times they stand for.
pub(crate) fn amz_date(ms: u64) -> String {
    let c = civil(ms);
    let (year, month, day) = (c.year, c.month, c.day);
    let (hour, minute, second) = (c.hour, c.minute, c.second);
    format!("{year:04}{month:02}{day:02}T{hour:02}{minute:02}{second:02}Z")
}
