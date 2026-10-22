use std::cmp::Ordering::{Equal, Greater, Less};

use super::{DebVersion, DebVersionError};

fn v(text: &str) -> DebVersion {
    DebVersion::parse(text).unwrap_or_else(|e| panic!("{text}: {e}"))
}

/// Catches an ordering that differs from `dpkg --compare-versions` on a known pair: the
/// pairs are the cases of deb-version(7) and of dpkg's own test suite, written `a < b`.
#[test]
fn ordering_matches_dpkg_on_known_pairs() {
    let ascending = [
        // `~` sorts before anything, even the end of the text.
        ("1.0~~", "1.0~"),
        ("1.0~", "1.0~rc1"),
        ("1.0~rc1", "1.0"),
        ("1.0~rc1", "1.0~rc2"),
        ("1.0~beta", "1.0~rc"),
        ("1.0-1~bpo1", "1.0-1"),
        // The end of the text sorts before a letter, a letter before other characters.
        ("1.0", "1.0a"),
        ("1.0a", "1.0b"),
        ("1.0a", "1.0+"),
        ("1.0", "1.0+b1"),
        ("1.0+", "1.0."),
        // Digit runs are numbers, not text.
        ("1.9", "1.10"),
        ("1.2", "1.12"),
        ("1.0a2", "1.0a10"),
        // Letters and digits.
        ("1a", "1b"),
        ("1.0a", "1.0.1"),
        // The revision after the last hyphen.
        ("1.0", "1.0-1"),
        ("1.0-1", "1.0-2"),
        ("1.0-2", "1.0-10"),
        ("1.0-1", "1.0-1+b1"),
        ("1.0-1", "1.1-0"),
        ("1.0-1-1", "1.0-1-2"),
        // The epoch outranks the rest.
        ("9.9", "1:0.1"),
        ("1:9", "2:0"),
        ("0.5", "1:0.1"),
    ];
    for (low, high) in ascending {
        assert_eq!(v(low).cmp(&v(high)), Less, "{low} < {high}");
        assert_eq!(v(high).cmp(&v(low)), Greater, "{high} > {low}");
    }
}

/// Catches an equality that is textual: leading zeros and an explicit epoch of zero do not
/// change a version, and a revision of zero is not an empty revision.
#[test]
fn equal_versions_compare_equal() {
    for (a, b) in [
        ("1.0", "1.0"),
        ("1.0", "1.00"),
        ("1.0", "01.0"),
        ("0:1.0", "1.0"),
        ("1.0-0", "1.0-00"),
        ("1.0-1", "1.0-01"),
    ] {
        assert_eq!(v(a).cmp(&v(b)), Equal, "{a} = {b}");
        assert_eq!(v(a), v(b));
    }
    assert_eq!(v("1.0").cmp(&v("1.0-0")), Equal, "an absent revision is revision 0");
}

/// Catches a parser that takes the first hyphen, not the last, as the revision's start.
#[test]
fn the_revision_starts_at_the_last_hyphen() {
    assert_eq!(v("1.0-1-2").cmp(&v("1.0-1-02")), Equal);
    assert_eq!(v("1.0-1-2").cmp(&v("1.0-2")), Greater, "upstream 1.0-1 is above 1.0");
}

/// Catches a parser that accepts what dpkg refuses, or an error code that moved.
#[test]
fn bad_versions_do_not_parse() {
    for (text, why) in [
        ("", DebVersionError::Empty),
        ("1.0 ", DebVersionError::BadChar),
        ("1.0\n", DebVersionError::BadChar),
        ("1.0_1", DebVersionError::BadChar),
        ("1.0;id", DebVersionError::BadChar),
        (":1.0", DebVersionError::BadEpoch),
        ("a:1.0", DebVersionError::BadEpoch),
        ("1a:1.0", DebVersionError::BadEpoch),
        ("99999999999999999999:1.0", DebVersionError::BadEpoch),
        ("1:", DebVersionError::EmptyUpstream),
        ("-1", DebVersionError::EmptyUpstream),
        ("1:-1", DebVersionError::EmptyUpstream),
        ("a1", DebVersionError::UpstreamNotDigit),
        ("~1", DebVersionError::UpstreamNotDigit),
        ("1.0-", DebVersionError::EmptyRevision),
    ] {
        assert_eq!(DebVersion::parse(text), Err(why), "{text:?}");
    }
}

/// The error text is what the operator reads on stderr.
#[test]
fn errors_read_aloud() {
    assert_eq!(DebVersionError::Empty.to_string(), "empty version");
    assert!(DebVersionError::BadChar.to_string().contains("alphabet"));
    assert!(DebVersionError::BadEpoch.to_string().contains("epoch"));
    assert!(DebVersionError::EmptyUpstream.to_string().contains("empty"));
    assert!(DebVersionError::UpstreamNotDigit.to_string().contains("digit"));
    assert!(DebVersionError::EmptyRevision.to_string().contains("revision"));
}

/// `PartialOrd` agrees with `Ord`.
#[test]
fn partial_ord_is_ord() {
    assert_eq!(v("1.0").partial_cmp(&v("1.1")), Some(Less));
}
