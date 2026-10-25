//! The calendar forms of S3.

use super::time::{amz_date, http_date, iso8601};

/// Asserts that an HTTP date has the right weekday, month and time at the epoch, at a date of
/// the SigV4 documentation, on a leap day of a century year, and on the day after the February
/// of a century year that is not a leap year.
#[test]
fn an_http_date_is_right_at_known_times() {
    assert_eq!(http_date(0), "Thu, 01 Jan 1970 00:00:00 GMT");
    let documented = http_date(1_369_353_600_000);
    assert_eq!(documented, "Fri, 24 May 2013 00:00:00 GMT");
    assert_eq!(http_date(951_868_799_000), "Tue, 29 Feb 2000 23:59:59 GMT");
    let century = http_date(4_107_542_400_000);
    assert_eq!(century, "Mon, 01 Mar 2100 00:00:00 GMT");
}

/// Asserts that the time of a listing has the milliseconds, and the right day at the same
/// known times.
#[test]
fn a_listing_time_has_milliseconds() {
    assert_eq!(iso8601(0), "1970-01-01T00:00:00.000Z");
    assert_eq!(iso8601(1_369_353_600_123), "2013-05-24T00:00:00.123Z");
    assert_eq!(iso8601(951_868_799_999), "2000-02-29T23:59:59.999Z");
    assert_eq!(iso8601(4_107_542_400_000), "2100-03-01T00:00:00.000Z");
}

/// Asserts that the date of a signature is right at the known times, and that two of them
/// compare in the order of their times, across a day and a year.
#[test]
fn a_signature_date_is_right_and_sorts_by_time() {
    assert_eq!(amz_date(0), "19700101T000000Z");
    assert_eq!(amz_date(1_369_353_600_000), "20130524T000000Z");
    assert_eq!(amz_date(951_868_799_000), "20000229T235959Z");
    assert_eq!(amz_date(4_107_542_400_000), "21000301T000000Z");
    assert!(amz_date(86_399_000) < amz_date(86_400_000));
    assert!(amz_date(946_684_799_000) < amz_date(946_684_800_000));
}
