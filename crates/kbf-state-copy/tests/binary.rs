//! The `kbf-state-copy` binary as a process: its exit status for each way a run can end that
//! needs no database.

mod support;

use std::process::{Command, Output};

use support::{UNREACHABLE, built, golden_copy, open, scratch};

fn run(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_kbf-state-copy"))
        .args(args)
        .output()
        .expect("run kbf-state-copy")
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// Asserts that `--help` ends with status 0 and prints the usage to the standard output.
#[test]
fn the_help_ends_well_on_the_standard_output() {
    let out = run(&["--help"]);
    assert_eq!(out.status.code(), Some(0));
    assert!(stdout(&out).contains("Usage: kbf-state-copy"));
    assert!(out.stderr.is_empty());
}

/// Asserts that a mistake on the command line ends with status 1 and prints the reason to
/// the standard error, and that a flag that would skip a check is such a mistake.
#[test]
fn a_flag_that_skips_a_check_is_a_mistake_with_status_1() {
    let out = run(&["state.db", "postgres://h/db", "--force"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(stdout(&out).is_empty());
    assert!(stderr(&out).contains("--force"));
}

/// Asserts that a state file a frontend has open ends the run with status 2, a refusal, and
/// prints the reason to the standard error and nothing to the standard output.
#[test]
fn a_state_file_a_frontend_has_open_ends_with_status_2() {
    let dir = scratch("binary-lock");
    let written = built(&dir);
    let store = open(&written.db);
    let out = run(&[written.db.to_str().expect("a UTF-8 path"), UNREACHABLE]);
    assert_eq!(out.status.code(), Some(2));
    assert!(stdout(&out).is_empty());
    assert!(stderr(&out).starts_with("kbf-state-copy: refused: "));
    drop(store);
}

/// Asserts that a target that cannot be reached ends a dry run with status 1 after the
/// report of the file, with the reason on the standard error.
#[test]
fn a_target_that_cannot_be_reached_ends_with_status_1() {
    let dir = scratch("binary-unreachable");
    let db = golden_copy(&dir);
    let out = run(&[db.to_str().expect("a UTF-8 path"), UNREACHABLE]);
    assert_eq!(out.status.code(), Some(1));
    assert!(stdout(&out).contains("session/"));
    assert!(stderr(&out).starts_with("kbf-state-copy: failed: "));
}
