//! A UTC time as `YYYYMMDDTHHMMSSZ`, for names that must sort as times (the suffix of a rolled-back
//! directory).

/// `YYYYMMDDTHHMMSSZ` for `secs` since the epoch, UTC and fixed width.
#[must_use]
pub fn utc_stamp(secs: u64) -> String {
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
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}{month:02}{day:02}T{:02}{:02}{:02}Z",
        rest / 3600,
        rest % 3600 / 60,
        rest % 60
    )
}

#[cfg(test)]
mod tests {
    use super::utc_stamp;

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
}
