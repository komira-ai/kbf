//! Unit tests of `cargo xtask gamma check-build`: what buck2's output means, which targets are
//! refused and the text that is shown. The verb as the workflow runs it, with a fake `buck2`, is
//! tested in `tests/gamma_check.rs`.

use super::*;

/// The end of a build that ran every action on the farm or in its cache, as buck2 prints it
/// with `--console simple`.
const SUCCESS: &str = "Build ID: 0b8cf4f6\n\
    Jobs completed: 9. Time elapsed: 4.2s.\n\
    Cache hits: 89%. Commands: 9 (cached: 8, remote: 1, local: 0)\n\
    BUILD SUCCEEDED\n";

/// The same build with three actions run on this machine (the farm did not take them).
const LOCAL: &str = "Build ID: 0b8cf4f6\n\
    Cache hits: 66%. Commands: 9 (cached: 6, remote: 0, local: 3)\n\
    BUILD SUCCEEDED\n";

/// A build that failed on the farm.
const FAILED: &str = "Build ID: 0b8cf4f6\n\
    Cache hits: 0%. Commands: 2 (cached: 0, remote: 2, local: 0)\n\
    BUILD FAILED\n";

/// Why an output with no `BUILD ...` line fails.
const NO_VERDICT: &str = "no BUILD SUCCEEDED or BUILD FAILED line";

/// Asserts that `text` holds `needle`.
fn has(text: &str, needle: &str) {
    assert!(text.contains(needle), "{needle:?} is not in {text}");
}

/// The message of a usage error; any other answer fails the test.
fn usage<T: std::fmt::Debug>(answer: Result<T, Error>) -> String {
    match answer {
        Err(Error::Usage(message)) => message,
        other => panic!("not a usage error: {other:?}"),
    }
}

/// Proves a build that succeeded with no local command passes, and shows its Commands line.
/// Catches a parser that reads the wrong count as `local`.
#[test]
fn a_success_with_no_local_command_passes() {
    let judged = judge(SUCCESS, true);
    assert_eq!(judged.failure, None);
    let line = "Commands: 9 (cached: 8, remote: 1, local: 0)";
    assert_eq!(judged.commands.as_deref(), Some(line));
}

/// Proves a success that ran three actions locally fails and says so: the farm did not do the
/// work. Catches a check that reads only `BUILD SUCCEEDED`.
#[test]
fn a_success_with_local_commands_fails() {
    let judged = judge(LOCAL, true);
    assert_eq!(judged.failure, Some("local commands ran"));
    has(judged.commands.as_deref().unwrap(), "local: 3");
}

/// Proves `BUILD FAILED` fails even when the Commands line is clean.
#[test]
fn a_failed_build_fails() {
    let judged = judge(FAILED, false);
    assert_eq!(judged.failure, Some("BUILD FAILED"));
    let judged = judge(FAILED, true);
    assert_eq!(judged.failure, Some("BUILD FAILED"));
}

/// Proves an output without a summary fails, whichever half is missing: nothing is a pass by
/// default.
#[test]
fn a_missing_summary_fails() {
    let nothing = judge("", true);
    assert_eq!(nothing.failure, Some(NO_VERDICT));
    assert_eq!(nothing.commands, None);
    let no_verdict = "Commands: 9 (cached: 9, remote: 0, local: 0)\n";
    let judged = judge(no_verdict, true);
    assert_eq!(judged.failure, Some(NO_VERDICT));
    let no_counts = judge("BUILD SUCCEEDED\n", true);
    assert_eq!(no_counts.failure, Some("no Commands line"));
    assert_eq!(no_counts.commands, None);
}

/// Proves the last summary wins, in both orders: a retry that succeeded after a failure
/// passes, and a failure after a success fails.
#[test]
fn the_last_summary_wins() {
    let retried = format!("{FAILED}{SUCCESS}");
    assert_eq!(judge(&retried, true).failure, None);
    let regressed = format!("{SUCCESS}{FAILED}");
    assert_eq!(judge(&regressed, true).failure, Some("BUILD FAILED"));
    let locals = format!("{LOCAL}{SUCCESS}");
    assert_eq!(judge(&locals, true).failure, None);
    let later_local = format!("{SUCCESS}{LOCAL}");
    let judged = judge(&later_local, true);
    assert_eq!(judged.failure, Some("local commands ran"));
}

/// Proves a Commands line that does not parse is a failure and not a skipped check: no
/// total, no closing parenthesis, a count that is not a number, no `local` count, and a list
/// holding anything but lowercase words (a name with a dot is host-like).
#[test]
fn a_commands_line_that_does_not_parse_fails() {
    let bad = [
        "Commands: 3",
        "Commands: 3 (cached: 3, remote: 0, local: 0",
        "Commands: x (cached: 3, remote: 0, local: 0)",
        "Commands: 3 (cached: 3, remote: 0)",
        "Commands: 3 (cached: 3, remote: 0, local: x)",
        "Commands: 3 (cached: 3, local: +0)",
        "Commands: 3 (cached: 3, host: a.example, local: 0)",
        "Commands: 3 (cached: 3, HOST: 1, local: 0)",
    ];
    for line in bad {
        let judged = judge(&format!("{line}\nBUILD SUCCEEDED\n"), true);
        let why = Some("the Commands line does not parse");
        assert_eq!(judged.failure, why, "{line}");
        assert_eq!(judged.commands, None, "{line}");
    }
}

/// Proves buck2's own exit status counts: a clean summary with a failing exit is a failure.
#[test]
fn a_failing_exit_status_fails_a_clean_summary() {
    let judged = judge(SUCCESS, false);
    assert_eq!(judged.failure, Some("buck2 exited with a failure status"));
}

/// Proves the fallback count buck2 appends after the line is not part of what is shown, and
/// that a line with a prefix of its own is read from `Commands: ` on.
#[test]
fn the_line_is_cut_at_its_closing_parenthesis() {
    let log = "Cache hits: 0%. Commands: 2 (cached: 0, remote: 2, local: 0). Fallback: 0/2\n\
        BUILD SUCCEEDED\n";
    let judged = judge(log, true);
    assert_eq!(judged.failure, None);
    let line = "Commands: 2 (cached: 0, remote: 2, local: 0)";
    assert_eq!(judged.commands.as_deref(), Some(line));
}

/// Proves what is shown is the Commands line and the verdict and nothing else, even when the
/// output names a host in other lines or in a line that is not a Commands line.
#[test]
fn the_summary_is_the_commands_line_and_the_verdict_alone() {
    let noise = "Connecting to farm-front.tailnet.example:8980\n\
        error: worker canary-host.tailnet.example refused\n";
    let judged = judge(&format!("{noise}{SUCCESS}"), true);
    let want = "### gamma check\n\n\
        Commands: 9 (cached: 8, remote: 1, local: 0)\n\n\
        PASS\n";
    assert_eq!(judged.summary(), want);
    let failed = judge(&format!("{noise}{LOCAL}"), true).summary();
    let want = "### gamma check\n\n\
        Commands: 9 (cached: 6, remote: 0, local: 3)\n\n\
        FAIL (local commands ran)\n";
    assert_eq!(failed, want);
    // A Commands line with a host in it is not shown at all.
    let host = "Commands: 3 (cached: 3, local: 0, name: canary.example)\nBUILD SUCCEEDED\n";
    let shown = judge(&format!("{noise}{host}"), true).summary();
    let want = "### gamma check\n\n\
        Commands: not printed\n\n\
        FAIL (the Commands line does not parse)\n";
    assert_eq!(shown, want);
    assert!(!shown.contains("canary"));
}

/// Catches: `--no-remote-cache` (or any flag, or an argument file) arriving through the
/// variable that names the targets, which would make the check prove nothing about the
/// farm's cache; and an empty list. Plain target names, one or many, pass.
#[test]
fn the_targets_are_never_a_flag() {
    let ok = targets("komira//src/a:a  //src/b:b\n komira//src/c/...").unwrap();
    assert_eq!(ok, ["komira//src/a:a", "//src/b:b", "komira//src/c/..."]);
    let cache = usage(targets("komira//src/a:a --no-remote-cache"));
    has(&cache, "--no-remote-cache");
    let equals = usage(targets("--no-remote-cache=true"));
    has(&equals, "--no-remote-cache");
    has(&usage(targets("-c")), "flag");
    has(&usage(targets("--local-only komira//src/a:a")), "flag");
    has(&usage(targets("@argfile")), "argument file");
    has(&usage(targets("")), "no target");
    has(&usage(targets(" \n ")), "no target");
}

/// Catches: a flag that bypasses the cache among the arguments the verb builds itself, and a
/// config file or a target left out. Every target is last, after the config.
#[test]
fn the_buck2_arguments_are_a_plain_build_with_the_config_file() {
    let config = Path::new("/w/farm.buckconfig");
    let args = buck2_args(config, &["komira//a:a", "komira//b:b"]);
    let want = [
        "build",
        "--console",
        "simple",
        "--config-file",
        "/w/farm.buckconfig",
        "komira//a:a",
        "komira//b:b",
    ];
    assert_eq!(args, want);
    for arg in &args {
        assert!(!arg.contains("no-remote-cache"), "{arg}");
    }
}
