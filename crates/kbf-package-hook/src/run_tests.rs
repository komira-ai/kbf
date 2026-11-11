//! The arguments, the params file, the record directory and its lock.

use std::ffi::OsString;
use std::fs::File;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};

use kbf_testkit::dir::TestDir;
use update_hook::{Outcome, Params, Report, Scalar};

use crate::host::UnitState;
use crate::record::{FILE, Record, SCHEMA, Store, StoreError};
use crate::run::{Settings, run};
use crate::testing::MemHost;

fn scratch(name: &str) -> TestDir {
    TestDir::under(std::env::temp_dir(), name)
}

fn go(host: &MemHost, args: &[&str]) -> (Outcome, Report) {
    let args: Vec<OsString> = args.iter().map(OsString::from).collect();
    let settings = Settings {
        host,
        before_write: None,
    };
    let mut out = Vec::new();
    let outcome = run(&args, &settings, &mut out);
    let line = String::from_utf8(out).expect("utf-8");
    (
        outcome,
        Report::from_stdout(&line).unwrap_or_else(|e| panic!("{e}: {line}")),
    )
}

fn params_text(unit: &str, mode: &str, state_dir: &Path) -> String {
    let mut params = Params::default();
    for (key, value) in [
        ("unit", unit.to_owned()),
        ("mode", mode.to_owned()),
        ("state_dir", state_dir.display().to_string()),
        ("updater.target_version", "2.0.0".to_owned()),
    ] {
        params.insert(key, Scalar::Text(value));
    }
    params.to_toml()
}

fn host() -> MemHost {
    MemHost::new(UnitState::Active, "1.0.0", "2.0.0")
}

/// Command lines the updater would never give end with exit 1, `bad_arguments` and a last line.
#[test]
fn bad_command_lines_are_refused_with_the_contracts_last_line() {
    let host = host();
    for args in [
        &[][..],
        &["frobnicate"],
        &["verify"],
        &["verify", "--params"],
        &["prepare", "--params", "x"],
    ] {
        let (outcome, report) = go(&host, args);
        assert_eq!(outcome, Outcome::Failure, "{args:?}");
        assert_eq!(report.reason.as_deref(), Some("bad_arguments"), "{args:?}");
    }
    assert!(host.calls().is_empty());
}

/// A params file that is missing, or not TOML, is `bad_params`.
#[test]
fn an_unreadable_params_file_is_bad_params() {
    let dir = scratch("package-hook-run-params");
    let host = host();
    let missing = dir.join("missing.toml").display().to_string();
    let (outcome, report) = go(&host, &["verify", "--params", &missing]);
    assert_eq!(outcome, Outcome::Failure);
    assert_eq!(report.reason.as_deref(), Some("bad_params"));
    let broken = dir.join("broken.toml");
    std::fs::write(&broken, "this is = = not toml").expect("write");
    let (_, report) = go(
        &host,
        &["verify", "--params", &broken.display().to_string()],
    );
    assert_eq!(report.reason.as_deref(), Some("bad_params"));
}

/// Each manifest param is required, and the unit and the mode must look like names: nothing
/// starting with `-`, no space, no path, reaches systemctl.
#[test]
fn the_manifest_params_are_required_and_checked() {
    let dir = scratch("package-hook-run-manifest");
    let host = host();
    let state = dir.join("state");
    let file = dir.join("params.toml");
    let cases: [(&str, &str, bool); 8] = [
        ("kbf.service", "storage-and-daemon", true),
        ("kbf.service", "storage2", true),
        ("-kbf.service", "storage-and-daemon", false),
        ("kbf service", "storage-and-daemon", false),
        ("../x.service", "storage-and-daemon", false),
        ("kbf.service", "--check", false),
        ("kbf.service", "Mode", false),
        ("kbf.service", "", false),
    ];
    for (unit, mode, good) in cases {
        std::fs::write(&file, params_text(unit, mode, &state)).expect("write");
        let (outcome, report) = go(&host, &["verify", "--params", &file.display().to_string()]);
        assert_eq!(outcome, Outcome::Failure);
        // A good manifest gets as far as the record, which is not there.
        let want = if good { "not_prepared" } else { "bad_params" };
        assert_eq!(report.reason.as_deref(), Some(want), "{unit:?} {mode:?}");
    }
    let mut params = Params::default();
    params.insert("unit", Scalar::Text("kbf.service".to_owned()));
    params.insert("mode", Scalar::Bool(true));
    std::fs::write(&file, params.to_toml()).expect("write");
    let (_, report) = go(&host, &["verify", "--params", &file.display().to_string()]);
    assert_eq!(report.reason.as_deref(), Some("bad_params"));
    params.insert("mode", Scalar::Text("m".to_owned()));
    std::fs::write(&file, params.to_toml()).expect("write");
    let (_, report) = go(&host, &["verify", "--params", &file.display().to_string()]);
    assert_eq!(report.reason.as_deref(), Some("bad_params"));
    params.insert("unit", Scalar::Bool(false));
    params.insert("state_dir", Scalar::Text("s".to_owned()));
    std::fs::write(&file, params.to_toml()).expect("write");
    let (_, report) = go(&host, &["verify", "--params", &file.display().to_string()]);
    assert_eq!(report.reason.as_deref(), Some("bad_params"));
    assert!(host.calls().is_empty());
}

/// A record directory that cannot be made is the environment's fault; one that is missing is
/// made, and nothing outside it is.
#[test]
fn the_record_directory_is_made_or_its_failure_named() {
    let dir = scratch("package-hook-run-dir");
    let host = host();
    let file = dir.join("params.toml");
    let blocked = dir.join("a-file");
    std::fs::write(&blocked, "x").expect("write");
    std::fs::write(&file, params_text("kbf.service", "m", &blocked)).expect("write");
    let (outcome, report) = go(&host, &["verify", "--params", &file.display().to_string()]);
    assert_eq!(outcome, Outcome::Failure);
    assert_eq!(report.reason.as_deref(), Some("record_unusable"));

    let made = dir.join("made").join("state");
    std::fs::write(&file, params_text("kbf.service", "m", &made)).expect("write");
    go(&host, &["verify", "--params", &file.display().to_string()]);
    assert!(made.is_dir());
}

/// With the record directory locked by another copy, every step exits 1 as `busy`, the class of
/// the environment, and changes nothing.
#[test]
fn a_step_refuses_while_another_copy_holds_the_record_directory() {
    let dir = scratch("package-hook-run-lock");
    let host = host();
    let state = dir.join("state");
    std::fs::create_dir_all(&state).expect("dir");
    let file = dir.join("params.toml");
    std::fs::write(&file, params_text("kbf.service", "m", &state)).expect("write");
    let held = File::open(&state).expect("open");
    held.try_lock().expect("lock");
    let params = file.display().to_string();
    for step in ["cutover", "verify", "rollback", "cleanup"] {
        let (outcome, report) = go(&host, &[step, "--params", &params]);
        assert_eq!(outcome, Outcome::Failure, "{step}");
        assert_eq!(report.reason.as_deref(), Some("busy"), "{step}");
        assert_eq!(report.class, update_hook::Class::Environment);
    }
    assert!(host.calls().is_empty());
    assert!(!state.join(FILE).exists());
    drop(held);
    let (_, report) = go(&host, &["verify", "--params", &params]);
    assert_eq!(report.reason.as_deref(), Some("not_prepared"));
}

/// A record that is not the hook's, or is from a newer hook, is not touched and the step fails.
#[test]
fn a_corrupt_or_newer_record_is_refused_and_left_as_it_is() {
    let dir = scratch("package-hook-run-corrupt");
    let host = host();
    let state = dir.join("state");
    std::fs::create_dir_all(&state).expect("dir");
    let file = dir.join("params.toml");
    std::fs::write(&file, params_text("kbf.service", "m", &state)).expect("write");
    let params = file.display().to_string();
    for text in [
        "{ not json".to_owned(),
        format!("{{\"schema\":{}}}", SCHEMA + 1),
    ] {
        std::fs::write(state.join(FILE), &text).expect("write");
        let (outcome, report) = go(&host, &["rollback", "--params", &params]);
        assert_eq!(outcome, Outcome::Failure);
        assert_eq!(report.reason.as_deref(), Some("record_unusable"));
        assert_eq!(
            std::fs::read_to_string(state.join(FILE)).expect("read"),
            text
        );
    }
}

static GATED: AtomicUsize = AtomicUsize::new(0);

fn count() {
    GATED.fetch_add(1, Ordering::SeqCst);
}

/// The record is written whole and read back; the gate is called before each change (a save
/// and a removal) and a removal of a record that is not there is no error.
#[test]
fn the_record_round_trips_and_the_gate_runs_before_each_change() {
    let dir = scratch("package-hook-record");
    let store = Store::new(dir.join("state"), Some(count));
    store.ensure().expect("ensure");
    store.ensure().expect("ensure again");
    assert_eq!(store.load().expect("load"), None);
    let mut record = Record::new("2.0.0", "d", true);
    record.was_active = true;
    store.save(&record).expect("save");
    assert_eq!(store.load().expect("load"), Some(record.clone()));
    assert!(!dir.join("state").join("record.json.tmp").exists());
    store.remove().expect("remove");
    store.remove().expect("remove again");
    assert_eq!(store.load().expect("load"), None);
    assert_eq!(GATED.load(Ordering::SeqCst), 3);
    assert!(record.is_of("2.0.0", "d") && !record.is_of("2.0.0", "e"));
}

/// A record directory that is not there cannot be locked, saved to, or removed from, and the
/// errors name what failed.
#[test]
fn a_record_directory_that_is_not_there_is_an_error_naming_it() {
    let dir = scratch("package-hook-record-missing");
    let store = Store::new(dir.join("absent"), None);
    assert!(matches!(store.lock(), Err(StoreError::Io { .. })));
    assert!(matches!(
        store.save(&Record::default()),
        Err(StoreError::Io { .. })
    ));
    assert_eq!(store.load().expect("no record"), None);
    assert!(store.remove().is_err());
    let text = store.lock().expect_err("no directory").to_string();
    assert!(text.contains("absent"), "{text}");
    let blocked = Store::new(dir.join("file"), None);
    std::fs::write(dir.join("file"), "x").expect("write");
    assert!(blocked.ensure().is_err());
    // A path that is a directory where the record file should be cannot be read as one.
    let odd = Store::new(dir.join("odd"), None);
    std::fs::create_dir_all(dir.join("odd").join(FILE)).expect("dir");
    assert!(matches!(odd.load(), Err(StoreError::Io { .. })));
    assert!(odd.remove().is_err());
}

/// The real entry point answers a command line with no step with exit 1.
#[test]
fn the_entry_point_refuses_a_missing_step() {
    assert_eq!(crate::main(Vec::new()), std::process::ExitCode::from(1));
}
