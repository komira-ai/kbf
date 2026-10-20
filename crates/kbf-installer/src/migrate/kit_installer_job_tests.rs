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

/// Catches a dry run that offers to enable a job that is running already, and one that
/// swallows a failed look at the job: a kit host whose installer job is loaded says "leave it",
/// and a look that cannot run stops the plan with the io error, naming the job.
///
/// Mutants seen red: the dry run's wording fixed to "enable it"; the `?` on the look replaced
/// by `unwrap_or(false)`.
#[test]
fn a_dry_run_leaves_a_loaded_job_and_stops_when_it_cannot_look() {
    let dry = |rig: &mut Rig, m: &Migration| {
        let runner = runner();
        migrate(
            m,
            &mut rig.host,
            &Hooks {
                runner: &runner,
                verifier: &never(),
                frontend: &NoFrontend,
            },
            Mode::DryRun,
        )
    };
    let (mut rig, m) = kit_without_flag("job-dry-loaded");
    rig.active.borrow_mut().insert(JOB.to_owned());
    let shown = dry(&mut rig, &m).expect("plan").to_string();
    assert!(
        shown.contains(&format!("{JOB}: it is enabled already: leave it")),
        "{shown}"
    );
    assert!(!shown.contains("enable it and start it"), "{shown}");

    let (mut rig, m) = kit_without_flag("job-dry-error");
    let answer = std::mem::replace(
        &mut rig.host.answer,
        Box::new(|_| Err(std::io::Error::other("planted"))),
    );
    let label = JOB.trim_end_matches(".plist");
    rig.host.answer = Box::new(move |cmd| {
        if cmd.args.iter().any(|a| a.ends_with(label))
            && cmd.args.first().is_some_and(|a| a == "print")
        {
            Err(std::io::Error::other("planted: cannot look"))
        } else {
            answer(cmd)
        }
    });
    let Err(MigrateError::Io { what, source }) = dry(&mut rig, &m) else {
        panic!("the plan went on");
    };
    assert_eq!(what, JOB);
    assert!(source.to_string().contains("cannot look"), "{source}");
}

/// Catches a dry run that lists an installer job for an old host with no `--installer-unit`:
/// that host's units are the operator's, the apply enables nothing, so the plan names no job
/// and asks nothing about one.
///
/// Mutant seen red: the dry run's job defaulting to the kit's whatever the host.
#[test]
fn a_dry_run_of_an_old_host_without_the_flag_lists_no_installer_job() {
    let mut rig = Rig::mac("job-dry-old");
    let mut m = rig.migration();
    m.installer_unit = None;
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
    assert!(!shown.contains("kbf-installer"), "{shown}");
    assert!(shown.contains("copy or verify each release"), "{shown}");
}
