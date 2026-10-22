//! A UTC time as `YYYYMMDDTHHMMSSZ`, for names that must sort as times (the suffix of a rolled-back
//! directory), and as RFC 3339, for the times a status endpoint reports.

/// A time of day on a date of the proleptic Gregorian calendar, UTC.
struct Civil {
    year: i64,
    month: i64,
    day: i64,
    hour: u64,
    minute: u64,
    second: u64,
}

/// `secs` since the epoch as a date and a time of day, UTC.
fn civil(secs: u64) -> Civil {
    // Days since the epoch to a civil date (Howard Hinnant's `civil_from_days`).
    let days = i64::try_from(secs / 86_400).unwrap_or(i64::MAX / 2);
    let rest = secs % 86_400;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let mp = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    Civil {
        year: year_of_era + era * 400 + i64::from(month <= 2),
        month,
        day,
        hour: rest / 3600,
        minute: rest % 3600 / 60,
        second: rest % 60,
    }
}

/// `YYYYMMDDTHHMMSSZ` for `secs` since the epoch, UTC and fixed width.
#[must_use]
pub fn utc_stamp(secs: u64) -> String {
    let Civil {
        year,
        month,
        day,
        hour,
        minute,
        second,
    } = civil(secs);
    format!("{year:04}{month:02}{day:02}T{hour:02}{minute:02}{second:02}Z")
}

/// `YYYY-MM-DDTHH:MM:SSZ` (RFC 3339, UTC, whole seconds) for `secs` since the epoch.
#[must_use]
pub fn rfc3339(secs: u64) -> String {
    let Civil {
        year,
        month,
        day,
        hour,
        minute,
        second,
    } = civil(secs);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

#[cfg(test)]
mod tests {
    use super::{rfc3339, utc_stamp};

    /// Catches the stamp drifting from fixed-width UTC: a name that does not sort as time
    /// (local time, no zero padding, a wrong leap day or year end) makes a rolled-back
    /// directory's age unreadable.
    ///
    /// Mutant seen red: the days-to-date conversion off by one in a leap year.
    #[test]
    fn stamp_is_fixed_width_utc_and_sorts_as_time() {
        assert_eq!(utc_stamp(0), "19700101T000000Z");
        assert_eq!(utc_stamp(1_788_220_800), "20260901T000000Z");
        assert_eq!(utc_stamp(1_798_761_599), "20261231T235959Z");
        assert_eq!(utc_stamp(1_835_395_200), "20280229T000000Z");
        assert_eq!(utc_stamp(1_803_859_200), "20270301T000000Z");
        assert_eq!(utc_stamp(4_107_542_399), "21000228T235959Z");
        let mut times = [1_790_000_000, 1_788_220_800, 1_790_000_001, 1_795_000_000];
        times.sort_unstable();
        let named: Vec<String> = times.iter().map(|t| utc_stamp(*t)).collect();
        let mut sorted = named.clone();
        sorted.sort();
        assert_eq!(named, sorted);
    }

    /// Catches the RFC 3339 form drifting from `YYYY-MM-DDTHH:MM:SSZ`: the separators, the zero
    /// padding, the `Z` of UTC, and the same date as the stamp for the same second (a leap day,
    /// a year end, a second before midnight).
    ///
    /// Mutants it catches: a separator dropped or a space for the `T`; an hour, minute or second
    /// without its padding; local time or an offset instead of `Z`; a date that is not the
    /// stamp's.
    #[test]
    fn rfc3339_is_the_stamps_date_and_time_with_separators() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339(1_792_658_476), "2026-10-22T08:41:16Z");
        assert_eq!(rfc3339(1_798_761_599), "2026-12-31T23:59:59Z");
        assert_eq!(rfc3339(1_835_395_200), "2028-02-29T00:00:00Z");
        assert_eq!(rfc3339(1_788_220_805), "2026-09-01T00:00:05Z");
        for secs in [0, 1_788_220_800, 1_835_395_200, 4_107_542_399] {
            let plain = rfc3339(secs).replace(['-', ':'], "");
            assert_eq!(plain, utc_stamp(secs));
        }
    }
}
