//! The `gc --keep` flag. Children of the app tests, which parse the command line.

use super::*;

fn keep_of(args: &[&str]) -> usize {
    match parse(args).verb {
        Verb::Gc(g) => g.keep,
        _ => panic!("{args:?} is not gc"),
    }
}

/// Catches a `--keep` that takes a count below the floor, a default other than four, and a
/// refusal that does not say why: `gc` alone keeps four, 2 and more are taken as given, and
/// 0, 1, a negative and a word are refused by the parser with the floor or the word named, so
/// no release is removed on the strength of them.
///
/// Catches: the `value_parser` removed from the flag (1 is taken); the default changed to the
/// old one release.
#[test]
fn gc_keeps_four_by_default_and_the_parser_refuses_a_count_below_two() {
    assert_eq!(keep_of(&["gc"]), 4);
    assert_eq!(keep_of(&["gc", "--keep", "2"]), 2);
    assert_eq!(keep_of(&["gc", "--keep=32"]), 32);
    assert_eq!(keep_of(&["--apply", "gc", "--keep", "7"]), 7);
    for (bad, why) in [
        ("1", "a host keeps at least 2 releases"),
        ("0", "a host keeps at least 2 releases"),
        ("-1", "unexpected argument"),
        ("many", "is not a count of releases"),
    ] {
        let err = Cli::try_parse_from(["kbf-installer", "gc", "--keep", bad])
            .err()
            .unwrap_or_else(|| panic!("--keep {bad} was taken"));
        assert!(err.to_string().contains(why), "--keep {bad}: {err}");
    }
}
