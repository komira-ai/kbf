//! Undoing a migration, and the guard that keeps the old helper and the installer from
//! both managing one host.

use std::fs;

use serde_json::json;

use super::rig::*;
use super::*;

fn never() -> FnVerifier {
    FnVerifier(Box::new(|_, _| Err("no pipeline here".to_owned())))
}

fn rollback_now(rig: &mut Rig, mode: Mode) -> Result<Report, MigrateError> {
    let m = rig.migration();
    rollback(&m, &mut rig.host, mode)
}

fn guard_now(rig: &Rig) -> Result<(), MigrateError> {
    let m = rig.migration();
    guard(&m.layout, m.owner_uid)
}

/// Catches a `setup` that goes ahead beside an old helper: while the old state exists and no
/// migration finished (never started, begun and crashed, or rolled back) the guard refuses
/// (exit 5); on a new host and after a finished migration it lets through.
///
/// Mutants seen red: the guard checking only that a journal exists (not its phase); the
/// guard passing when the old state is there but the new tree is not.
#[test]
fn the_guard_holds_until_the_migration_is_done() {
    let mut rig = Rig::linux("guard");
    let error = guard_now(&rig).expect_err("unmigrated");
    assert!(matches!(error, MigrateError::Unmigrated(_)), "{error}");
    assert_eq!(error.exit().code(), 5);
    assert!(error.to_string().contains("kbf-installer migrate --apply"));

    rig.fail_start.set(true);
    run_migrate(&mut rig, &never(), Mode::Apply).expect_err("crashes before the start");
    assert!(guard_now(&rig).is_err(), "begun is not done");

    rig.fail_start.set(false);
    run_migrate(&mut rig, &never(), Mode::Apply).expect("finish");
    guard_now(&rig).expect("done");

    rollback_now(&mut rig, Mode::Apply).expect("rollback");
    assert!(guard_now(&rig).is_err(), "rolled back is unmigrated again");

    let fresh = Rig::linux("guard-new-host");
    fs::remove_file(fresh.at("var/lib/kbf-deploy/state.json")).expect("rm");
    fs::remove_file(fresh.at("var/lib/kbf-deploy/helper.json")).expect("rm");
    guard_now(&fresh).expect("no old state, nothing to wait for");
}

/// Catches a rollback that leaves the host half old: after a finished migration it stops
/// the installer's unit first, the migrated unit, restores the unit's original bytes,
/// enables the helper, starts what ran before (helper and daemon), takes the note away,
/// and records `rolled_back`. The old tree and state are what they were, so the old tools
/// work again. A second rollback is a no-op, and a migrate after it works.
///
/// Mutants seen red: the installer unit not stopped; the helper not enabled; the unit
/// original not restored.
#[test]
fn a_rollback_puts_the_old_layout_back_and_a_migrate_can_follow() {
    let mut rig = Rig::linux("rollback");
    run_migrate(&mut rig, &never(), Mode::Apply).expect("migrate");
    rig.active
        .borrow_mut()
        .insert("kbf-installer.service".to_owned());
    rig.events.borrow_mut().clear();
    let old_tree = rig.snapshot("usr/local/kbf");

    let report = rollback_now(&mut rig, Mode::Apply).expect("rollback");
    assert_eq!(report.outcome, Outcome::RolledBack);
    assert_eq!(
        *rig.events.borrow(),
        [
            "stop kbf-installer.service links=true",
            "disable kbf-installer.service links=true",
            "stop kbf-daemon.service links=true",
            "daemon-reload links=true",
            "enable kbf-helper.service links=true",
            "start kbf-helper.service links=true",
            "start kbf-daemon.service links=true",
        ]
    );
    let unit = fs::read_to_string(rig.at("etc/systemd/system/kbf-daemon.service")).expect("unit");
    assert_eq!(unit, OLD_UNIT);
    assert!(!rig.at("var/lib/kbf-deploy/MIGRATED").exists());
    assert_eq!(rig.snapshot("usr/local/kbf"), old_tree);
    let journal = rig.json(&rig.new_state_dir().join("migration.json"));
    assert_eq!(
        (journal["phase"].clone(), journal["stopped"].clone()),
        (json!("rolled_back"), json!([]))
    );
    let active = rig.active.borrow().clone();
    assert!(active.contains("kbf-daemon.service") && active.contains("kbf-helper.service"));
    assert!(!active.contains("kbf-installer.service"));
    drop(active);

    let again = rollback_now(&mut rig, Mode::Apply).expect("again");
    assert!(again.to_string().contains("already rolled back"));

    rig.events.borrow_mut().clear();
    let migrated = run_migrate(&mut rig, &never(), Mode::Apply).expect("migrate again");
    assert_eq!(migrated.outcome, Outcome::Migrated);
    guard_now(&rig).expect("done again");
    let unit = fs::read_to_string(rig.at("etc/systemd/system/kbf-daemon.service")).expect("unit");
    assert!(unit.contains("/var/lib/kbf-installer/releases/current"));
}

/// Catches a rollback that throws away the installer's work: once the installer has
/// activated, staged or put a release on trial since, the old tree no longer holds what
/// runs, so the rollback is refused (exit 5) and changes nothing. Before the migration
/// finished it is allowed whatever the state says.
///
/// Mutant seen red: the `moved` comparison removed.
#[test]
fn a_rollback_after_the_installer_moved_on_is_refused() {
    let mut rig = Rig::linux("moved-on");
    run_migrate(&mut rig, &never(), Mode::Apply).expect("migrate");
    let path = rig.new_state_dir().join("state.json");
    let original = fs::read(&path).expect("state");
    for (field, value) in [
        ("current", json!(OTHER)),
        ("previous", json!(null)),
        ("staged", json!(OTHER)),
    ] {
        let mut state = rig.json(&path);
        state[field] = value;
        fs::write(&path, serde_json::to_vec(&state).expect("json")).expect("write");
        let before = rig.snapshot("");
        let error = rollback_now(&mut rig, Mode::Apply).expect_err(field);
        assert!(
            matches!(error, MigrateError::MovedOn(_)),
            "{field}: {error}"
        );
        assert_eq!(rig.snapshot(""), before);
        fs::write(&path, &original).expect("restore");
    }
    let mut state = rig.json(&path);
    state["pending_confirm"] = json!({"activation": "1-2", "commit": OTHER, "previous": null});
    fs::write(&path, serde_json::to_vec(&state).expect("json")).expect("write");
    assert!(matches!(
        rollback_now(&mut rig, Mode::Apply),
        Err(MigrateError::MovedOn(_))
    ));
}

/// Catches a rollback of a migration that crashed half way being refused: with the
/// journal `started` (the daemon and helper stopped, units rewritten) a rollback restores
/// the original unit and starts what was stopped.
#[test]
fn a_migration_that_crashed_can_be_rolled_back() {
    let mut rig = Rig::linux("crash-rollback");
    rig.fail_start.set(true);
    run_migrate(&mut rig, &never(), Mode::Apply).expect_err("crashes");
    rig.fail_start.set(false);
    rollback_now(&mut rig, Mode::Apply).expect("rollback");
    let active = rig.active.borrow();
    assert!(active.contains("kbf-daemon.service") && active.contains("kbf-helper.service"));
    let unit = fs::read_to_string(rig.at("etc/systemd/system/kbf-daemon.service")).expect("unit");
    assert_eq!(unit, OLD_UNIT);
}

/// Catches a rollback with nothing to roll back, a dry run that acts, and a user who is
/// not the owner: no journal is an error (exit 5), a dry run changes nothing, a stranger
/// is refused.
#[test]
fn rollback_needs_a_migration_a_dry_run_changes_nothing_and_the_owner_applies() {
    let mut rig = Rig::linux("rollback-none");
    let error = rollback_now(&mut rig, Mode::Apply).expect_err("no tree");
    assert!(matches!(error, MigrateError::NoJournal), "{error}");
    fs::create_dir_all(rig.new_state_dir()).expect("dir");
    assert!(matches!(
        rollback_now(&mut rig, Mode::Apply),
        Err(MigrateError::NoJournal)
    ));

    run_migrate(&mut rig, &never(), Mode::Apply).expect("migrate");
    let before = rig.snapshot("");
    let planned = rollback_now(&mut rig, Mode::DryRun).expect("plan");
    assert_eq!(planned.outcome, Outcome::Planned);
    assert_eq!(rig.snapshot(""), before);
    rig.host.euid = me() + 1;
    let error = rollback_now(&mut rig, Mode::Apply).expect_err("stranger");
    assert!(matches!(error, MigrateError::NotRoot { .. }), "{error}");
    assert_eq!(rig.snapshot(""), before);
}

/// Catches the macOS rollback using systemd's verbs: the plist is restored, launchd's
/// `bootstrap` restarts the daemon and `enable` re-enables the helper.
#[test]
fn a_mac_rollback_uses_launchd() {
    let mut rig = Rig::mac("mac-rollback");
    run_migrate(&mut rig, &never(), Mode::Apply).expect("migrate");
    rig.events.borrow_mut().clear();
    rollback_now(&mut rig, Mode::Apply).expect("rollback");
    assert_eq!(
        fs::read_to_string(rig.at("Library/LaunchDaemons/com.komira.kbf-daemon.plist"))
            .expect("plist"),
        PLIST
    );
    assert_eq!(
        *rig.events.borrow(),
        [
            "stop com.komira.kbf-daemon.plist links=true",
            "enable com.komira.kbf-helper.plist links=true",
            "start com.komira.kbf-helper.plist links=true",
            "start com.komira.kbf-daemon.plist links=true",
        ]
    );
}
