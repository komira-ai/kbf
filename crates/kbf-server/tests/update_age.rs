//! R4, the age check on the build seq (`kbf_server::update::age`): every row of its table.

use kbf_server::update::age::{Build, Refusal, check};

/// The running build: seq 500, commit `aaaaaaaaaaaa`.
const OWN: Build<'static> = Build {
    seq: 500,
    commit: "aaaaaaaaaaaa",
};
/// 40 digits that `OWN.commit` names.
const OWN_FULL: &str = "aaaaaaaaaaaa0000000000000000000000000000";
/// A commit that is not the running build's, text-smaller and text-larger than it.
const LOW: &str = "0000000000000000000000000000000000000001";
const HIGH: &str = "ffffffffffffffffffffffffffffffffffffffff";

/// Catches: a target above the running seq refused, or accepted only when its text is larger
/// (a 12-hex text compare): a newer build with a smaller commit text must pass.
#[test]
fn a_newer_target_is_allowed_whatever_its_commit_text() {
    assert_eq!(check(OWN, Some(501), LOW), Ok(()));
    assert_eq!(check(OWN, Some(501), HIGH), Ok(()));
    assert_eq!(check(OWN, Some(9000), LOW), Ok(()));
}

/// Catches: `<=` for `<` (the repeat of the running build refused), and equal seq with the
/// running commit refused.
#[test]
fn an_equal_seq_of_the_same_commit_is_allowed() {
    assert_eq!(check(OWN, Some(500), OWN_FULL), Ok(()));
}

/// Catches: an equal seq of another commit allowed (two branches cannot be ordered), under
/// either text order.
#[test]
fn an_equal_seq_of_another_commit_is_a_tie() {
    assert_eq!(check(OWN, Some(500), LOW), Err(Refusal::SeqTie));
    assert_eq!(check(OWN, Some(500), HIGH), Err(Refusal::SeqTie));
}

/// Catches: an older target allowed, and an order by commit text: the older build has the
/// larger text here and the smaller there.
#[test]
fn an_older_target_is_refused_whatever_its_commit_text() {
    assert_eq!(check(OWN, Some(499), LOW), Err(Refusal::Older));
    assert_eq!(check(OWN, Some(499), HIGH), Err(Refusal::Older));
    assert_eq!(check(OWN, Some(1), HIGH), Err(Refusal::Older));
}

/// Catches: an unknown target taken as the newest (or as allowed), `None` and 0 told apart.
#[test]
fn an_unknown_target_seq_is_refused_when_the_own_seq_is_known() {
    assert_eq!(check(OWN, None, HIGH), Err(Refusal::SeqUnknown));
    assert_eq!(check(OWN, Some(0), HIGH), Err(Refusal::SeqUnknown));
    // Even the running commit: with no seq nothing is known of it.
    assert_eq!(check(OWN, None, OWN_FULL), Err(Refusal::SeqUnknown));
}

/// Catches: R4 refusing on a build that could not count (it cannot apply there), for any
/// target including an unknown or older-looking one.
#[test]
fn an_unknown_own_seq_allows_everything() {
    let blind = Build {
        seq: 0,
        commit: "unknown",
    };
    for target in [None, Some(0), Some(1), Some(500)] {
        assert_eq!(check(blind, target, LOW), Ok(()), "{target:?}");
    }
}

/// Catches: `unknown` or a short text taken to name the target's commit, so an equal seq of
/// another commit is allowed.
#[test]
fn a_commit_that_is_not_twelve_digits_names_nothing() {
    for own in ["unknown", "", "aaaa"] {
        let build = Build {
            seq: 500,
            commit: own,
        };
        assert_eq!(
            check(build, Some(500), OWN_FULL),
            Err(Refusal::SeqTie),
            "{own:?}"
        );
    }
}

/// The codes are the wire contract of the 409 that will carry them.
#[test]
fn the_codes_are_stable_and_distinct() {
    assert_eq!(Refusal::SeqUnknown.code(), "seq_unknown");
    assert_eq!(Refusal::Older.code(), "older");
    assert_eq!(Refusal::SeqTie.code(), "seq_tie");
    assert_eq!(Refusal::Older.to_string(), "older");
}

/// Catches: a running build that knows its full commit comparing by prefix. One 12-digit
/// prefix names two 40-digit commits; with the full commit the first is the same build and
/// the second, equal in seq, is another commit (a tie). Only exact comparison passes both.
#[test]
fn a_full_running_commit_tells_two_commits_with_one_prefix_apart() {
    let own = Build {
        seq: 500,
        commit: OWN_FULL,
    };
    let sibling = "aaaaaaaaaaaa1111111111111111111111111111";
    assert_eq!(check(own, Some(500), OWN_FULL), Ok(()));
    assert_eq!(check(own, Some(500), sibling), Err(Refusal::SeqTie));
    // With only the 12 digits both look like the running build: the best it can say.
    assert_eq!(check(OWN, Some(500), OWN_FULL), Ok(()));
    assert_eq!(check(OWN, Some(500), sibling), Ok(()));
}

/// Catches: the commit compare taking the longer text as the prefix, or the other way round:
/// a running build named in full is the same commit as a target named by 12 digits of it, and
/// a target named by fewer than 12 digits is no commit at all.
#[test]
fn the_shorter_commit_text_may_be_either_one_and_must_be_12_digits() {
    let full = Build {
        seq: 500,
        commit: OWN_FULL,
    };
    assert_eq!(check(full, Some(500), "aaaaaaaaaaaa"), Ok(()));
    assert_eq!(check(full, Some(500), "aaaaaaaaaaa"), Err(Refusal::SeqTie));
}
