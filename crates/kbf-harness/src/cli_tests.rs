//! The command line: what it refuses, and the exit status of each way a run ends.

use std::ffi::OsString;

use super::*;

fn args(words: &[&str]) -> Vec<OsString> {
    std::iter::once("kbf-beta")
        .chain(words.iter().copied())
        .map(OsString::from)
        .collect()
}

fn none(_: &str) -> Option<String> {
    None
}

#[test]
fn help_is_not_a_failure() {
    // Catches: `--help` exiting 2, which a workflow step would show as a failed run.
    assert_eq!(status(args(&["--help"]), none), 0);
    assert_eq!(status(args(&["b0", "--help"]), none), 0);
}

#[test]
fn a_refused_command_line_exits_2() {
    // Catches: a missing required flag, an unknown flag, an unknown scenario or an unknown
    // mutant starting a run (or exiting 1, which says a gate failed).
    for words in [
        &[][..],
        &["b1"],
        &["b0"],
        &["b0", "--no-such-flag"],
        &[
            "b0",
            "--bin-dir",
            "x",
            "--buck2",
            "y",
            "--project",
            "p",
            "--work",
            "w",
            "--mutant",
            "nope",
        ],
    ] {
        assert_eq!(status(args(words), none), 2, "{words:?}");
    }
}

#[test]
fn a_work_directory_that_cannot_be_made_exits_2() {
    // Catches: a run that cannot write its result exiting with a gate's status, or
    // running anyway: with no document there is nothing to read of it.
    let words = [
        "b0",
        "--bin-dir",
        "x",
        "--buck2",
        "y",
        "--project",
        "p",
        "--work",
        "/proc/no/such/work",
    ];
    assert_eq!(status(args(&words), none), 2);
}

#[test]
fn the_mutants_are_the_scenarios() {
    assert_eq!(
        Mutant::from(MutantFlag::ShrinkNodeCap),
        Mutant::ShrinkNodeCap
    );
    assert_eq!(
        Mutant::from(MutantFlag::ShrinkEveryCap),
        Mutant::ShrinkEveryCap
    );
    assert_eq!(
        Mutant::from(MutantFlag::KillNodeMidway),
        Mutant::KillNodeMidway
    );
}

#[test]
fn the_exit_code_wraps_the_status() {
    // `run` is `status` as an `ExitCode`.
    let code = run(args(&["--help"]), none);
    assert_eq!(format!("{code:?}"), format!("{:?}", ExitCode::SUCCESS));
}

#[test]
fn the_wall_bound_adds_up_and_saturates() {
    // Catches: `duration + build timeout + 60` overflowing (a panic in a debug build and a
    // bound of a few seconds in a release one) on a command line with absurd seconds.
    // Mutant: plain `+`.
    assert_eq!(wall_bound(300, 300), Duration::from_secs(660));
    assert_eq!(wall_bound(u64::MAX, 300), Duration::from_secs(u64::MAX));
    assert_eq!(wall_bound(300, u64::MAX), Duration::from_secs(u64::MAX));
    assert_eq!(wall_bound(u64::MAX - 30, 0), Duration::from_secs(u64::MAX));
}
