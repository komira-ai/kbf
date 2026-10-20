//! The installer's own unit after a migration: enabled and started last, as `setup` does,
//! so the migrated host takes orders.

use super::rig::*;
use super::*;

fn never() -> FnVerifier {
    FnVerifier(Box::new(|_, _| Err("no pipeline here".to_owned())))
}

fn position(events: &[String], prefix: &str) -> usize {
    events
        .iter()
        .position(|e| e.starts_with(prefix))
        .unwrap_or_else(|| panic!("no event {prefix:?} in {events:?}"))
}

/// Catches a migrated host that takes no orders: after the run the installer's unit is
/// enabled and running, the command is the one `setup` runs, it comes after the old helper
/// is disabled and the daemon is started (the last thing done), and the only units touched
/// are the helper, the daemon and the installer (no frontend, no proxy). The journal is done.
///
/// Mutants seen red: the enable removed; the enable moved above the helper's disable; `enable`
/// without `--now`; the enable made for every unit of `--unit` (a frontend unit).
#[test]
fn a_migrated_linux_host_has_the_installer_enabled_and_running_last() {
    let mut rig = Rig::linux("installer-enabled");
    let report = run_migrate(&mut rig, &never(), Mode::Apply).expect("migrate");
    assert!(
        report
            .to_string()
            .contains("enabled and started kbf-installer.service"),
        "{report}"
    );
    let events = rig.events.borrow().clone();
    assert_eq!(
        events.last().map(String::as_str),
        Some("enable --now kbf-installer.service links=true")
    );
    assert!(position(&events, "disable kbf-helper") < position(&events, "enable --now"));
    assert!(position(&events, "start kbf-daemon") < position(&events, "enable --now"));
    for e in &events {
        let unit = e.split(' ').rfind(|w| w.ends_with(".service"));
        assert!(
            matches!(
                unit,
                Some("kbf-helper.service" | "kbf-daemon.service" | "kbf-installer.service") | None
            ),
            "{e}"
        );
    }
    assert!(rig.enabled.borrow().contains("kbf-installer.service"));
    assert!(rig.active.borrow().contains("kbf-installer.service"));
    let journal = rig.json(&rig.new_state_dir().join("migration.json"));
    assert_eq!(journal["phase"], "done");
}

/// Catches a re-run that does anything: after a finished migration a second apply reports
/// it done and runs no service command at all.
///
/// Mutant seen red: the already-migrated early return replaced by a fall through.
#[test]
fn a_rerun_after_done_runs_no_service_command() {
    let mut rig = Rig::linux("installer-rerun");
    run_migrate(&mut rig, &never(), Mode::Apply).expect("migrate");
    rig.events.borrow_mut().clear();
    let logged = rig.host.log.len();
    let report = run_migrate(&mut rig, &never(), Mode::Apply).expect("again");
    assert_eq!(report.outcome, Outcome::AlreadyMigrated);
    assert!(rig.events.borrow().is_empty());
    assert_eq!(rig.host.log.len(), logged);
}

/// Catches an enable made without asking: a unit that is enabled already is left alone
/// (`setup`'s question), and the report says so.
///
/// Mutant seen red: the `is_enabled` check removed (the enable always runs).
#[test]
fn an_installer_unit_that_is_enabled_already_is_left_alone() {
    let mut rig = Rig::linux("installer-already");
    rig.enabled
        .borrow_mut()
        .insert("kbf-installer.service".to_owned());
    let report = run_migrate(&mut rig, &never(), Mode::Apply).expect("migrate");
    assert!(!rig.events.borrow().iter().any(|e| e.contains("enable")));
    assert!(
        report
            .to_string()
            .contains("kbf-installer.service: already enabled"),
        "{report}"
    );
}

/// Catches a failure to enable that is swallowed or that marks the migration done: the run
/// ends with the unit named and the way out, exit 5, the journal is not done (so `setup`
/// still waits), and once the unit can be enabled a run again finishes with it enabled.
///
/// Mutants seen red: the error dropped (`let _ =`); the journal written done before the
/// enable.
#[test]
fn a_unit_that_will_not_enable_is_a_stated_refusal_and_a_rerun_finishes() {
    let mut rig = Rig::linux("installer-refused");
    rig.fail_enable.set(true);
    let error = run_migrate(&mut rig, &never(), Mode::Apply).expect_err("refused");
    assert!(
        matches!(error, MigrateError::InstallerUnit { .. }),
        "{error}"
    );
    let said = error.to_string();
    assert!(
        said.contains("kbf-installer.service") && said.contains("planted: enable failed"),
        "{said}"
    );
    assert!(said.contains("migrate --apply"), "{said}");
    assert_eq!(error.exit(), crate::exit::Exit::NeedsOperator);
    assert!(
        guard(&rig.layout, me()).is_err(),
        "a migration that left the host without orders is not done"
    );

    rig.fail_enable.set(false);
    let report = run_migrate(&mut rig, &never(), Mode::Apply).expect("finish");
    assert_eq!(report.outcome, Outcome::Migrated);
    assert!(rig.enabled.borrow().contains("kbf-installer.service"));
    guard(&rig.layout, me()).expect("done");
}

/// Catches a Mac left without orders: the installer's job is bootstrapped from the plist
/// directory, after the daemon's, once (launchd has no enabled-but-stopped job), and a job
/// that is loaded already is not bootstrapped again.
///
/// Mutants seen red: the Linux command used for a plist; the loaded check removed.
#[test]
fn a_mac_host_bootstraps_the_installer_job_last_unless_it_is_loaded() {
    let mut rig = Rig::mac("installer-mac");
    run_migrate(&mut rig, &never(), Mode::Apply).expect("migrate");
    let events = rig.events.borrow().clone();
    assert_eq!(
        events.last().map(String::as_str),
        Some("start com.komira.kbf-installer.plist links=true")
    );
    assert!(rig.host.log.iter().map(ToString::to_string).any(|l| {
        l.ends_with("bootstrap system /Library/LaunchDaemons/com.komira.kbf-installer.plist")
    }));

    let mut loaded = Rig::mac("installer-mac-loaded");
    loaded
        .active
        .borrow_mut()
        .insert("com.komira.kbf-installer.plist".to_owned());
    run_migrate(&mut loaded, &never(), Mode::Apply).expect("migrate");
    assert!(
        !loaded
            .events
            .borrow()
            .iter()
            .any(|e| e.contains("kbf-installer"))
    );
}

/// Catches a dry run that enables, and a plan that does not say it will: the plan names the
/// installer's unit and nothing is run beyond questions.
///
/// Mutant seen red: the plan line removed.
#[test]
fn a_dry_run_says_the_installer_will_be_enabled_and_does_not() {
    let mut rig = Rig::linux("installer-dry");
    let report = run_migrate(&mut rig, &never(), Mode::DryRun).expect("plan");
    let shown = report.to_string();
    assert!(
        shown.contains("kbf-installer.service: enable it and start it, last"),
        "{shown}"
    );
    assert!(
        rig.events.borrow().is_empty() && !rig.enabled.borrow().contains("kbf-installer.service")
    );
}

/// Catches an enable made when the operator named no installer unit on an old host (a kit Mac
/// is the exception: `kit_installer_job_tests` enables the package's job there): nothing about
/// the installer is run.
///
/// Mutant seen red: the `Option` ignored and the unit's name constant used.
#[test]
fn no_installer_unit_named_none_is_enabled() {
    let mut rig = Rig::linux("installer-none");
    let mut m = rig.migration();
    m.installer_unit = None;
    let hooks = Hooks {
        runner: &runner(),
        verifier: &never(),
        frontend: &NoFrontend,
    };
    migrate(&m, &mut rig.host, &hooks, Mode::Apply).expect("migrate");
    assert!(!rig.events.borrow().iter().any(|e| e.contains("installer")));
    guard(&rig.layout, me()).expect("done");
}
