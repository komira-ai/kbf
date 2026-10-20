//! The paths through the migration that only an unusual host or a fault reaches, each with
//! the defect it would let through: a host with no old layout, a damaged one, a host that
//! has no helper or no installer unit, a migration run again, and a rollback of a host the
//! installer has not touched.

use std::fs;

use super::rig::*;
use super::*;
use crate::testkit::A;

fn never() -> FnVerifier {
    FnVerifier(Box::new(|_, _| Err("no pipeline here".to_owned())))
}

/// Run `m` (a migration the test changed) against the rig's host.
fn run_with_migration(rig: &mut Rig, m: &Migration, mode: Mode) -> Result<Report, MigrateError> {
    let hooks = Hooks {
        runner: &runner(),
        verifier: &never(),
        frontend: &NoFrontend,
    };
    migrate(m, &mut rig.host, &hooks, mode)
}

/// Catches a host with no old state directory at all being an error: it has nothing to
/// migrate, and says so (exit 0). A state directory that is no directory is not "nothing":
/// it is an error naming the path, since the host is not what the verb thinks.
#[test]
fn a_host_without_the_old_state_directory_has_nothing_and_a_file_in_its_place_is_an_error() {
    let mut rig = Rig::linux("old-dir-missing");
    fs::remove_dir_all(rig.at("var/lib/kbf-deploy")).expect("rm");
    let report = run_migrate(&mut rig, &never(), Mode::Apply).expect("nothing");
    assert_eq!(report.outcome, Outcome::NothingToMigrate);

    let mut rig = Rig::linux("old-dir-file");
    fs::remove_dir_all(rig.at("var/lib/kbf-deploy")).expect("rm");
    put(&rig.at("var/lib/kbf-deploy"), "not a directory", 0o644);
    let error = run_migrate(&mut rig, &never(), Mode::Apply).expect_err("a file");
    assert!(matches!(error, MigrateError::Io { .. }), "{error}");
    assert_eq!(error.exit().code(), 1);
}

/// Catches a helper memory with no state file being read as nothing: a host that kept only
/// `helper.json` is an old host, whose state is the empty state, and its links (here none)
/// must agree with it.
#[test]
fn a_host_with_only_the_helpers_memory_is_an_old_host_with_an_empty_state() {
    let mut rig = Rig::linux("memory-only");
    fs::remove_file(rig.at("var/lib/kbf-deploy/state.json")).expect("rm");
    fs::remove_file(rig.at("usr/local/kbf/current")).expect("rm");
    fs::remove_file(rig.at("usr/local/kbf/previous")).expect("rm");
    let report = run_migrate(&mut rig, &never(), Mode::Apply).expect("migrates");
    assert_eq!(report.outcome, Outcome::Migrated);
    assert!(report.unverified.is_empty());
    let state = rig.json(&rig.new_state_dir().join("state.json"));
    assert_eq!(state["current"], serde_json::Value::Null);
    assert_eq!(
        state["floor"]["order_id"], 42,
        "the helper's order is still the floor"
    );
}

/// Catches a damaged `current` link being read as no link: where a regular file stands in
/// its place the read fails with the file's name, rather than the migration going ahead
/// with half a picture.
#[test]
fn a_link_that_cannot_be_read_stops_the_run() {
    let mut rig = Rig::linux("link-file");
    fs::remove_file(rig.at("usr/local/kbf/current")).expect("rm");
    put(&rig.at("usr/local/kbf/current"), "not a link", 0o644);
    let error = run_migrate(&mut rig, &never(), Mode::Apply).expect_err("stops");
    assert!(
        matches!(error, MigrateError::Io { ref what, .. } if what == "current"),
        "{error}"
    );
}

/// Catches the report and the services assuming a floor, a helper and an installer unit: a
/// host seeded with no floor says so and writes none into the settings line, one with no
/// helper unit stops and disables nothing, and neither is a failure.
#[test]
fn a_migration_with_no_floor_and_no_helper_unit_goes_through() {
    let mut rig = Rig::linux("no-floor-no-helper");
    let mut m = rig.migration();
    m.seed.min_generation = None;
    m.helper_unit = None;
    let report = run_with_migration(&mut rig, &m, Mode::Apply).expect("migrates");
    assert_eq!(report.min_generation, None);
    assert!(
        report
            .warnings
            .iter()
            .any(|w| w.contains("NO REPLAY FLOOR")),
        "{report}"
    );
    assert!(
        rig.events
            .borrow()
            .iter()
            .all(|e| !e.contains("kbf-helper")),
        "{:?}",
        rig.events
    );
    assert!(
        rig.active.borrow().contains("kbf-helper.service"),
        "the helper was left as it was"
    );
}

/// Catches a verified release being copied over or asked of the pipeline again: after a
/// migration whose pipeline installed both releases with `VERIFIED`, a rollback and a
/// second migration adopt them as they are and call the pipeline for neither.
#[test]
fn a_release_already_verified_is_adopted_as_it_is() {
    let mut rig = Rig::linux("verified-twice");
    let tree = rig.new_tree().join("releases");
    let calls = std::rc::Rc::new(std::cell::Cell::new(0));
    let installing = FnVerifier(Box::new({
        let (tree, calls) = (tree.clone(), calls.clone());
        move |claimed, _| {
            calls.set(calls.get() + 1);
            let dir = tree.join(claimed.as_str());
            put(&dir.join("kbf"), "verified bytes", 0o755);
            put(&dir.join("VERIFIED"), "", 0o644);
            Ok(())
        }
    }));
    let report = run_migrate(&mut rig, &installing, Mode::Apply).expect("first");
    assert!(report.unverified.is_empty());
    assert_eq!(calls.get(), 2);
    let m = rig.migration();
    rollback(&m, &mut rig.host, Mode::Apply).expect("rollback");
    let report = run_migrate(&mut rig, &installing, Mode::Apply).expect("second");
    assert!(report.unverified.is_empty());
    assert_eq!(calls.get(), 2, "the pipeline is not asked again");
}

/// Catches a leftover note that cannot be removed being ignored: a directory where the note
/// goes (it is not empty, so it cannot be unlinked) is an error naming the note; a note
/// that is not there is not.
#[test]
fn a_note_that_cannot_be_removed_is_an_error_and_one_that_is_gone_is_not() {
    let rig = Rig::linux("note-stuck");
    let m = rig.migration();
    apply::remove_note(&m).expect("no note, nothing to remove");
    put(&rig.at("var/lib/kbf-deploy/MIGRATED/inner"), "x", 0o644);
    let error = apply::remove_note(&m).expect_err("stuck");
    assert!(
        matches!(error, MigrateError::Io { ref what, .. } if what == "MIGRATED"),
        "{error}"
    );
}

/// Catches a guard that trusts a journal it cannot read: with the old state present and a
/// damaged `migration.json` it stops with the file named, not with "unmigrated" or a pass.
#[test]
fn the_guard_names_a_damaged_journal() {
    let rig = Rig::linux("guard-corrupt");
    put(
        &rig.new_state_dir().join("migration.json"),
        "not json",
        0o600,
    );
    let error = guard(&rig.layout, me()).expect_err("damaged");
    assert!(
        matches!(
            error,
            MigrateError::Corrupt {
                file: "migration.json",
                ..
            }
        ),
        "{error}"
    );
}

/// Catches `releases/<name>` links made and unmade wrongly: asking for no `previous` when
/// there is none changes nothing, making it changes something, making it again nothing, and
/// asking for none removes it; a link that is not a link is an error naming it.
#[test]
fn links_are_made_unmade_and_left_alone_as_asked() {
    let rig = Rig::linux("links");
    let rules = apply::rules(&rig.migration());
    let tree = newtree::NewTree::create(&rig.layout, &rules).expect("tree");
    assert!(!tree.set_link("previous", None).expect("nothing to remove"));
    assert!(tree.set_link("previous", Some(&commit(A))).expect("made"));
    assert!(
        !tree
            .set_link("previous", Some(&commit(A)))
            .expect("already")
    );
    assert!(tree.set_link("previous", None).expect("removed"));
    assert_eq!(tree.link_target("previous").expect("read"), None);
    fs::create_dir(rig.new_tree().join("releases/current")).expect("dir");
    let error = tree
        .link_target("current")
        .expect_err("a directory is no link");
    assert!(
        matches!(error, MigrateError::Io { ref what, .. } if what == "current"),
        "{error}"
    );
}

/// Catches a rollback that assumes the installer has written its state, listed a helper and
/// an installer unit, and found every unit running as the migration left it: with the new
/// `state.json` gone, no helper recorded and no installer unit named it still undoes the
/// migration; a unit that is running but was not stopped by the migration is stopped and
/// started again; a unit with no kept original is left as it is.
#[test]
fn a_rollback_of_an_unusual_host_still_puts_the_old_layout_back() {
    let mut rig = Rig::linux("rollback-unusual");
    rig.active.borrow_mut().remove("kbf-daemon.service");
    let mut m = rig.migration();
    m.units.push(unit("kbf-other.service"));
    m.helper_unit = None;
    m.installer_unit = None;
    run_with_migration(&mut rig, &m, Mode::Apply).expect("migrates");
    assert!(
        !rig.active.borrow().contains("kbf-daemon.service"),
        "it was not running"
    );

    rig.active
        .borrow_mut()
        .insert("kbf-daemon.service".to_owned());
    fs::remove_file(rig.new_state_dir().join("state.json")).expect("rm");
    rig.events.borrow_mut().clear();
    let report = rollback(&m, &mut rig.host, Mode::Apply).expect("rolls back");
    assert_eq!(report.outcome, Outcome::RolledBack);
    assert!(
        report.warnings.iter().any(|w| w.contains("unusable")),
        "{:?}",
        report.warnings
    );
    let events = rig.events.borrow().clone();
    assert!(
        events
            .iter()
            .any(|e| e.starts_with("stop kbf-daemon.service")),
        "{events:?}"
    );
    assert!(
        events
            .iter()
            .any(|e| e.starts_with("start kbf-daemon.service")),
        "{events:?}"
    );
    assert!(
        events.iter().all(|e| !e.contains("kbf-helper")),
        "{events:?}"
    );
    let unit_text =
        fs::read_to_string(rig.at("etc/systemd/system/kbf-daemon.service")).expect("unit");
    assert_eq!(unit_text, OLD_UNIT, "the original is back");
}

/// Catches the rig itself answering a command it was not taught: it stops the test instead of
/// returning success for a service command nothing asked for. (The rig is test code; this
/// keeps the test of the migration from passing over a command it did not mean to run.)
#[test]
#[should_panic(expected = "a command the rig does not know")]
fn the_rig_refuses_a_command_it_does_not_know() {
    let rig = Rig::linux("rig-unknown");
    let cmd = kbf_host::Cmd::new(kbf_host::HostPath::new("/usr/bin/true").expect("path"));
    let _ = (rig.host.answer)(&cmd);
}

/// The rig's snapshot of a directory that is not there is empty.
#[test]
fn the_rigs_snapshot_of_a_missing_directory_is_empty() {
    let rig = Rig::linux("rig-snapshot");
    assert!(rig.snapshot("no/such/directory").is_empty());
}

/// Catches a host with a state file and no helper memory being refused: a host whose helper
/// never ran has no `helper.json`, and its replay memory is the empty one (the floor then
/// comes from the flag alone).
#[test]
fn a_host_whose_helper_never_ran_has_the_empty_memory() {
    let mut rig = Rig::linux("no-memory");
    fs::remove_file(rig.at("var/lib/kbf-deploy/helper.json")).expect("rm");
    let report = run_migrate(&mut rig, &never(), Mode::Apply).expect("migrates");
    assert_eq!(report.outcome, Outcome::Migrated);
    let state = rig.json(&rig.new_state_dir().join("state.json"));
    assert_eq!(state["floor"]["order_id"], 0);
    assert_eq!(state["floor"]["generation"], 17);
}
