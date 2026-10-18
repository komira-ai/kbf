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

#[test]
fn the_sentinels_command_line_is_the_one_the_harness_writes() {
    // Catches: the harness and the sentinel disagreeing on a flag (renamed on one side): the
    // sentinel then refuses its command line, exits at once, and a killed harness is not
    // cleaned up, which nothing else would show. Every flag is set, and the socket is
    // optional. Mutant: `--kill-timeout-ms` written as `--kill-timeout`.
    use clap::Parser as _;
    let reap = |socket: Option<&str>| Reap {
        buck2: PathBuf::from("/b/buck2"),
        project: PathBuf::from("/p q"),
        lanes: 3,
        run_id: "-7".to_owned(),
        kill_timeout_ms: 1500,
        socket: socket.map(PathBuf::from),
        pgids: socket.map(|_| PathBuf::from("/l o/gs")),
    };
    for r in [reap(Some("/tmp/s.sock")), reap(None)] {
        let words = std::iter::once(OsString::from("kbf-beta")).chain(r.args());
        match Cli::try_parse_from(words).expect("parses").scenario {
            Scenario::Reap(parsed) => assert_eq!(parsed, r),
            Scenario::B0(_) => panic!("the sentinel's words ran B0"),
        }
    }
}

#[test]
fn the_sentinel_is_not_in_the_help() {
    // Catches: an internal subcommand shown to the people who read `kbf-beta --help`.
    use clap::CommandFactory as _;
    let help = Cli::command().render_help().to_string();
    assert!(help.contains("b0"), "{help}");
    assert!(!help.contains("reap"), "{help}");
}
