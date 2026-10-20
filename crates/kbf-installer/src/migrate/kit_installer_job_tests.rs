//! The installer's own job after a kit host is adopted, when `--installer-unit` names none.
//! Each test says the defect it catches.

use super::kit_host::Form;
use super::rig::*;
use super::*;
use crate::layout::Platform;

const JOB: &str = "com.komira.kbf-installer.plist";

fn never() -> FnVerifier {
    FnVerifier(Box::new(|_, _| Err("no pipeline here".to_owned())))
}

fn kit_without_flag(name: &str) -> (Rig, Migration) {
    let rig = Rig::kit(name, Platform::MacOs, Form::New);
    let mut m = rig.migration();
    m.installer_unit = None;
    (rig, m)
}

fn apply(rig: &mut Rig, m: &Migration) -> Report {
    let runner = runner();
    migrate(
        m,
        &mut rig.host,
        &Hooks {
            runner: &runner,
            verifier: &never(),
            frontend: &NoFrontend,
        },
        Mode::Apply,
    )
    .expect("migrate")
}

/// Catches an adopted Mac that takes no orders: with no `--installer-unit`, a kit-host migration
/// still enables and starts the package's job `com.komira.kbf-installer` (launchd: a bootstrap
/// of its plist), last, and says so. Before this change `enable_installer` ran only when the
/// flag named a unit, so a Mac whose job was left disabled (a re-adopt after a rollback) stayed
/// that way.
///
/// Mutants seen red: the default removed (`installer_unit` alone decides); the default given to
/// every host, not only a kit host (see the next test).
#[test]
fn a_kit_migration_enables_the_installer_job_without_the_flag() {
    let (mut rig, m) = kit_without_flag("job-default");
    let report = apply(&mut rig, &m);
    assert!(
        report
            .to_string()
            .contains(&format!("enabled and started {JOB}")),
        "{report}"
    );
    let events = rig.events.borrow().clone();
    assert_eq!(
        events.last().map(String::as_str),
        Some(format!("start {JOB} links=true").as_str()),
        "{events:?}"
    );
    assert!(rig.active.borrow().contains(JOB));
}

/// Catches the default leaking: an old host (not a kit host) with no flag enables nothing of
/// the installer, and a kit host with the flag uses the unit the flag names, not the default.
///
/// Mutant seen red: the named unit replaced by the default.
#[test]
fn the_default_is_for_a_kit_host_and_a_named_unit_wins() {
    let mut rig = Rig::mac("job-old-host");
    let mut m = rig.migration();
    m.installer_unit = None;
    apply(&mut rig, &m);
    assert!(
        !rig.events
            .borrow()
            .iter()
            .any(|e| e.contains("kbf-installer"))
    );

    let mut rig = Rig::kit("job-named", Platform::MacOs, Form::New);
    let mut m = rig.migration();
    m.installer_unit = Some(unit("com.komira.other.plist"));
    let report = apply(&mut rig, &m);
    assert!(
        report.to_string().contains("com.komira.other.plist"),
        "{report}"
    );
    assert!(!rig.events.borrow().iter().any(|e| e.contains(JOB)));
}

/// Catches an enable that ignores what is running, and a dry run that does not say it: a job
/// that is loaded already is left alone, and the dry run lists the job (named or not).
///
/// Mutant seen red: the dry run's plan reading `m.installer_unit` alone.
#[test]
fn a_loaded_job_is_left_alone_and_the_dry_run_names_it() {
    let (mut rig, m) = kit_without_flag("job-dry");
    let runner = runner();
    let report = migrate(
        &m,
        &mut rig.host,
        &Hooks {
            runner: &runner,
            verifier: &never(),
            frontend: &NoFrontend,
        },
        Mode::DryRun,
    )
    .expect("plan");
    let shown = report.to_string();
    assert!(
        shown.contains(&format!("{JOB}: enable it and start it, last")),
        "{shown}"
    );

    let (mut rig, m) = kit_without_flag("job-loaded");
    rig.active.borrow_mut().insert(JOB.to_owned());
    let report = apply(&mut rig, &m);
    assert!(
        report
            .to_string()
            .contains(&format!("{JOB}: already enabled")),
        "{report}"
    );
    assert!(
        !rig.events
            .borrow()
            .iter()
            .any(|e| e == &format!("start {JOB} links=true"))
    );
}
