//! This release's socket answers against the previous release's: `previous_answer/v0_0_63_answer.rs`
//! is v0.0.63's `src/local/answer.rs` as it shipped, compiled here as `previous`. Every line the
//! previous helper wrote is the line this one writes for the same answer, so an agent of either
//! release reads this helper as it read the previous one; the one code this release adds,
//! `leases_live`, is one the previous helper never wrote.

use kbf_host_install::contract::local::{Answer, Code};

/// What the shipped file reads of its crate: the cut of a detail for the log, as v0.0.63's
/// `src/outcome.rs` has it.
mod outcome {
    pub const MAX_DETAIL_LEN: usize = 2048;
}

// The shipped file, whole; the parts of it no test here calls are dead in this crate.
#[allow(dead_code)]
#[path = "previous_answer/v0_0_63_answer.rs"]
mod previous;

/// Catches a line of a refusal or a failure that this release writes otherwise than the
/// previous one did: for each of the previous helper's codes, its `refused` and `failed`
/// lines, made by the previous release's own code, are byte for byte the lines this release
/// makes for the code of the same spelling; and `leases_live` is not a code of the previous
/// release.
#[test]
fn every_refusal_the_previous_helper_wrote_is_written_the_same_here() {
    assert_eq!(previous::Code::ALL.len(), 22);
    for old in previous::Code::ALL {
        let new = Code::from_code(old.code()).expect("each previous code is still a code");
        for (was, is) in [
            (
                previous::Answer::Refused(old, "d".to_owned()),
                Answer::Refused(new, "d".to_owned()),
            ),
            (
                previous::Answer::Failed(old, "d".to_owned()),
                Answer::Failed(new, "d".to_owned()),
            ),
        ] {
            assert_eq!(was.json_line(), is.json_line(), "{}", old.code());
        }
    }
    assert_eq!(previous::Code::from_code("leases_live"), None);
    assert_eq!(Code::from_code("leases_live"), Some(Code::LeasesLive));
}

/// Catches an answer of an update that this release writes otherwise than the previous one:
/// `staged`, `switched`, `confirmed` and `reverted`, made by the previous release's code and by
/// this one's, are the same lines.
#[test]
fn the_answers_of_an_update_are_the_previous_helpers_lines() {
    let (v, c) = ("0.1.7+gbbbbbbbbbbbb-3", "b".repeat(40));
    let pairs = [
        (
            previous::Answer::Staged {
                version: v.to_owned(),
                commit: c.clone(),
                current: false,
            },
            Answer::Staged {
                version: v.to_owned(),
                commit: c.clone(),
                current: false,
            },
        ),
        (
            previous::Answer::Switched {
                version: v.to_owned(),
                commit: c.clone(),
                deadline_unix: 1_600,
            },
            Answer::Switched {
                version: v.to_owned(),
                commit: c,
                deadline_unix: 1_600,
            },
        ),
        (
            previous::Answer::Confirmed {
                version: v.to_owned(),
            },
            Answer::Confirmed {
                version: v.to_owned(),
            },
        ),
        (
            previous::Answer::Reverted { version: None },
            Answer::Reverted { version: None },
        ),
    ];
    for (was, is) in pairs {
        assert_eq!(was.json_line(), is.json_line());
    }
}
