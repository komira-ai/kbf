//! The two time formats S3 answers with, read into milliseconds since the Unix epoch: the
//! ISO 8601 `<LastModified>` of a listing (`2009-10-12T17:50:30.000Z`) and the HTTP date of
//! a `Last-Modified` header (`Sun, 06 Nov 1994 08:49:37 GMT`).
//!
//! Both are UTC. A time before the epoch, a year past 9999 or a field out of its range is
//! refused, so a garbled answer is never read as a time.

/// The month names of an HTTP date.
pub(crate) const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// The milliseconds since the epoch of `YYYY-MM-DDTHH:MM:SS[.fff]Z`. A fraction of a second
/// is cut to milliseconds.
pub(crate) fn iso8601_millis(text: &str) -> Option<u64> {
    let text = text.strip_suffix('Z')?;
    let (date, time) = text.split_once('T')?;
    let (time, fraction) = time.split_once('.').unwrap_or((time, ""));
    let date = numbers::<3>(date, '-')?;
    let clock = numbers::<3>(time, ':')?;
    unix_millis(date, clock, fraction_millis(fraction)?)
}

/// The milliseconds since the epoch of `Www, DD Mon YYYY HH:MM:SS GMT`. The name of the
/// weekday is not checked.
pub(crate) fn http_date_millis(text: &str) -> Option<u64> {
    let (_, rest) = text.split_once(", ")?;
    let mut fields = rest.split(' ');
    let day = digits(fields.next()?)?;
    let name = fields.next()?;
    let month = MONTHS.iter().position(|month| *month == name)? as u64 + 1;
    let year = digits(fields.next()?)?;
    let clock = numbers::<3>(fields.next()?, ':')?;
    (fields.next()? == "GMT").then_some(())?;
    fields.next().is_none().then_some(())?;
    unix_millis([year, month, day], clock, 0)
}

/// The milliseconds since the epoch of a date `[year, month, day]` and a time of day
/// `[hour, minute, second]`, plus `millis`.
fn unix_millis(date: [u64; 3], clock: [u64; 3], millis: u64) -> Option<u64> {
    let [year, month, day] = date;
    let [hour, minute, second] = clock;
    (year < 10_000).then_some(())?;
    (1..=12).contains(&month).then_some(())?;
    let last_day = days_in_month(year, month);
    (1..=last_day).contains(&day).then_some(())?;
    (hour < 24).then_some(())?;
    (minute < 60).then_some(())?;
    (second < 60).then_some(())?;
    let days = days_from_civil(year as i64, month as i64, day as i64);
    let of_day = (hour * 3600 + minute * 60 + second) as i64;
    let seconds = u64::try_from(days * 86_400 + of_day).ok()?;
    Some(seconds * 1000 + millis)
}

/// The days of `month` (1 to 12) of `year` in the Gregorian calendar.
fn days_in_month(year: u64, month: u64) -> u64 {
    const DAYS: [u64; 12] = [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    // A leap year is one divisible by 4, except a century that 400 does not divide.
    let by_four = u64::from(year.is_multiple_of(4));
    let by_hundred = u64::from(year.is_multiple_of(100));
    let by_four_hundred = u64::from(year.is_multiple_of(400));
    let leap = by_four + by_four_hundred - by_hundred;
    DAYS[(month - 1) as usize] + u64::from(month == 2) * leap
}

/// The days from 1970-01-01 to a date of the proleptic Gregorian calendar, after Howard
/// Hinnant's `days_from_civil`.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = year - i64::from(month <= 2);
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let day_of_year = (153 * ((month + 9) % 12) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// The `N` numbers `text` holds between `separator`s, each of plain digits.
fn numbers<const N: usize>(text: &str, separator: char) -> Option<[u64; N]> {
    let parts: Vec<u64> = text.split(separator).map(digits).collect::<Option<_>>()?;
    parts.try_into().ok()
}

/// The number `text` spells in plain digits: no sign, no space, not empty.
fn digits(text: &str) -> Option<u64> {
    let plain = text.bytes().all(|byte| byte.is_ascii_digit());
    plain.then(|| text.parse().ok()).flatten()
}

/// The milliseconds of a fraction of a second written as digits, cut or padded to three.
fn fraction_millis(fraction: &str) -> Option<u64> {
    let plain = fraction.bytes().all(|byte| byte.is_ascii_digit());
    let padded = format!("{fraction:0<3}");
    plain.then(|| digits(&padded[..3])).flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Asserts that both formats are read to the millisecond: the documented example of each,
    /// the epoch, a leap day of a year divisible by 4 and one divisible by 400, the first day
    /// after a century that is not a leap year, and a fraction of a second that is absent,
    /// short or longer than three digits.
    #[test]
    fn times_are_read_to_the_millisecond() {
        let cases = [
            ("1970-01-01T00:00:00Z", 0),
            ("2009-10-12T17:50:30.000Z", 1_255_369_830_000),
            ("2009-10-12T17:50:30Z", 1_255_369_830_000),
            ("2009-10-12T17:50:30.5Z", 1_255_369_830_500),
            ("2009-10-12T17:50:30.1239Z", 1_255_369_830_123),
            ("2000-02-29T23:59:59.999Z", 951_868_799_999),
            ("2100-03-01T00:00:00.000Z", 4_107_542_400_000),
        ];
        for (text, want) in cases {
            assert_eq!(iso8601_millis(text), Some(want), "{text}");
        }
        assert!(iso8601_millis("2004-02-29T00:00:00Z").is_some());
        let http = "Sun, 06 Nov 1994 08:49:37 GMT";
        assert_eq!(http_date_millis(http), Some(784_111_777_000));
        let epoch = "Thu, 01 Jan 1970 00:00:00 GMT";
        assert_eq!(http_date_millis(epoch), Some(0));
    }

    /// Asserts that text which is not an ISO time is refused: one with no `Z` or no `T`, a
    /// part missing or added, a sign or a letter in a number, a field out of its range (a day
    /// past the end of its month, and 29 February of a year that is not a leap year, included),
    /// a year past 9999, or a time before the epoch.
    #[test]
    fn garbled_iso_times_are_refused() {
        for text in [
            "",
            "2009-10-12T17:50:30",
            "2009-10-12 17:50:30Z",
            "2009-10T17:50:30Z",
            "2009-10-12-1T17:50:30Z",
            "2009-10-12T17:50Z",
            "2009-10-+2T17:50:30Z",
            "2009-10-1xT17:50:30Z",
            "2009-13-12T17:50:30Z",
            "2009-00-12T17:50:30Z",
            "2009-10-32T17:50:30Z",
            "2009-10-00T17:50:30Z",
            "2009-02-30T17:50:30Z",
            "2009-04-31T17:50:30Z",
            "2009-02-29T17:50:30Z",
            "2100-02-29T17:50:30Z",
            "2009-10-12T24:50:30Z",
            "2009-10-12T17:60:30Z",
            "2009-10-12T17:50:60Z",
            "2009-10-12T17:50:30.abcZ",
            "10000-01-01T00:00:00Z",
            "1969-12-31T23:59:59Z",
        ] {
            assert_eq!(iso8601_millis(text), None, "{text:?}");
        }
    }

    /// Asserts that text which is not an HTTP date is refused: one with no comma, an unknown
    /// month, a zone other than GMT, a field missing at each position, something after the
    /// zone, a number that is not digits, a day past the end of its month, or a time before
    /// the epoch.
    #[test]
    fn garbled_http_dates_are_refused() {
        for text in [
            "",
            "Fri, 31 Apr 2009 08:49:37 GMT",
            "Sun 06 Nov 1994 08:49:37 GMT",
            "Sun, 06",
            "Sun, 06 Nov",
            "Sun, 06 Nov 1994",
            "Sun, 06 Nov 1994 08:49:37",
            "Sun, 06 Foo 1994 08:49:37 GMT",
            "Sun, 06 Nov 1994 08:49:37 UTC",
            "Sun, 06 Nov 1994 08:49:37 GMT extra",
            "Sun, xx Nov 1994 08:49:37 GMT",
            "Sun, 06 Nov 19x4 08:49:37 GMT",
            "Sun, 06 Nov 1994 08:49 GMT",
            "Sun, 06 Nov 1969 23:59:59 GMT",
        ] {
            assert_eq!(http_date_millis(text), None, "{text:?}");
        }
    }
}
