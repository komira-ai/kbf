//! The cutover against a real Postgres server: that a dry run changes no file, that a
//! cutover writes the new URL and keeps the original byte for byte, that a copy which does
//! not verify leaves the setting as it was, and that a rollback puts the original back.
//!
//! The tests are `#[ignore]`d so `cargo test --workspace` stays hermetic; the Postgres jobs
//! of CI run them with `--include-ignored` against a service container.
//! `KBF_TEST_POSTGRES_URL` names a server on which they may make and drop databases, each
//! test in a database of its own. That URL carries a password, which the setting may not,
//! so each test gives the setting the URL of `FRONTEND_URL` with `--frontend-url`.

mod support;

use std::io;
use std::process::{Command, Output};

use kbf_state_copy::{CutoverArgs, Seams, Stage};
use support::{
    Database, FRONTEND_URL, Farm, backup_of, config_place, count, cutover_args, has, lacks, read,
    read_text, run_cutover, run_rollback, shape, sqlite_url, starts, unit_for, unit_place,
    write_with_mode,
};

fn inactive(_unit: &str) -> io::Result<String> {
    Ok("inactive".to_owned())
}

fn change_a_value(stage: Stage) -> Option<&'static str> {
    let sql = "UPDATE kv SET value = decode('00', 'hex') WHERE key = 'op/0000000000000000'";
    (stage == Stage::InTransaction).then_some(sql)
}

fn change_a_version_after_the_commit(stage: Stage) -> Option<&'static str> {
    let sql = "UPDATE kv SET version = version + 1 WHERE key = 'meta/epoch'";
    (stage == Stage::AfterCommit).then_some(sql)
}

/// A cutover of the farm's state file into `target`, with the setting getting the URL the
/// frontend uses.
fn args_to(farm: &Farm, target: &Database, apply: bool) -> CutoverArgs {
    let mut args = cutover_args(&farm.db, &target.url(), config_place(&farm.config), apply);
    args.frontend_url = Some(FRONTEND_URL.to_owned());
    args
}

/// Asserts that a dry run against an empty target at the current schema is accepted and
/// reports the setting, the new value and every key family, and that it changes nothing: not
/// the kbf.toml, not the state file, not the directory, not the target.
#[test]
#[ignore = "needs a Postgres server (KBF_TEST_POSTGRES_URL); the CI Postgres jobs run it"]
fn a_dry_run_changes_no_file() {
    let farm = Farm::golden("cutover-pg-dry");
    let bytes = read(&farm.db);
    let before = farm.names();
    let target = Database::with_schema();
    let args = args_to(&farm, &target, false);
    let (result, report) = run_cutover(&args, &Seams::default());
    result.expect("a dry run is accepted");
    has(&report, "kbf-state-copy cutover (dry run)");
    has(&report, "then: postgres:///kbf?host=/run/postgresql&user=kbf");
    has(&report, "key families (rows, digest):");
    has(&report, "dry run: no file was changed");
    lacks(&report, "switched");
    assert_eq!(farm.config_now(), farm.text);
    assert_eq!(read(&farm.db), bytes);
    assert_eq!(farm.names(), before);
    assert_eq!(shape(&mut target.client()), [0, 0, 0, 0]);
}

/// Asserts that a cutover copies the rows, writes the frontend's URL as the one change to
/// the kbf.toml (every other byte as it was), keeps the original byte for byte as
/// `kbf.toml.pre-postgres`, leaves the state file as it was, and makes no other file.
#[test]
#[ignore = "needs a Postgres server (KBF_TEST_POSTGRES_URL); the CI Postgres jobs run it"]
fn a_cutover_writes_the_new_url_and_keeps_the_original() {
    let farm = Farm::golden("cutover-pg-apply");
    let bytes = read(&farm.db);
    let before = farm.names();
    let target = Database::with_schema();
    let args = args_to(&farm, &target, true);
    let (result, report) = run_cutover(&args, &Seams::default());
    result.expect("the cutover is accepted");
    let expected = farm.text.replace(&sqlite_url(&farm.db), FRONTEND_URL);
    assert_eq!(farm.config_now(), expected);
    assert_eq!(read(&farm.backup()), farm.text.as_bytes());
    assert_eq!(farm.names().len(), before.len() + 1);
    assert_eq!(read(&farm.db), bytes);
    assert_eq!(count(&mut target.client(), "kv"), 14);
    has(&report, "10 families match");
    has(&report, "switched state-store in [scheduler] in ");
    has(&report, "the original is kept byte for byte as ");
    has(&report, "next: start the frontend");
}

/// Asserts that a cutover of a unit file, with the unit reported inactive, rewrites the
/// `--state-store=` flag and nothing else, keeps the original byte for byte beside it, and
/// says to reload systemd.
#[test]
#[ignore = "needs a Postgres server (KBF_TEST_POSTGRES_URL); the CI Postgres jobs run it"]
fn a_cutover_of_a_unit_file_rewrites_its_flag_and_keeps_the_original() {
    let farm = Farm::golden("cutover-pg-unit");
    let file = farm.dir.join("override.conf");
    let original = unit_for(&farm.db);
    write_with_mode(&file, &original, 0o644);
    let target = Database::with_schema();
    let mut args = cutover_args(&farm.db, &target.url(), unit_place(&file), true);
    args.frontend_url = Some(FRONTEND_URL.to_owned());
    args.unit = Some("kbf-frontend.service".to_owned());
    let seams = Seams {
        unit_state: inactive,
        ..Seams::default()
    };
    let (result, report) = run_cutover(&args, &seams);
    result.expect("the cutover is accepted");
    let expected = original.replace(&sqlite_url(&farm.db), FRONTEND_URL);
    assert_eq!(read_text(&file), expected);
    assert_eq!(read(&backup_of(&file)), original.as_bytes());
    assert_eq!(count(&mut target.client(), "kv"), 14);
    has(&report, "unit kbf-frontend.service: stopped");
    has(&report, "next: run systemctl daemon-reload");
}

/// Asserts that a copy whose rows do not read back as the file's, found before the commit,
/// fails the cutover with status 1, rolls the copy back, and leaves the kbf.toml, the state
/// file and the directory as they were (no backup, no temporary file).
#[test]
#[ignore = "needs a Postgres server (KBF_TEST_POSTGRES_URL); the CI Postgres jobs run it"]
fn a_check_that_fails_before_the_commit_leaves_the_config_unchanged() {
    let farm = Farm::golden("cutover-pg-check");
    let bytes = read(&farm.db);
    let before = farm.names();
    let target = Database::with_schema();
    let seams = Seams {
        tamper: change_a_value,
        ..Seams::default()
    };
    let (result, report) = run_cutover(&args_to(&farm, &target, true), &seams);
    let error = result.expect_err("the check fails");
    assert_eq!(error.code(), 1);
    has(&error.to_string(), "key family op/");
    lacks(&report, "switched");
    assert_eq!(farm.config_now(), farm.text);
    assert_eq!(farm.names(), before);
    assert_eq!(read(&farm.db), bytes);
    assert_eq!(shape(&mut target.client()), [0, 0, 0, 0]);
}

/// Asserts that a copy that does not read back as the file's on a new connection after the
/// commit fails the cutover with status 1 saying the target holds a copy that does not
/// match, and leaves the kbf.toml, the state file and the directory as they were.
#[test]
#[ignore = "needs a Postgres server (KBF_TEST_POSTGRES_URL); the CI Postgres jobs run it"]
fn a_check_that_fails_after_the_commit_leaves_the_config_unchanged() {
    let farm = Farm::golden("cutover-pg-after");
    let bytes = read(&farm.db);
    let before = farm.names();
    let target = Database::with_schema();
    let seams = Seams {
        tamper: change_a_version_after_the_commit,
        ..Seams::default()
    };
    let (result, report) = run_cutover(&args_to(&farm, &target, true), &seams);
    let error = result.expect_err("the check fails");
    assert_eq!(error.code(), 1);
    let said = error.to_string();
    has(&said, "key family meta/");
    has(&said, "the target holds a copy that does not match");
    lacks(&report, "switched");
    assert_eq!(farm.config_now(), farm.text);
    assert_eq!(farm.names(), before);
    assert_eq!(read(&farm.db), bytes);
    assert_eq!(count(&mut target.client(), "kv"), 14);
}

/// Asserts that a rollback after a cutover puts the kbf.toml back byte for byte, removes the
/// backup, and leaves the state file as it was.
#[test]
#[ignore = "needs a Postgres server (KBF_TEST_POSTGRES_URL); the CI Postgres jobs run it"]
fn a_rollback_after_a_cutover_restores_the_original_byte_for_byte() {
    let farm = Farm::golden("cutover-pg-rollback");
    let bytes = read(&farm.db);
    let before = farm.names();
    let target = Database::with_schema();
    let (result, _report) = run_cutover(&args_to(&farm, &target, true), &Seams::default());
    result.expect("the cutover is accepted");
    assert_ne!(farm.config_now(), farm.text);
    let place = config_place(&farm.config);
    let (result, _report) = run_rollback(place, None, true, &Seams::default());
    result.expect("the rollback is accepted");
    assert_eq!(read(&farm.config), farm.text.as_bytes());
    assert_eq!(farm.names(), before);
    assert_eq!(read(&farm.db), bytes);
}

fn tool(argv: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_kbf-state-copy"))
        .args(argv)
        .output()
        .expect("run kbf-state-copy")
}

/// Asserts that the binary dry-runs a cutover with status 0 and changes nothing, switches
/// with status 0, refuses a second switch with status 2 (the backup is there) before it
/// copies, rolls back with status 0, and refuses a second rollback with status 2.
#[test]
#[ignore = "needs a Postgres server (KBF_TEST_POSTGRES_URL); the CI Postgres jobs run it"]
fn the_binary_switches_refuses_a_second_switch_and_rolls_back() {
    let farm = Farm::golden("cutover-pg-binary");
    let target = Database::with_schema();
    let db = farm.db.to_str().expect("a UTF-8 path");
    let config = farm.config.to_str().expect("a UTF-8 path");
    let url = target.url();
    let frontend = format!("--frontend-url={FRONTEND_URL}");
    let mut cutover = vec!["cutover", db, url.as_str()];
    cutover.extend(["--config", config, frontend.as_str()]);
    let dry = tool(&cutover);
    assert_eq!(dry.status.code(), Some(0));
    let said = String::from_utf8_lossy(&dry.stdout);
    starts(&said, "kbf-state-copy cutover (dry run)\n");
    assert_eq!(farm.config_now(), farm.text);
    assert_eq!(shape(&mut target.client()), [0, 0, 0, 0]);
    cutover.push("--apply");
    let switched = tool(&cutover);
    assert_eq!(switched.status.code(), Some(0));
    let said = String::from_utf8_lossy(&switched.stdout);
    has(&said, "switched state-store in [scheduler]");
    has(&farm.config_now(), FRONTEND_URL);
    lacks(&farm.config_now(), "sqlite://");
    let again = tool(&cutover);
    assert_eq!(again.status.code(), Some(2));
    let said = String::from_utf8_lossy(&again.stderr);
    starts(&said, "kbf-state-copy: refused: ");
    has(&said, "pre-postgres exists");
    let rollback = ["rollback", "--config", config, "--apply"];
    assert_eq!(tool(&rollback).status.code(), Some(0));
    assert_eq!(read(&farm.config), farm.text.as_bytes());
    assert_eq!(tool(&rollback).status.code(), Some(2));
}
