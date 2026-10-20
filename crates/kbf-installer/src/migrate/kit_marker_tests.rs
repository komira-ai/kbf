//! The two markers of a kit host: `etc/av2_mac.conf` in the kit's tree (the kit dies without
//! it on every verb, and `mac_join` writes it) and the `com.komira.kbf-daemon` plist whose
//! `ProgramArguments` run `/usr/local/kbf/current/...`. With one and not the other the
//! migration refuses and names the one that is missing; with neither the host is not a kit
//! host; with both it is one. Every place that recognises the kit asks the same question:
//! `migrate` itself, the old-state-beside-a-kit-tree check and `setup`'s guard.

use std::fs;

use super::kit_host::{CURRENT, Form, PLIST_NEW, PLIST_OLD, PLIST_PATH, PREVIOUS, at};
use super::rig::*;
use super::*;
use crate::exit::Exit;
use crate::layout::Platform;

const CONF: &str = "usr/local/kbf/etc/av2_mac.conf";

fn never() -> FnVerifier {
    FnVerifier(Box::new(|_, _| Err("no pipeline here".to_owned())))
}

/// A refusal, with the host untouched.
fn refused(rig: &mut Rig) -> MigrateError {
    let before = rig.snapshot("");
    let error = run_migrate(rig, &never(), Mode::Apply).expect_err("refused");
    assert_eq!(
        rig.snapshot(""),
        before,
        "a refusal changes nothing: {error}"
    );
    error
}

fn drop_conf(rig: &Rig) {
    fs::remove_file(rig.at(CONF)).expect("rm conf");
}

fn drop_plist(rig: &Rig) {
    fs::remove_file(at(&rig.host.root, PLIST_PATH)).expect("rm plist");
}

fn half(error: &MigrateError) -> String {
    assert!(matches!(error, MigrateError::HalfKit(_)), "{error}");
    assert_eq!(error.exit().code(), Exit::Refused.code());
    error.to_string()
}

/// Catches a kit recognised by one marker. Both markers: a kit host, migrated. Only the
/// settings file: refused, saying the plist is not in the LaunchDaemons directory. Only the
/// plist: refused, saying the settings file is missing. Neither (and a plist that runs
/// something else, with no settings file): not a kit host, "nothing to migrate". `setup`'s
/// guard (`standing`) says the same in each case, and the host is untouched by a refusal.
///
/// Mutants seen red: the settings file not looked for (only-plist migrates); the plist
/// taken without its program being the kit's `current/` (the foreign plist is refused);
/// either marker alone enough (the two refusals become a migration or a NothingToMigrate).
#[test]
fn a_kit_host_needs_both_markers_and_one_alone_is_refused_by_name() {
    let mut rig = Rig::kit("m-both", Platform::MacOs, Form::New);
    assert!(matches!(
        guard(&rig.layout, me()),
        Err(MigrateError::Unmigrated(_))
    ));
    let report = run_migrate(&mut rig, &never(), Mode::Apply).expect("both");
    assert_eq!(report.outcome, Outcome::Migrated);

    let mut rig = Rig::kit("m-conf-only", Platform::MacOs, Form::New);
    drop_plist(&rig);
    let said = half(&refused(&mut rig));
    assert!(
        said.contains("com.komira.kbf-daemon.plist is not in"),
        "{said}"
    );
    assert!(said.contains("av2_mac.conf is there"), "{said}");
    let said = half(&guard(&rig.layout, me()).expect_err("half"));
    assert!(
        said.contains("com.komira.kbf-daemon.plist is not in"),
        "{said}"
    );

    let mut rig = Rig::kit("m-plist-only", Platform::MacOs, Form::Old);
    drop_conf(&rig);
    let said = half(&refused(&mut rig));
    assert!(said.contains("av2_mac.conf is missing"), "{said}");
    assert!(said.contains("com.komira.kbf-daemon.plist"), "{said}");
    let said = half(&guard(&rig.layout, me()).expect_err("half"));
    assert!(said.contains("av2_mac.conf is missing"), "{said}");

    let mut rig = Rig::kit("m-neither", Platform::MacOs, Form::New);
    drop_conf(&rig);
    drop_plist(&rig);
    guard(&rig.layout, me()).expect("no kit");
    let report = run_migrate(&mut rig, &never(), Mode::Apply).expect("nothing");
    assert_eq!(report.outcome, Outcome::NothingToMigrate);

    let mut rig = Rig::kit("m-foreign-plist", Platform::MacOs, Form::New);
    drop_conf(&rig);
    let foreign = PLIST_NEW.replace("/usr/local/kbf/current/kbf", "/usr/local/bin/other");
    fs::write(at(&rig.host.root, PLIST_PATH), foreign).expect("plist");
    let report = run_migrate(&mut rig, &never(), Mode::Apply).expect("not the kit's");
    assert_eq!(report.outcome, Outcome::NothingToMigrate);
}

/// Catches a journalled resume that loses the plist marker. A run that died after repointing
/// the plist leaves one running the installer's link; with the kit's journal that counts as
/// the plist marker, so with the settings file gone the host is a half kit (naming the
/// settings file), not "nothing to migrate" and not a disagreement with the journal; with
/// the settings file there the run is finished.
///
/// Mutant seen red: the marker taken from the kit's path only (the repointed plist is
/// no marker, the run reports a disagreement with its journal).
#[test]
fn a_repointed_plist_with_a_kit_journal_is_the_plist_marker() {
    let mut rig = Rig::kit("m-resume", Platform::MacOs, Form::New);
    rig.fail_start.set(true);
    run_migrate(&mut rig, &never(), Mode::Apply).expect_err("start fails");
    rig.fail_start.set(false);
    let conf = fs::read(rig.at(CONF)).expect("conf");
    drop_conf(&rig);
    let said = half(&refused(&mut rig));
    assert!(said.contains("av2_mac.conf is missing"), "{said}");
    let said = half(&guard(&rig.layout, me()).expect_err("half"));
    assert!(said.contains("av2_mac.conf is missing"), "{said}");

    fs::write(rig.at(CONF), conf).expect("conf");
    let report = run_migrate(&mut rig, &never(), Mode::Apply).expect("resumed");
    assert_eq!(report.outcome, Outcome::Migrated);
}

/// Catches old state beside a half kit being guessed at. `state.json` with both markers is
/// two managers (a conflict, exit 5); with the settings file and no job, or a job that runs
/// something else, it is refused naming the job as missing (exit 3). The job without the
/// settings file is the old tools' own job (they ran `current/kbf` too) and is an old host,
/// as is a tree with neither.
///
/// Mutants seen red: the check on the settings file alone (the conf-only cases become a
/// Conflict); the job required but not what it runs (the foreign job is a Conflict).
#[test]
fn old_state_beside_one_kit_marker_is_refused_by_name() {
    let old_state = |rig: &Rig| {
        fs::create_dir_all(rig.at("var/db/kbf-deploy")).expect("dir");
        rig.write_old_state(Some(CURRENT), Some(PREVIOUS), None, None);
    };

    let mut rig = Rig::kit("m-state-both", Platform::MacOs, Form::New);
    old_state(&rig);
    let error = refused(&mut rig);
    assert!(matches!(error, MigrateError::Conflict(_)), "{error}");
    assert_eq!(error.exit().code(), Exit::NeedsOperator.code());

    let mut rig = Rig::kit("m-state-conf", Platform::MacOs, Form::New);
    old_state(&rig);
    drop_plist(&rig);
    let said = half(&refused(&mut rig));
    assert!(said.contains("av2_mac.conf"), "{said}");
    assert!(said.contains("com.komira.kbf-daemon.plist"), "{said}");

    let mut rig = Rig::kit("m-state-foreign", Platform::MacOs, Form::New);
    old_state(&rig);
    let foreign = PLIST_OLD.replace("/usr/local/kbf/current/kbf", "/usr/local/bin/other");
    fs::write(at(&rig.host.root, PLIST_PATH), foreign).expect("plist");
    half(&refused(&mut rig));

    for (name, damage) in [("plist", true), ("neither", false)] {
        let mut rig = Rig::kit(&format!("m-state-{name}"), Platform::MacOs, Form::New);
        old_state(&rig);
        drop_conf(&rig);
        if !damage {
            drop_plist(&rig);
        }
        let report = run_migrate(&mut rig, &never(), Mode::Apply).expect("an old host");
        assert_eq!(report.outcome, Outcome::Migrated, "{name}");
        assert!(!report.to_string().contains("Mac kit"), "{name}: {report}");
    }
}

/// Catches the kit's plist backup being replaced by a later rewrite, at the level of a kit
/// host. A run that died after repointing the plist kept the kit's original; the kit then
/// writes its job again (here in its other form, with the old path) before the next run,
/// which rewrites it and finishes. A rollback after that gives back the first original, not
/// what the kit wrote in between.
///
/// Reachable only this way: `units::judge` answers `Already` for a plist that is repointed
/// and unedited, so a second run of the same plist never reaches the backup. The unit
/// test that covers the case where it does not is
/// `units_tests::the_first_original_is_kept_and_restored`.
///
/// Mutant seen red: `units::rewrite` writing the backup on every rewrite.
#[test]
fn a_rollback_gives_back_the_first_plist_when_the_kit_wrote_another_between_runs() {
    let mut rig = Rig::kit("m-backup", Platform::MacOs, Form::New);
    rig.fail_start.set(true);
    run_migrate(&mut rig, &never(), Mode::Apply).expect_err("start fails");
    rig.fail_start.set(false);
    fs::write(at(&rig.host.root, PLIST_PATH), PLIST_OLD).expect("the kit writes its job");
    let report = run_migrate(&mut rig, &never(), Mode::Apply).expect("finish");
    assert_eq!(report.outcome, Outcome::Migrated);
    assert_eq!(
        fs::read_to_string(at(&rig.host.root, PLIST_PATH)).expect("plist"),
        PLIST_OLD.replace("/usr/local/kbf/current", rig.layout.current().as_str())
    );

    let mut m = rig.migration();
    m.installer_unit = None;
    let report = rollback(&m, &mut rig.host, Mode::Apply).expect("rollback");
    assert_eq!(report.outcome, Outcome::RolledBack);
    assert_eq!(
        fs::read_to_string(at(&rig.host.root, PLIST_PATH)).expect("plist"),
        PLIST_NEW,
        "the first original, not the one written in between"
    );
}
