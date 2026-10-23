//! The `cutover` and `rollback` commands up to the point where a database is needed: what
//! they refuse before they touch a file, that a copy which fails leaves every file as it was,
//! what a rollback does, and the command line. The cutover that succeeds is in
//! `cutover_postgres.rs`.

mod support;

use std::fs;
use std::io;

use kbf_state_copy::{CutoverArgs, Error, PlaceArgs, Seams, unit};
use support::{
    FRONTEND_URL, Farm, UNREACHABLE, backup_of, built, config_for, config_place, config_with,
    cutover_args, has, lacks, mode_of, open, read, read_text, run_cutover, run_main, run_rollback,
    scratch, sqlite_url, starts, temp_of, unit_for, unit_place, write_with_mode,
};

type Ask = fn(&str) -> io::Result<String>;

const UNIT: &str = "kbf-frontend.service";

fn active(_unit: &str) -> io::Result<String> {
    Ok("active".to_owned())
}

fn activating(_unit: &str) -> io::Result<String> {
    Ok("activating".to_owned())
}

fn silent(_unit: &str) -> io::Result<String> {
    Ok(String::new())
}

fn inactive(_unit: &str) -> io::Result<String> {
    Ok("inactive".to_owned())
}

fn failed(_unit: &str) -> io::Result<String> {
    Ok("failed".to_owned())
}

fn unaskable(_unit: &str) -> io::Result<String> {
    Err(io::Error::other("no systemd here"))
}

fn asking(unit_state: Ask) -> Seams {
    Seams {
        unit_state,
        ..Seams::default()
    }
}

/// A cutover of the farm's state file to a target nothing listens on.
fn args_for(farm: &Farm, apply: bool) -> CutoverArgs {
    cutover_args(&farm.db, UNREACHABLE, config_place(&farm.config), apply)
}

fn with_unit(farm: &Farm, apply: bool) -> CutoverArgs {
    let mut args = args_for(farm, apply);
    args.unit = Some(UNIT.to_owned());
    args
}

/// Asserts that a frontend that has the state file open is refused with status 2, for a dry
/// run and for a copy, naming the lock, and that no file is made or changed.
#[test]
fn a_frontend_that_holds_the_lock_is_refused_and_no_file_changes() {
    let dir = scratch("cutover-lock");
    let db = built(&dir).db;
    let farm = Farm::new(dir, db);
    let store = open(&farm.db);
    let before = farm.names();
    for apply in [false, true] {
        let args = args_for(&farm, apply);
        let (result, _report) = run_cutover(&args, &Seams::default());
        let error = result.expect_err("a running frontend is refused");
        assert_eq!(error.code(), 2);
        has(&error.to_string(), "state.db.lock");
        assert_eq!(farm.names(), before);
        assert_eq!(farm.config_now(), farm.text);
    }
    drop(store);
}

/// Asserts that a unit systemd does not call stopped (running, starting, or one it says
/// nothing of) is refused with status 2 naming the unit, for a dry run and for a copy, and
/// that no file is made or changed.
#[test]
fn a_unit_that_is_not_stopped_is_refused_and_no_file_changes() {
    let farm = Farm::golden("cutover-unit");
    let before = farm.names();
    let asks: [Ask; 3] = [active, activating, silent];
    for ask in asks {
        for apply in [false, true] {
            let args = with_unit(&farm, apply);
            let (result, report) = run_cutover(&args, &asking(ask));
            let error = result.expect_err("the unit is not stopped");
            assert_eq!(error.code(), 2);
            has(&error.to_string(), UNIT);
            has(&error.to_string(), "stop it first");
            lacks(&report, "stopped");
            assert_eq!(farm.names(), before);
            assert_eq!(farm.config_now(), farm.text);
        }
    }
}

/// Asserts that a unit that is inactive, or failed, passes the check: the run goes on, says
/// the unit is stopped, and ends at the target that cannot be reached with status 1.
#[test]
fn a_stopped_unit_goes_on_to_the_copy() {
    let farm = Farm::golden("cutover-stopped");
    let asks: [Ask; 2] = [inactive, failed];
    for ask in asks {
        let args = with_unit(&farm, false);
        let (result, report) = run_cutover(&args, &asking(ask));
        let error = result.expect_err("the target cannot be reached");
        assert_eq!(error.code(), 1);
        starts(&error.to_string(), "failed: ");
        has(&report, "unit kbf-frontend.service: stopped");
    }
    assert_eq!(farm.config_now(), farm.text);
}

/// Asserts that a systemd that cannot be asked fails the run with status 1 naming the unit
/// and the reason, and that no file changes.
#[test]
fn a_systemd_that_cannot_be_asked_fails_the_run() {
    let farm = Farm::golden("cutover-no-systemd");
    let before = farm.names();
    let args = with_unit(&farm, true);
    let (result, _report) = run_cutover(&args, &asking(unaskable));
    let error = result.expect_err("systemd cannot be asked");
    assert_eq!(error.code(), 1);
    let said = error.to_string();
    has(&said, "could not ask systemd about");
    has(&said, UNIT);
    has(&said, "no systemd here");
    assert_eq!(farm.names(), before);
}

fn kind_of(asked: io::Result<String>) -> Result<String, io::ErrorKind> {
    asked.map_err(|e| e.kind())
}

/// Asserts that the default way of asking systemd about a unit is `unit::query` on
/// `systemctl`: for a name that no host has as a unit, the default gives the answer that the
/// direct call gives (or an error of the same kind, where there is no `systemctl`).
#[test]
fn the_default_check_of_a_unit_asks_systemctl() {
    let name = "kbf-state-copy-test-unit.service";
    let default = kind_of((Seams::default().unit_state)(name));
    let direct = kind_of(unit::query("systemctl", name));
    assert_eq!(default, direct);
}

const ASKED: &str = "show --property=LoadState,ActiveState -- kbf-frontend.service";

/// Asserts that the question put to systemd is `show --property=LoadState,ActiveState --
/// <unit>`, that what the program prints is read as `state_of` reads it, and that a program
/// that is not there is an error.
#[test]
fn the_question_put_to_systemd_is_show_and_the_unit_name() {
    let said = unit::shown("echo", UNIT).expect("echo runs");
    assert_eq!(said, ASKED);
    let state = unit::query("echo", UNIT).expect("echo runs");
    assert!(state.is_empty(), "{state}");
    let error = unit::query("/nonexistent/systemctl", UNIT).expect_err("no such program");
    assert_eq!(error.kind(), io::ErrorKind::NotFound);
}

/// Asserts that a listing from `systemctl show` is read as the unit's active state when the
/// unit is loaded (in either order of its lines), as its load state when it is not, and as
/// the empty word when a line it needs is not there.
#[test]
fn a_listing_is_read_as_the_state_of_a_unit_that_is_loaded() {
    let cases = [
        ("LoadState=loaded\nActiveState=inactive", "inactive"),
        ("ActiveState=failed\nLoadState=loaded\n", "failed"),
        ("ActiveState=inactive\nLoadState=not-found", "not-found"),
        ("ActiveState=active\nLoadState=masked", "masked"),
        ("ActiveState=active", ""),
        ("LoadState=loaded", ""),
    ];
    for (listing, state) in cases {
        assert_eq!(unit::state_of(listing), state, "{listing:?}");
    }
}

fn not_found(_unit: &str) -> io::Result<String> {
    Ok("not-found".to_owned())
}

/// Asserts that a name that systemd has no unit for is refused with status 2 saying so and
/// naming the unit, for a dry run and for a copy, and that no file is made or changed.
#[test]
fn a_unit_that_does_not_exist_is_refused_and_no_file_changes() {
    let farm = Farm::golden("cutover-no-unit");
    let before = farm.names();
    for apply in [false, true] {
        let args = with_unit(&farm, apply);
        let (result, report) = run_cutover(&args, &asking(not_found));
        let error = result.expect_err("there is no such unit");
        assert_eq!(error.code(), 2);
        let said = error.to_string();
        has(&said, "systemd has no unit kbf-frontend.service");
        lacks(&report, "stopped");
        assert_eq!(farm.names(), before);
        assert_eq!(farm.config_now(), farm.text);
    }
}

/// Asserts that a backup of an earlier cutover is refused with status 2, for a dry run and
/// for a copy, that it is left as it was, and that no file is made or changed.
#[test]
fn an_existing_backup_is_refused_and_left_alone() {
    let farm = Farm::golden("cutover-backup");
    let backup = farm.backup();
    fs::write(&backup, "an earlier original").expect("write the backup");
    let before = farm.names();
    for apply in [false, true] {
        let args = args_for(&farm, apply);
        let (result, _report) = run_cutover(&args, &Seams::default());
        let error = result.expect_err("an existing backup is refused");
        assert_eq!(error.code(), 2);
        has(&error.to_string(), "pre-postgres exists");
        assert_eq!(read_text(&backup), "an earlier original");
        assert_eq!(farm.names(), before);
        assert_eq!(farm.config_now(), farm.text);
    }
}

/// Asserts that a temporary file left by an interrupted cutover is refused with status 2, for
/// a dry run and for a copy, that it is left as it was, and that no file is made or changed.
#[test]
fn a_file_left_by_an_interrupted_cutover_is_refused_and_left_alone() {
    let farm = Farm::golden("cutover-leftover");
    let leftover = temp_of(&farm.config);
    fs::write(&leftover, "left by a crash").expect("write the leftover");
    let before = farm.names();
    for apply in [false, true] {
        let args = args_for(&farm, apply);
        let (result, _report) = run_cutover(&args, &Seams::default());
        let error = result.expect_err("a leftover file is refused");
        assert_eq!(error.code(), 2);
        let said = error.to_string();
        has(&said, "postgres-new exists");
        assert_eq!(read_text(&leftover), "left by a crash");
        assert_eq!(farm.names(), before);
        assert_eq!(farm.config_now(), farm.text);
    }
}

fn url_error(farm: &Farm, postgres: &str, frontend: Option<&str>) -> (Error, String) {
    let mut args = args_for(farm, true);
    args.postgres_url = postgres.to_owned();
    args.frontend_url = frontend.map(str::to_owned);
    let (result, report) = run_cutover(&args, &Seams::default());
    (result.expect_err("the URL is refused"), report)
}

/// Asserts that a URL with a password, as the copy's or as the frontend's, is refused with
/// status 2 before anything is printed or done, without the password in the message; that
/// a URL the store does not take fails with status 1; and that no file changes.
#[test]
fn a_url_the_frontend_would_refuse_is_refused_before_any_work() {
    let farm = Farm::golden("cutover-url");
    let before = farm.names();
    let secret = "postgres://u:s3cret-pw@h/db";
    let (error, report) = url_error(&farm, secret, None);
    assert_eq!(error.code(), 2);
    has(&error.to_string(), "--frontend-url");
    lacks(&error.to_string(), "s3cret-pw");
    assert!(report.is_empty(), "{report}");
    let (error, report) = url_error(&farm, UNREACHABLE, Some(secret));
    assert_eq!(error.code(), 2);
    lacks(&error.to_string(), "s3cret-pw");
    assert!(report.is_empty(), "{report}");
    let (error, _report) = url_error(&farm, UNREACHABLE, Some("mysql://h/db"));
    assert_eq!(error.code(), 1);
    has(&error.to_string(), "must start with postgres://");
    assert_eq!(farm.names(), before);
    assert_eq!(farm.config_now(), farm.text);
}

/// Writes `text` as the farm's kbf.toml and runs a copying cutover; returns the refusal.
fn refused_with(farm: &Farm, text: &str) -> String {
    write_with_mode(&farm.config, text, 0o640);
    let (result, report) = run_cutover(&args_for(farm, true), &Seams::default());
    let error = result.expect_err("the setting is refused");
    assert_eq!(error.code(), 2);
    assert!(report.is_empty(), "{report}");
    error.to_string()
}

/// Asserts that a setting that is not the state file's is refused with status 2 before
/// anything is printed or made: one that names another file, a Postgres URL (which is not
/// quoted in the message), a file that does not exist, and a kbf.toml with no setting.
#[test]
fn a_setting_that_is_not_the_state_files_is_refused() {
    let farm = Farm::golden("cutover-setting");
    let other = farm.dir.join("other.db");
    fs::copy(&farm.db, &other).expect("copy the file");
    let before = farm.names();
    let said = refused_with(&farm, &config_for(&other));
    has(&said, "other.db, not ");
    let said = refused_with(&farm, &config_with("postgres://u:s3cret-pw@h/db"));
    has(&said, "not a sqlite:// URL");
    lacks(&said, "s3cret-pw");
    let said = refused_with(&farm, &config_with("sqlite:///no/such/state.db"));
    has(&said, "/no/such/state.db");
    let said = refused_with(&farm, "mode = \"frontend\"\n");
    has(&said, "has no [scheduler] state-store");
    assert_eq!(farm.names(), before);
}

/// Asserts that a setting file that is not there fails the run with status 1 naming it, and
/// that giving neither place, or both, is a mistake on the command line with status 1.
#[test]
fn a_missing_setting_file_and_a_missing_or_double_place_are_failures() {
    let farm = Farm::golden("cutover-place");
    let missing = farm.dir.join("missing.toml");
    let args = cutover_args(&farm.db, UNREACHABLE, config_place(&missing), true);
    let (result, _report) = run_cutover(&args, &Seams::default());
    let error = result.expect_err("no such file");
    assert_eq!(error.code(), 1);
    has(&error.to_string(), "missing.toml");
    let neither = PlaceArgs {
        config: None,
        unit_file: None,
    };
    let both = PlaceArgs {
        config: Some(missing.clone()),
        unit_file: Some(missing),
    };
    for place in [neither, both] {
        let error = place.resolve().expect_err("not exactly one place");
        assert_eq!(error.code(), 1);
        let said = error.to_string();
        has(&said, "give exactly one of --config and --unit-file");
    }
}

/// Asserts that a copy that fails (here, a target that cannot be reached) ends the run with
/// status 1 and leaves every file as it was: the kbf.toml and its mode, the state file, and
/// the directory, with no backup and no temporary file in it.
#[test]
fn a_copy_that_fails_leaves_every_file_as_it_was() {
    let farm = Farm::golden("cutover-failed");
    let bytes = read(&farm.db);
    let before = farm.names();
    let mode = mode_of(&farm.config);
    let (result, report) = run_cutover(&args_for(&farm, true), &Seams::default());
    let error = result.expect_err("the target cannot be reached");
    assert_eq!(error.code(), 1);
    has(&report, "now:  sqlite://");
    lacks(&report, "switched");
    assert_eq!(farm.names(), before);
    assert_eq!(farm.config_now(), farm.text);
    assert_eq!(mode_of(&farm.config), mode);
    assert_eq!(read(&farm.db), bytes);
}

/// Asserts that the same holds for a unit file: a copy that fails leaves the unit file and
/// the directory as they were.
#[test]
fn a_copy_that_fails_leaves_a_unit_file_as_it_was() {
    let farm = Farm::golden("cutover-failed-unit");
    let file = farm.dir.join("override.conf");
    let text = unit_for(&farm.db);
    write_with_mode(&file, &text, 0o644);
    let before = farm.names();
    let args = cutover_args(&farm.db, UNREACHABLE, unit_place(&file), true);
    let (result, report) = run_cutover(&args, &Seams::default());
    let error = result.expect_err("the target cannot be reached");
    assert_eq!(error.code(), 1);
    has(&report, "--state-store= flag in ");
    assert_eq!(read_text(&file), text);
    assert_eq!(mode_of(&file), 0o644);
    assert_eq!(farm.names(), before);
}

/// What a cutover leaves behind, made by hand: the original as the backup beside the
/// kbf.toml, and the Postgres text as the kbf.toml.
fn switched_farm(name: &str) -> (Farm, String) {
    let farm = Farm::golden(name);
    let switched = farm.text.replace(&sqlite_url(&farm.db), FRONTEND_URL);
    write_with_mode(&farm.backup(), &farm.text, 0o640);
    write_with_mode(&farm.config, &switched, 0o640);
    (farm, switched)
}

/// Asserts that a rollback with no backup is refused with status 2 naming it, for a dry run
/// and for a restore, before anything is printed, and that no file changes.
#[test]
fn a_rollback_without_a_backup_is_refused() {
    let farm = Farm::golden("rollback-none");
    let before = farm.names();
    for apply in [false, true] {
        let place = config_place(&farm.config);
        let (result, report) = run_rollback(place, None, apply, &Seams::default());
        let error = result.expect_err("there is no backup");
        assert_eq!(error.code(), 2);
        has(&error.to_string(), "pre-postgres does not exist");
        assert!(report.is_empty(), "{report}");
        assert_eq!(farm.names(), before);
        assert_eq!(farm.config_now(), farm.text);
    }
}

/// Asserts that a rollback dry run says what it would restore and changes no file.
#[test]
fn a_rollback_dry_run_changes_no_file() {
    let (farm, switched) = switched_farm("rollback-dry");
    let before = farm.names();
    let place = config_place(&farm.config);
    let (result, report) = run_rollback(place, None, false, &Seams::default());
    result.expect("a dry run is accepted");
    has(&report, "kbf-state-copy rollback (dry run)");
    has(&report, "restore: ");
    has(&report, "dry run: no file was changed");
    assert_eq!(farm.config_now(), switched);
    assert_eq!(read_text(&farm.backup()), farm.text);
    assert_eq!(farm.names(), before);
}

/// Asserts that a rollback puts the original kbf.toml back byte for byte (comments, a tab,
/// trailing spaces, a CR LF, no final newline) with its mode, removes the backup, and
/// leaves the state file as it was.
#[test]
fn a_rollback_restores_the_config_byte_for_byte() {
    let (farm, _switched) = switched_farm("rollback-apply");
    let bytes = read(&farm.db);
    let before = farm.names();
    let place = config_place(&farm.config);
    let (result, report) = run_rollback(place, None, true, &Seams::default());
    result.expect("the rollback is accepted");
    assert_eq!(read(&farm.config), farm.text.as_bytes());
    assert_eq!(mode_of(&farm.config), 0o640);
    assert!(!farm.backup().exists());
    assert_eq!(farm.names().len(), before.len() - 1);
    assert_eq!(read(&farm.db), bytes);
    has(&report, "restored ");
    has(&report, "next: start the frontend");
}

/// Asserts that a rollback of a unit file puts it back byte for byte and says to reload
/// systemd before starting the frontend.
#[test]
fn a_rollback_of_a_unit_file_says_to_reload_systemd() {
    let farm = Farm::golden("rollback-unit-file");
    let file = farm.dir.join("override.conf");
    let original = unit_for(&farm.db);
    let switched = original.replace(&sqlite_url(&farm.db), FRONTEND_URL);
    write_with_mode(&file, &switched, 0o644);
    write_with_mode(&backup_of(&file), &original, 0o644);
    let (result, report) = run_rollback(unit_place(&file), None, true, &Seams::default());
    result.expect("the rollback is accepted");
    assert_eq!(read_text(&file), original);
    has(&report, "next: run systemctl daemon-reload");
}

/// Asserts that a rollback with a unit that is running is refused with status 2 and changes
/// no file, and that one with a stopped unit goes ahead and says so.
#[test]
fn a_rollback_checks_the_unit_too() {
    let (farm, switched) = switched_farm("rollback-unit");
    let before = farm.names();
    let place = config_place(&farm.config);
    let (result, _report) = run_rollback(place, Some(UNIT), true, &asking(active));
    let error = result.expect_err("the unit is running");
    assert_eq!(error.code(), 2);
    has(&error.to_string(), UNIT);
    assert_eq!(farm.config_now(), switched);
    assert_eq!(farm.names(), before);
    let place = config_place(&farm.config);
    let (result, report) = run_rollback(place, Some(UNIT), true, &asking(inactive));
    result.expect("a stopped unit is accepted");
    has(&report, "unit kbf-frontend.service: stopped");
    assert_eq!(farm.config_now(), farm.text);
}

/// Asserts that the help of the tool lists both commands, and that the help of each names
/// its arguments and flags, states the exit status, ends well and offers no way to skip a
/// check.
#[test]
fn the_help_of_each_command_names_its_arguments() {
    let ran = run_main(&["--help"]);
    assert_eq!(ran.code, 0);
    has(&ran.out, "kbf-state-copy cutover");
    has(&ran.out, "kbf-state-copy rollback");
    let ran = run_main(&["cutover", "--help"]);
    assert_eq!(ran.code, 0);
    assert!(ran.err.is_empty(), "{}", ran.err);
    has(&ran.out, "Usage: kbf-state-copy cutover");
    for needle in ["<STATE_DB>", "<POSTGRES_URL>", "--config", "--unit-file"] {
        has(&ran.out, needle);
    }
    for needle in ["--unit <NAME>", "--frontend-url", "--apply"] {
        has(&ran.out, needle);
    }
    has(&ran.out, "Exit status: 0 done, 2 refused");
    lacks(&ran.out, "--force");
    let ran = run_main(&["rollback", "--help"]);
    assert_eq!(ran.code, 0);
    has(&ran.out, "Usage: kbf-state-copy rollback");
    has(&ran.out, "--config");
    has(&ran.out, "Exit status: 0 done, 2 refused");
    lacks(&ran.out, "--force");
}

/// Asserts that a mistake in a command is status 1 with the usage on the error stream and
/// nothing on the output: no arguments, a flag that does not exist, and no place for the
/// setting or two.
#[test]
fn a_mistake_in_a_command_is_status_1() {
    let ran = run_main(&["cutover"]);
    assert_eq!(ran.code, 1);
    assert!(ran.out.is_empty(), "{}", ran.out);
    has(&ran.err, "Usage: kbf-state-copy cutover");
    let ran = run_main(&["rollback", "--force"]);
    assert_eq!(ran.code, 1);
    has(&ran.err, "Usage: kbf-state-copy rollback");
    let neither = ["cutover", "s", "u"];
    let both = ["cutover", "s", "u", "--config=a", "--unit-file=b"];
    let rollback = ["rollback"];
    let cases: [&[&str]; 3] = [&neither, &both, &rollback];
    for argv in cases {
        let ran = run_main(argv);
        assert_eq!(ran.code, 1, "{argv:?}");
        assert!(ran.out.is_empty(), "{argv:?}: {}", ran.out);
        let said = "kbf-state-copy: failed: give exactly one of --config and --unit-file";
        starts(&ran.err, said);
    }
}
