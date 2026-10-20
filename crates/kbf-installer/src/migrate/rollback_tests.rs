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
            "stop com.komira.kbf-installer.plist links=true",
            "disable com.komira.kbf-installer.plist links=true",
            "stop com.komira.kbf-daemon.plist links=true",
            "enable com.komira.kbf-helper.plist links=true",
            "start com.komira.kbf-helper.plist links=true",
            "start com.komira.kbf-daemon.plist links=true",
        ]
    );
}

/// Catches a rollback that touches a Mac's `kbf.toml` (G9: `kbf-mac-provision` is its one
/// writer, so migrate keeps no original of it and a rollback has none to put back): after a
/// migrate and a real rollback, on a kit Mac and on a Mac the kit never touched (one whose
/// file has no `orders-dir`, which a Linux host would have been given), the file is byte for
/// byte what it was, with its mode, and no kept original is left behind.
///
/// Mutant seen red: `daemon_dirs::restore` removing `kbf.toml` when no original is kept.
#[test]
fn a_mac_rollback_leaves_kbf_toml_byte_for_byte() {
    use crate::migrate::kit_host::{Form, KBF_TOML_PATH, at};
    use std::os::unix::fs::MetadataExt;
    let plain_text = "mode = \"daemon\"\n\n[daemon]\nupdate-dir = \"/var/db/kbf-installer\"\n\
                      driver = [\"native\"]\n";
    for kit in [true, false] {
        let mut rig = if kit {
            Rig::kit(
                "mac-rollback-toml-kit",
                crate::layout::Platform::MacOs,
                Form::New,
            )
        } else {
            Rig::mac("mac-rollback-toml-plain")
        };
        let path = at(&rig.host.root, KBF_TOML_PATH);
        if !kit {
            put(&path, plain_text, 0o640);
        }
        let (before, mode) = (
            fs::read(&path).expect("kbf.toml"),
            fs::metadata(&path).expect("meta").mode() & 0o7777,
        );
        run_migrate(&mut rig, &never(), Mode::Apply).expect("migrate");
        assert_eq!(fs::read(&path).expect("kbf.toml"), before, "kit={kit}");
        let report = rollback_now(&mut rig, Mode::Apply).expect("rollback");
        assert_eq!(report.outcome, Outcome::RolledBack);
        assert_eq!(fs::read(&path).expect("kbf.toml"), before, "kit={kit}");
        assert_eq!(
            fs::metadata(&path).expect("meta").mode() & 0o7777,
            mode,
            "kit={kit}"
        );
        assert!(
            !rig.new_state_dir()
                .join("migration-units/kbf.toml")
                .exists(),
            "kit={kit}"
        );
        assert!(
            !report.to_string().contains("kbf.toml: the original"),
            "{report}"
        );
    }
}

/// Catches a `setup` that is told the wrong thing about a host: `standing` is the guard plus
/// the units a finished migration repointed. A host with no old layout has none; the old
/// layout unmigrated, begun or rolled back is refused (exit 5); after a finished migration it
/// names the journalled units, and still does when the old state has gone (the journal
/// decides first). The real `Journaled` seam of `setup` answers the same.
///
/// Mutants seen red: the units dropped from the answer; the old state checked before the
/// journal (the last host refused); the phase not compared.
#[test]
fn standing_names_the_units_a_finished_migration_repointed() {
    let standing = |rig: &Rig| standing(&rig.layout, me());
    let mut rig = Rig::linux("standing");
    let error = standing(&rig).expect_err("unmigrated");
    assert!(matches!(error, MigrateError::Unmigrated(_)), "{error}");

    run_migrate(&mut rig, &never(), Mode::Apply).expect("migrate");
    let names = |units: Vec<Unit>| units.iter().map(ToString::to_string).collect::<Vec<_>>();
    assert_eq!(names(standing(&rig).expect("done")), ["kbf-daemon.service"]);
    let seam = crate::setup::backend::Journaled::new(rig.layout.clone(), me());
    let via_seam = crate::setup::backend::Migrations::units(&seam).expect("done");
    assert_eq!(names(via_seam), ["kbf-daemon.service"]);

    fs::remove_file(rig.at("var/lib/kbf-deploy/state.json")).expect("rm");
    fs::remove_file(rig.at("var/lib/kbf-deploy/helper.json")).expect("rm");
    assert_eq!(names(standing(&rig).expect("done")), ["kbf-daemon.service"]);

    let fresh = Rig::linux("standing-new-host");
    fs::remove_file(fresh.at("var/lib/kbf-deploy/state.json")).expect("rm");
    fs::remove_file(fresh.at("var/lib/kbf-deploy/helper.json")).expect("rm");
    assert!(standing(&fresh).expect("nothing").is_empty());

    let mut back = Rig::linux("standing-rolled-back");
    run_migrate(&mut back, &never(), Mode::Apply).expect("migrate");
    rollback_now(&mut back, Mode::Apply).expect("rollback");
    assert!(standing(&back).is_err(), "rolled back is unmigrated again");
}

/// Puts a unit file `setup` retired where it was: the file in the unit directory is taken out
/// and its bytes are in the backup under the retired name, as `setup` leaves them.
fn retire(rig: &Rig, unit: &str) {
    let name = rig.at("etc/systemd/system").join(unit);
    let bytes = fs::read(&name).expect("the unit file");
    let backup = rig.new_state_dir().join("migration-units");
    fs::create_dir_all(&backup).expect("backup");
    fs::write(backup.join(retired_name(unit)), bytes).expect("copy");
    fs::remove_file(name).expect("retire");
}

/// Catches a rollback that leaves a retired unit out: a unit the migration did not rewrite
/// (it ran no release path, so no original was kept) that `setup` then retired is put back
/// by `migrate --rollback`, byte for byte, with the rest of the old layout. Where an
/// original was kept, the original is what comes back, not the retired copy (which has the
/// new tree's paths in it).
///
/// Mutants seen red: the `unretire` branch removed (the first unit stays out); `unretire`
/// tried before `restore` (the second unit comes back with the new paths).
#[test]
fn a_rollback_puts_a_retired_unit_back() {
    const OTHER_UNIT: &str = "[Service]\nExecStart=/usr/bin/other-daemon\n";
    let mut rig = Rig::linux("rollback-retired");
    put(
        &rig.at("etc/systemd/system/kbf-daemon.service"),
        OTHER_UNIT,
        0o644,
    );
    run_migrate(&mut rig, &never(), Mode::Apply).expect("migrate");
    retire(&rig, "kbf-daemon.service");
    assert!(!rig.at("etc/systemd/system/kbf-daemon.service").exists());

    let report = rollback_now(&mut rig, Mode::Apply).expect("rollback");
    assert!(
        report
            .to_string()
            .contains("kbf-daemon.service: the file setup retired is back"),
        "{report}"
    );
    let back = fs::read_to_string(rig.at("etc/systemd/system/kbf-daemon.service")).expect("unit");
    assert_eq!(back, OTHER_UNIT);

    let mut rig = Rig::linux("rollback-retired-original");
    run_migrate(&mut rig, &never(), Mode::Apply).expect("migrate");
    retire(&rig, "kbf-daemon.service");
    let report = rollback_now(&mut rig, Mode::Apply).expect("rollback");
    assert!(
        report.to_string().contains("the original is back"),
        "{report}"
    );
    let back = fs::read_to_string(rig.at("etc/systemd/system/kbf-daemon.service")).expect("unit");
    assert_eq!(back, OLD_UNIT);
}
