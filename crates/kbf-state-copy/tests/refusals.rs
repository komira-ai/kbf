//! A run of the command line that ends before it writes anything, and needs no database:
//! the help, a mistake on the command line, the checks that come before the target, and a
//! target that cannot be reached.

mod support;

use std::io;

use kbf_state_copy::Seams;
use support::{
    GOLDEN_FAMILIES, UNREACHABLE, args, built, golden_copy, has, lacks, names, open, run_main,
    scratch, starts,
};

/// A writer whose reader has gone.
struct Closed;

impl io::Write for Closed {
    fn write(&mut self, _bytes: &[u8]) -> io::Result<usize> {
        Err(io::Error::other("the pipe is closed"))
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Asserts that the help names both arguments and `--apply`, states the exit status, ends
/// well, and offers no way to skip a check.
#[test]
fn the_help_names_the_arguments_and_the_exit_status_and_offers_no_force() {
    let ran = run_main(&["--help"]);
    assert_eq!(ran.code, 0);
    assert!(ran.err.is_empty(), "{}", ran.err);
    has(&ran.out, "Usage: kbf-state-copy");
    has(&ran.out, "<STATE_DB>");
    has(&ran.out, "<POSTGRES_URL>");
    has(&ran.out, "--apply");
    has(&ran.out, "Exit status: 0 done, 2 refused");
    lacks(&ran.out, "--force");
    let version = run_main(&["--version"]);
    assert_eq!(version.code, 0);
    starts(&version.out, "kbf-state-copy ");
}

/// Asserts that a mistake on the command line fails with status 1, not 2 (which means a
/// refusal), prints the usage on the error stream, and prints nothing else: no arguments, a
/// missing one, an extra one, and `--force`.
#[test]
fn a_mistake_on_the_command_line_is_status_1() {
    let cases = [
        vec![],
        vec!["state.db"],
        vec!["state.db", "postgres://h/db", "extra"],
        vec!["state.db", "postgres://h/db", "--force"],
    ];
    for argv in &cases {
        let ran = run_main(argv);
        assert_eq!(ran.code, 1, "{argv:?}");
        assert!(ran.out.is_empty(), "{argv:?}: {}", ran.out);
        has(&ran.err, "Usage: kbf-state-copy");
    }
    has(&run_main(&cases[3]).err, "--force");
}

/// Asserts that a frontend that has the state file open is refused with status 2 before the
/// file is read or the target is touched (the URL here is one nothing answers), for a dry
/// run and for a copy, and that nothing is printed to the report stream.
#[test]
fn a_frontend_that_has_the_file_open_is_refused_before_anything_else() {
    let dir = scratch("refusal-lock");
    let written = built(&dir);
    let store = open(&written.db);
    let db = written.db.to_str().expect("a UTF-8 path");
    for argv in [vec![db, UNREACHABLE], vec![db, UNREACHABLE, "--apply"]] {
        let ran = run_main(&argv);
        assert_eq!(ran.code, 2, "{argv:?}: {}", ran.err);
        assert!(ran.out.is_empty(), "{}", ran.out);
        starts(&ran.err, "kbf-state-copy: refused: ");
        has(&ran.err, "state.db.lock");
    }
    drop(store);
}

/// Asserts that a target that cannot be reached fails a dry run with status 1 after the
/// report of the file is printed (each family with its row count, the totals), and that the
/// state file and its directory are left exactly as they were.
#[test]
fn an_unreachable_target_fails_after_the_report_of_the_file() {
    let dir = scratch("refusal-unreachable");
    let db = golden_copy(&dir);
    let bytes = std::fs::read(&db).expect("read the file");
    let before = names(&dir);
    let ran = run_main(&[db.to_str().expect("a UTF-8 path"), UNREACHABLE]);
    assert_eq!(ran.code, 1, "{}", ran.err);
    starts(&ran.err, "kbf-state-copy: failed: ");
    starts(&ran.out, "kbf-state-copy (dry run)\nstate file: ");
    has(
        &ran.out,
        "key/value rows: 14; counters: 1; highest version: 36; head revision: 36\n",
    );
    for (family, rows) in GOLDEN_FAMILIES {
        has(&ran.out, &format!("  {family:<8}  {rows:>6}  "));
    }
    has(&ran.out, &format!("  {:<7}  {:>6}  ", "golden/", 1));
    lacks(&ran.out, "dry run: nothing was written");
    assert_eq!(std::fs::read(&db).expect("read again"), bytes);
    assert_eq!(names(&dir), before);
}

/// Asserts that a copy that cannot reach its target says so and prints no more than the
/// report, and that a URL the store does not take, or one that asks for TLS, fails after the
/// report with the reason.
#[test]
fn a_url_the_store_does_not_take_fails_after_the_report() {
    let dir = scratch("refusal-url");
    let db = golden_copy(&dir);
    let path = db.to_str().expect("a UTF-8 path");
    let ran = run_main(&[path, "--apply", UNREACHABLE]);
    assert_eq!(ran.code, 1, "{}", ran.err);
    starts(&ran.out, "kbf-state-copy (copy)\n");
    lacks(&ran.out, "copied");
    for (url, said) in [
        ("mysql://h/db", "must start with postgres://"),
        ("postgres://h/db?sslmode=require", "TLS"),
    ] {
        let ran = run_main(&[path, url]);
        assert_eq!(ran.code, 1, "{url}");
        has(&ran.err, said);
        has(&ran.out, "key families (rows, digest):");
    }
}

/// Asserts that a state file that is not there fails with status 1 naming it, and prints no
/// report.
#[test]
fn a_state_file_that_is_not_there_fails_without_a_report() {
    let dir = scratch("refusal-missing");
    let missing = dir.join("missing.db");
    let ran = run_main(&[missing.to_str().expect("a UTF-8 path"), UNREACHABLE]);
    assert_eq!(ran.code, 1, "{}", ran.err);
    assert!(ran.out.is_empty(), "{}", ran.out);
    has(&ran.err, "missing.db");
}

/// Asserts that a report that cannot be written fails the run at once, before the target is
/// contacted: the failure is the writer's, not a refused connection.
#[test]
fn an_output_that_cannot_be_written_fails_the_run_before_the_target_is_contacted() {
    let dir = scratch("refusal-closed");
    let db = golden_copy(&dir);
    let arguments = args(&db, UNREACHABLE, false);
    let result = kbf_state_copy::run(&arguments, &Seams::default(), &mut Closed);
    let error = result.expect_err("the report cannot be written");
    assert_eq!(error.code(), 1);
    assert_eq!(error.to_string(), "failed: the pipe is closed");
}
