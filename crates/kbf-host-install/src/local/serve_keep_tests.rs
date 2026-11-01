//! The `--keep-releases` flag of the serve mode.

use super::serve::{MAX_KEEP_RELEASES, ServeConfig, USAGE};

fn parse(extra: &[&str]) -> Result<ServeConfig, String> {
    let mut words = vec!["--socket", "/s", "--socket-user", "u"];
    words.extend_from_slice(extra);
    let args: Vec<String> = words.iter().map(|w| (*w).to_owned()).collect();
    ServeConfig::parse(&args)
}

/// Without the flag a host keeps four releases besides the ones that must stay.
///
/// Catches: a default other than four.
#[test]
fn the_default_is_four_releases() {
    assert_eq!(parse(&[]).unwrap().keep_releases, 4);
}

/// Both ends of the range are accepted, in either spelling of the flag.
///
/// Catches: the range made exclusive at either end.
#[test]
fn two_and_thirty_two_are_accepted_in_both_spellings() {
    assert_eq!(MAX_KEEP_RELEASES, 32);
    assert_eq!(parse(&["--keep-releases", "2"]).unwrap().keep_releases, 2);
    assert_eq!(parse(&["--keep-releases", "32"]).unwrap().keep_releases, 32);
    assert_eq!(parse(&["--keep-releases=3"]).unwrap().keep_releases, 3);
}

/// Below two, above thirty-two and what is not a number are refused with the range; so are a
/// flag given twice and one with no value.
#[test]
fn a_count_out_of_range_or_not_a_number_is_refused() {
    for bad in ["1", "0", "33", "abc", "-2", "", "4.5"] {
        let why = parse(&["--keep-releases", bad]).unwrap_err();
        assert!(why.contains("from 2 to 32"), "{bad:?}: {why}");
    }
    let twice = parse(&["--keep-releases", "3", "--keep-releases=4"]).unwrap_err();
    assert!(twice.contains("given twice"), "{twice}");
    let none = parse(&["--keep-releases"]).unwrap_err();
    assert!(none.contains("needs a value"), "{none}");
}

/// The usage line names the flag.
#[test]
fn the_usage_names_the_flag() {
    assert!(USAGE.contains("[--keep-releases N]"));
}
