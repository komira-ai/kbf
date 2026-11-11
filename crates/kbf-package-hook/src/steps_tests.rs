//! Each step over a host in memory and a real record directory.

use std::ffi::OsString;
use std::path::PathBuf;

use kbf_testkit::dir::TestDir;
use update_hook::{Class, Outcome, Params, Report, Scalar};

use crate::host::{Check, HostError, UnitState};
use crate::record::{Record, Store};
use crate::run::{Settings, run};
use crate::testing::MemHost;

const DIGEST: &str = "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

struct Rig {
    dir: TestDir,
    host: MemHost,
}

impl Rig {
    /// A unit in `state` running `1.0.0`, with `2.0.0` to be installed, and the params of an
    /// update.
    fn new(state: UnitState) -> Rig {
        let rig = Rig {
            dir: TestDir::under(std::env::temp_dir(), "package-hook-steps"),
            host: MemHost::new(state, "1.0.0", "2.0.0"),
        };
        rig.write(false);
        rig
    }

    fn staged(&self) -> PathBuf {
        self.dir.join("staged")
    }

    fn state_dir(&self) -> PathBuf {
        self.dir.join("state")
    }

    fn params_file(&self) -> PathBuf {
        self.dir.join("params.toml")
    }

    fn params(&self, first_install: bool) -> Params {
        let mut params = Params::default();
        let text = |s: &str| Scalar::Text(s.to_owned());
        params.insert("unit", text("kbf-storage-and-daemon.service"));
        params.insert("mode", text("storage-and-daemon"));
        params.insert("state_dir", text(&self.state_dir().display().to_string()));
        params.insert(
            "updater.staged_release",
            text(&self.staged().display().to_string()),
        );
        params.insert("updater.target_version", text("2.0.0"));
        params.insert("updater.artifact_digest", text(DIGEST));
        if !first_install {
            params.insert("updater.previous_release", text("/opt/previous"));
            params.insert("updater.previous_version", text("1.0.0"));
        }
        params
    }

    fn write(&self, first_install: bool) {
        self.write_params(&self.params(first_install));
    }

    fn write_params(&self, params: &Params) {
        std::fs::write(self.params_file(), params.to_toml()).expect("params");
    }

    fn args(&self, step: &str) -> Vec<OsString> {
        let mut args = vec![OsString::from(step)];
        if step == "prepare" {
            args.push(self.staged().into());
        }
        args.push("--params".into());
        args.push(self.params_file().into());
        args
    }

    fn run(&self, step: &str) -> (Outcome, Report) {
        self.run_args(&self.args(step))
    }

    fn run_args(&self, args: &[OsString]) -> (Outcome, Report) {
        let settings = Settings {
            host: &self.host,
            before_write: None,
        };
        let mut out = Vec::new();
        let outcome = run(args, &settings, &mut out);
        let line = String::from_utf8(out).expect("utf-8");
        let report = Report::from_stdout(&line).unwrap_or_else(|e| panic!("{e}: {line}"));
        (outcome, report)
    }

    fn ok(&self, step: &str) -> Report {
        let (outcome, report) = self.run(step);
        assert_eq!(outcome, Outcome::Success, "{step}: {report:?}");
        report
    }

    fn fails(&self, step: &str, reason: &str, class: Class) -> Report {
        let (outcome, report) = self.run(step);
        assert_eq!(outcome, Outcome::Failure, "{step}: {report:?}");
        assert_eq!(report.reason.as_deref(), Some(reason), "{report:?}");
        assert_eq!(report.class, class, "{report:?}");
        report
    }

    fn record(&self) -> Option<Record> {
        Store::new(self.state_dir(), None).load().expect("a record")
    }
}

/// An update of a running unit: `prepare` checks the staged binary, records that the unit was
/// active and stops it (the drain), and stopping is the last thing it does.
#[test]
fn prepare_checks_the_staged_binary_then_stops_the_running_unit() {
    let rig = Rig::new(UnitState::Active);
    rig.ok("prepare");
    let calls = rig.host.calls();
    assert_eq!(
        calls[0],
        format!(
            "check {}/tree/usr/bin/kbf storage-and-daemon",
            rig.staged().display()
        )
    );
    assert_eq!(calls[1..], ["state", "stop"]);
    let record = rig.record().expect("a record");
    assert!(record.was_active && record.prepared && !record.first_install);
    assert_eq!(
        (record.target.as_str(), record.digest.as_str()),
        ("2.0.0", DIGEST)
    );
    assert_eq!(*rig.host.state.borrow(), UnitState::Inactive);
}

/// A `prepare` run again does nothing: no check, no stop, the record as it was.
#[test]
fn prepare_run_again_changes_nothing() {
    let rig = Rig::new(UnitState::Active);
    rig.ok("prepare");
    let record = rig.record();
    rig.host.clear_calls();
    rig.ok("prepare");
    assert!(rig.host.calls().is_empty());
    assert_eq!(rig.record(), record);
}

/// On a first install (no previous release) `prepare` has no unit to drain and records it.
#[test]
fn prepare_on_a_first_install_has_nothing_to_stop() {
    let rig = Rig::new(UnitState::Inactive);
    rig.write(true);
    rig.ok("prepare");
    assert_eq!(rig.host.calls().last().map(String::as_str), Some("state"));
    let record = rig.record().expect("a record");
    assert!(record.first_install && !record.was_active && record.prepared);
}

/// An activating unit counts as running: it is stopped, and recorded as active.
#[test]
fn prepare_counts_an_activating_unit_as_running() {
    let rig = Rig::new(UnitState::Activating);
    rig.ok("prepare");
    assert!(rig.record().expect("a record").was_active);
    assert_eq!(*rig.host.state.borrow(), UnitState::Inactive);
}

/// A staged binary that refuses the configuration (exit 2) fails the release as
/// `config_refused`, and nothing changed: no record, the unit running.
#[test]
fn prepare_fails_the_release_when_the_staged_binary_refuses_the_configuration() {
    let rig = Rig::new(UnitState::Active);
    *rig.host.check.borrow_mut() = Ok(Check::Refused);
    let report = rig.fails("prepare", "config_refused", Class::Release);
    assert!(report.reversible);
    assert!(rig.record().is_none());
    assert_eq!(rig.host.calls().len(), 1);
    assert_eq!(*rig.host.state.borrow(), UnitState::Active);
}

/// Exit 75 (the old process still holds the port) passes the check and the unit is stopped.
#[test]
fn prepare_goes_on_when_the_check_says_not_ready_yet() {
    let rig = Rig::new(UnitState::Active);
    *rig.host.check.borrow_mut() = Ok(Check::NotReady);
    rig.ok("prepare");
    assert_eq!(*rig.host.state.borrow(), UnitState::Inactive);
}

/// Any other end of the check, and a host that cannot run it, is the environment's fault.
#[test]
fn prepare_blames_the_environment_when_the_check_cannot_run() {
    let rig = Rig::new(UnitState::Active);
    *rig.host.check.borrow_mut() = Ok(Check::Failed("exited 9".to_owned()));
    let report = rig.fails("prepare", "check_failed", Class::Environment);
    assert_eq!(report.detail.as_deref(), Some("exited 9"));
    *rig.host.check.borrow_mut() = Err(HostError("no host".to_owned()));
    rig.fails("prepare", "host_failed", Class::Environment);
    assert!(rig.record().is_none());
}

/// A staged path other than `updater.staged_release` is refused before anything runs.
#[test]
fn prepare_refuses_a_staged_path_that_differs_from_the_params() {
    let rig = Rig::new(UnitState::Active);
    let args: Vec<OsString> = vec![
        "prepare".into(),
        "/elsewhere".into(),
        "--params".into(),
        rig.params_file().into(),
    ];
    let (outcome, report) = rig.run_args(&args);
    assert_eq!(outcome, Outcome::Failure);
    assert_eq!(report.reason.as_deref(), Some("staged_mismatch"));
    assert!(rig.host.calls().is_empty());
    assert!(rig.record().is_none());
}

/// Params without a target version, or without a staged release, are refused as `bad_params`.
#[test]
fn prepare_needs_the_target_version_and_the_staged_release() {
    let rig = Rig::new(UnitState::Active);
    let mut params = Params::default();
    for (key, value) in [
        ("unit", "u.service"),
        ("mode", "m"),
        ("state_dir", &rig.state_dir().display().to_string()),
    ] {
        params.insert(key, Scalar::Text(value.to_owned()));
    }
    rig.write_params(&params);
    rig.fails("prepare", "bad_params", Class::Release);
    params.insert("updater.target_version", Scalar::Text("2.0.0".to_owned()));
    rig.write_params(&params);
    let report = rig.fails("prepare", "bad_params", Class::Release);
    assert_eq!(report.detail.as_deref(), Some("no updater.staged_release"));
}

/// A `prepare` killed between the record and the stop is run again: the unit is stopped, and
/// the record still says it was active, though it is not now.
#[test]
fn prepare_resumed_keeps_what_the_first_run_read() {
    let rig = Rig::new(UnitState::Active);
    let mut record = Record::new("2.0.0", DIGEST, false);
    record.was_active = true;
    std::fs::create_dir_all(rig.state_dir()).expect("dir");
    Store::new(rig.state_dir(), None)
        .save(&record)
        .expect("save");
    // The unit was stopped by the run that died, and nothing else happened.
    *rig.host.state.borrow_mut() = UnitState::Inactive;
    rig.ok("prepare");
    let record = rig.record().expect("a record");
    assert!(record.was_active && record.prepared);
    assert!(!rig.host.calls().contains(&"stop".to_owned()));
}

/// A new `prepare` of the release a rollback undid starts over, as does one for another update.
#[test]
fn prepare_after_a_rollback_or_for_another_update_starts_a_new_record() {
    let rig = Rig::new(UnitState::Active);
    rig.ok("prepare");
    rig.ok("rollback");
    assert!(rig.record().expect("a record").rolled_back);
    *rig.host.state.borrow_mut() = UnitState::Active;
    rig.ok("prepare");
    let record = rig.record().expect("a record");
    assert!(!record.rolled_back && record.prepared && record.was_active);

    let mut other = Record::new("1.5.0", DIGEST, false);
    other.prepared = true;
    Store::new(rig.state_dir(), None)
        .save(&other)
        .expect("save");
    *rig.host.state.borrow_mut() = UnitState::Inactive;
    rig.ok("prepare");
    let record = rig.record().expect("a record");
    assert_eq!(record.target, "2.0.0");
    assert!(!record.was_active);
}

/// A failure of the host while reading the state or stopping the unit ends `prepare` as the
/// environment's, and the record shows how far it got.
#[test]
fn prepare_reports_a_host_that_cannot_stop_the_unit() {
    let rig = Rig::new(UnitState::Active);
    rig.host.fail("stop");
    rig.fails("prepare", "host_failed", Class::Environment);
    let record = rig.record().expect("a record");
    assert!(record.was_active && !record.prepared);

    let rig = Rig::new(UnitState::Active);
    rig.host.fail("state");
    rig.fails("prepare", "host_failed", Class::Environment);
    assert!(rig.record().is_none());
}

/// `cutover` starts the stopped unit; run again with the unit up, it starts nothing.
#[test]
fn cutover_starts_the_unit_once() {
    let rig = Rig::new(UnitState::Active);
    rig.ok("prepare");
    rig.host.clear_calls();
    rig.ok("cutover");
    assert_eq!(rig.host.calls(), ["state", "start"]);
    assert!(rig.record().expect("a record").started);
    rig.host.clear_calls();
    rig.ok("cutover");
    assert_eq!(rig.host.calls(), ["state"]);
}

/// A unit that is up but whose start the record does not know of (a kill after the start) is
/// recorded as started without a second start.
#[test]
fn cutover_resumed_after_the_start_records_it() {
    let rig = Rig::new(UnitState::Active);
    rig.ok("prepare");
    *rig.host.state.borrow_mut() = UnitState::Activating;
    rig.ok("cutover");
    assert!(!rig.host.calls().contains(&"start".to_owned()));
    assert!(rig.record().expect("a record").started);
}

/// `cutover` before `prepare` finished, with no record, or after a rollback, is refused and
/// starts nothing.
#[test]
fn cutover_refuses_an_update_that_is_not_prepared() {
    let rig = Rig::new(UnitState::Inactive);
    rig.fails("cutover", "not_prepared", Class::Release);
    let mut record = Record::new("2.0.0", DIGEST, false);
    std::fs::create_dir_all(rig.state_dir()).expect("dir");
    Store::new(rig.state_dir(), None)
        .save(&record)
        .expect("save");
    rig.fails("cutover", "not_prepared", Class::Release);
    record.prepared = true;
    record.rolled_back = true;
    Store::new(rig.state_dir(), None)
        .save(&record)
        .expect("save");
    rig.fails("cutover", "rolled_back", Class::Release);
    assert!(!rig.host.calls().contains(&"start".to_owned()));
}

/// A unit that does not start ends `cutover` as the environment's failure.
#[test]
fn cutover_reports_a_unit_that_does_not_start() {
    let rig = Rig::new(UnitState::Active);
    rig.ok("prepare");
    rig.host.fail("start");
    rig.fails("cutover", "host_failed", Class::Environment);
    assert!(!rig.record().expect("a record").started);
}

/// `verify` passes when the unit is active and runs the target version, and says so once in
/// the record; run again it passes without writing.
#[test]
fn verify_passes_on_an_active_unit_running_the_target() {
    let rig = Rig::new(UnitState::Active);
    rig.ok("prepare");
    rig.ok("cutover");
    rig.ok("verify");
    assert!(rig.record().expect("a record").verified);
    rig.ok("verify");
}

/// A unit that is still activating defers `verify`: exit 75, `not_active_yet`, with a retry
/// time, and the record says nothing was verified.
#[test]
fn verify_defers_while_the_unit_is_activating() {
    let rig = Rig::new(UnitState::Active);
    *rig.host.after_start.borrow_mut() = UnitState::Activating;
    rig.ok("prepare");
    rig.ok("cutover");
    let (outcome, report) = rig.run("verify");
    assert_eq!(outcome, Outcome::Deferred);
    assert_eq!(report.reason.as_deref(), Some("not_active_yet"));
    assert_eq!(
        report.retry_after_secs,
        Some(crate::steps::VERIFY_RETRY_SECS)
    );
    assert!(!rig.record().expect("a record").verified);
}

/// A failed, stopped or odd unit fails `verify` as a release failure.
#[test]
fn verify_fails_a_unit_that_is_not_active() {
    for state in [
        UnitState::Failed,
        UnitState::Inactive,
        UnitState::Other("deactivating".to_owned()),
    ] {
        let rig = Rig::new(UnitState::Active);
        *rig.host.after_start.borrow_mut() = state.clone();
        rig.ok("prepare");
        rig.ok("cutover");
        let report = rig.fails("verify", "not_active", Class::Release);
        assert!(report.detail.expect("detail").ends_with(state.word()));
    }
}

/// An active unit still running the old version (the package was not installed) fails `verify`.
#[test]
fn verify_fails_when_the_installed_version_is_not_the_target() {
    let rig = Rig::new(UnitState::Active);
    *rig.host.installs.borrow_mut() = "1.0.0".to_owned();
    rig.ok("prepare");
    rig.ok("cutover");
    let report = rig.fails("verify", "version_mismatch", Class::Release);
    assert_eq!(
        report.detail.as_deref(),
        Some("kbf reports version 1.0.0, the update installs 2.0.0")
    );
}

/// `verify` without a record of this update fails, and a host that cannot name the version
/// fails it as the environment's.
#[test]
fn verify_needs_the_update_and_a_readable_version() {
    let rig = Rig::new(UnitState::Active);
    rig.fails("verify", "not_prepared", Class::Release);
    rig.ok("prepare");
    rig.ok("cutover");
    rig.host.fail("version");
    rig.fails("verify", "host_failed", Class::Environment);
}

/// `rollback` after the previous package is back restarts a unit that was running, so it runs
/// the previous binary; run again it does nothing.
#[test]
fn rollback_restarts_a_unit_that_was_running() {
    let rig = Rig::new(UnitState::Active);
    rig.ok("prepare");
    rig.ok("cutover");
    rig.host.clear_calls();
    rig.ok("rollback");
    assert_eq!(rig.host.calls(), ["restart"]);
    assert!(rig.record().expect("a record").rolled_back);
    rig.host.clear_calls();
    rig.ok("rollback");
    assert!(rig.host.calls().is_empty());
}

/// A rollback after `prepare` alone (the unit stopped, not started) restarts it too.
#[test]
fn rollback_after_prepare_gives_the_unit_back() {
    let rig = Rig::new(UnitState::Active);
    rig.ok("prepare");
    rig.ok("rollback");
    assert_eq!(*rig.host.state.borrow(), UnitState::Active);
}

/// On a first install the rollback stops the unit the update started, and a unit already
/// stopped is left alone.
#[test]
fn rollback_of_a_first_install_stops_the_unit() {
    let rig = Rig::new(UnitState::Inactive);
    rig.write(true);
    rig.ok("prepare");
    rig.ok("cutover");
    assert_eq!(*rig.host.state.borrow(), UnitState::Active);
    rig.ok("rollback");
    assert_eq!(*rig.host.state.borrow(), UnitState::Inactive);
    assert!(!rig.host.calls().contains(&"restart".to_owned()));

    let rig = Rig::new(UnitState::Inactive);
    rig.write(true);
    rig.ok("prepare");
    rig.host.clear_calls();
    rig.ok("rollback");
    assert_eq!(rig.host.calls(), ["state"]);
}

/// A unit that was stopped before the update stays stopped after its rollback.
#[test]
fn rollback_leaves_a_unit_stopped_that_was_stopped_before() {
    let rig = Rig::new(UnitState::Inactive);
    rig.ok("prepare");
    rig.ok("cutover");
    rig.ok("rollback");
    assert_eq!(*rig.host.state.borrow(), UnitState::Inactive);
    assert!(!rig.host.calls().contains(&"restart".to_owned()));
}

/// With nothing of this update done (no record, or the record of another update), `rollback`
/// succeeds and touches nothing.
#[test]
fn rollback_with_nothing_to_undo_succeeds_and_does_nothing() {
    let rig = Rig::new(UnitState::Active);
    rig.ok("rollback");
    let mut other = Record::new("1.5.0", DIGEST, false);
    other.was_active = true;
    std::fs::create_dir_all(rig.state_dir()).expect("dir");
    Store::new(rig.state_dir(), None)
        .save(&other)
        .expect("save");
    rig.ok("rollback");
    assert!(rig.host.calls().is_empty());
    assert!(!rig.record().expect("a record").rolled_back);
}

/// A unit that cannot be restarted or stopped fails `rollback` as the environment's, and the
/// record is not marked, so a retry tries again.
#[test]
fn rollback_that_cannot_reach_the_unit_is_not_recorded() {
    let rig = Rig::new(UnitState::Active);
    rig.ok("prepare");
    rig.host.fail("restart");
    rig.fails("rollback", "host_failed", Class::Environment);
    assert!(!rig.record().expect("a record").rolled_back);

    let rig = Rig::new(UnitState::Inactive);
    rig.write(true);
    rig.ok("prepare");
    rig.ok("cutover");
    rig.host.fail("stop");
    rig.fails("rollback", "host_failed", Class::Environment);
    assert!(!rig.record().expect("a record").rolled_back);
}

/// `cleanup` before `verify` succeeded is refused and removes nothing; after it, the record is
/// removed, and a `cleanup` run again has nothing to do.
#[test]
fn cleanup_waits_for_verify_and_removes_the_record() {
    let rig = Rig::new(UnitState::Active);
    rig.ok("prepare");
    rig.fails("cleanup", "not_verified", Class::Release);
    rig.ok("cutover");
    rig.fails("cleanup", "not_verified", Class::Release);
    assert!(rig.record().is_some());
    rig.ok("verify");
    rig.ok("cleanup");
    assert!(rig.record().is_none());
    rig.ok("cleanup");
}

/// After a rollback `cleanup` refuses and the record stays; with another update's record it
/// has nothing to do.
#[test]
fn cleanup_does_not_finish_a_rolled_back_update() {
    let rig = Rig::new(UnitState::Active);
    rig.ok("prepare");
    rig.ok("rollback");
    rig.fails("cleanup", "not_verified", Class::Release);
    assert!(rig.record().expect("a record").rolled_back);

    let rig = Rig::new(UnitState::Active);
    let mut other = Record::new("1.5.0", DIGEST, false);
    other.verified = true;
    std::fs::create_dir_all(rig.state_dir()).expect("dir");
    Store::new(rig.state_dir(), None)
        .save(&other)
        .expect("save");
    rig.ok("cleanup");
    assert!(rig.record().is_some());
}

/// A params file carrying an `updater.*` key the hook does not know is accepted.
#[test]
fn an_unknown_reserved_key_is_ignored() {
    let rig = Rig::new(UnitState::Active);
    let mut params = rig.params(false);
    params.insert("updater.from_the_future", Scalar::Text("x".to_owned()));
    params.insert("unrelated", Scalar::Bool(true));
    rig.write_params(&params);
    rig.ok("prepare");
}
