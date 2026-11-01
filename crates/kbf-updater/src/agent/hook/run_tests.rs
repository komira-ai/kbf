//! The command line and the params as the updater gives them: every way they can be wrong ends
//! the step with exit 1 and a last line that says why, and the step does nothing; a second copy
//! of a step, a record that is not ours and a world that cannot be built end it the same way.

use std::ffi::OsString;

use update_hook::{Class, Outcome, Scalar, Step};

use super::record::{FILE, SCHEMA};
use super::testing::{DIGEST, Rig};
use super::{Setup, WorldError, main, real, run};

fn s(text: &str) -> String {
    text.to_owned()
}

/// A command line or a params file that is wrong ends the step before it does anything: exit 1,
/// the reason named, no call to any port and no record.
///
/// Catches: a step that runs on a target or a digest it was not given, or on flags it cannot read.
#[test]
fn a_command_line_or_params_that_are_wrong_stop_the_step_before_it_does_anything() {
    let reason = |rig: &Rig, args: &[String]| {
        let ran = rig.run_args(args, None);
        assert_eq!(ran.outcome, Outcome::Failure, "{args:?}");
        assert_eq!(ran.report.class, Class::Release);
        assert!(rig.calls().is_empty(), "{args:?}");
        assert_eq!(rig.record(), None);
        ran.reason().to_owned()
    };
    let rig = Rig::new("hook-run-args");
    assert_eq!(reason(&rig, &[]), "bad_arguments");
    assert_eq!(reason(&rig, &[s("--record-dir")]), "bad_arguments");
    assert_eq!(
        reason(&rig, &[s("--nonsense"), s("x"), s("prepare")]),
        "bad_arguments"
    );
    let mut no_step = rig.flags.clone();
    assert_eq!(reason(&rig, &no_step), "bad_arguments");
    no_step.push(s("sideways"));
    assert_eq!(reason(&rig, &no_step), "bad_arguments");
    let mut no_params = rig.flags.clone();
    no_params.extend([s("verify")]);
    assert_eq!(reason(&rig, &no_params), "bad_arguments");

    let with = |rig: &Rig, change: &dyn Fn(&mut update_hook::Params)| {
        let mut rig2 = Rig::new("hook-run-params");
        rig2.params = rig.params.clone();
        change(&mut rig2.params);
        let ran = rig2.run(Step::Prepare);
        assert!(rig2.calls().is_empty());
        (ran.outcome, ran.reason().to_owned(), ran.report.detail)
    };
    let (outcome, why, detail) = with(&rig, &|p| {
        p.insert("updater.target_version", Scalar::Text(s("not a version")));
    });
    assert_eq!((outcome, why.as_str()), (Outcome::Failure, "bad_params"));
    assert!(detail.unwrap().contains("updater.target_version"));
    for (name, change) in [
        (
            "a digest of the wrong shape",
            Box::new(|p: &mut update_hook::Params| {
                p.insert("updater.artifact_digest", Scalar::Text(s("sha256:short")));
            }) as Box<dyn Fn(&mut update_hook::Params)>,
        ),
        (
            "a target of the wrong type",
            Box::new(|p| {
                p.insert("updater.target_version", Scalar::Integer(2));
            }),
        ),
    ] {
        let (outcome, why, _) = with(&rig, &*change);
        assert_eq!(
            (outcome, why.as_str()),
            (Outcome::Failure, "bad_params"),
            "{name}"
        );
    }
    // Keys the params lack.
    for key in ["updater.target_version", "updater.artifact_digest"] {
        let mut rig2 = Rig::new("hook-run-missing");
        rig2.params = update_hook::Params::default();
        for other in [
            "updater.staged_release",
            "updater.target_version",
            "updater.artifact_digest",
        ] {
            if other != key {
                let value = if other == "updater.staged_release" {
                    rig2.dir.join("artifact").display().to_string()
                } else if other == "updater.target_version" {
                    s("0.1.7+gbbbbbbbbbbbb-3")
                } else {
                    s(DIGEST)
                };
                rig2.params.insert(other, Scalar::Text(value));
            }
        }
        let ran = rig2.run(Step::Prepare);
        assert_eq!(
            (ran.outcome, ran.reason()),
            (Outcome::Failure, "bad_params"),
            "{key}"
        );
    }
    // A params file that is not there.
    let mut args = rig.args(Step::Verify);
    let at = args.iter().position(|a| a == "--params").unwrap() + 1;
    args[at] = rig.dir.join("absent.toml").display().to_string();
    assert_eq!(reason(&rig, &args), "bad_params");
}

/// The flags before the step take `--flag value` and `--flag=value`, and the step and the rest
/// are what the contract says: a step given with `=` flags runs the same.
///
/// Catches: a flag split from its value, or a flag swallowing the step.
#[test]
fn flags_in_either_form_run_the_step() {
    let rig = Rig::new("hook-run-forms");
    let mut args = rig.args(Step::Prepare);
    args[0] = format!("--record-dir={}", rig.record_dir().display());
    args.remove(1);
    let ran = rig.run_args(&args, None);
    assert_eq!(ran.outcome, Outcome::Success);
}

/// A second copy of a step, with the record directory locked, refuses: exit 1, `busy`, the
/// environment's class, and nothing changed. Once the lock is let go the step runs.
///
/// Catches: two copies of a step writing one record.
#[test]
fn a_second_copy_of_a_step_refuses_and_changes_nothing() {
    let rig = Rig::new("hook-run-busy");
    let held = std::fs::File::open(rig.record_dir()).unwrap();
    held.lock().unwrap();
    for step in Step::ALL {
        let ran = rig.run(step);
        assert_eq!(
            (ran.outcome, ran.reason(), ran.report.class),
            (Outcome::Failure, "busy", Class::Environment)
        );
    }
    assert!(rig.calls().is_empty());
    assert!(
        std::fs::read_dir(rig.record_dir())
            .unwrap()
            .next()
            .is_none()
    );
    held.unlock().unwrap();
    assert_eq!(rig.run(Step::Prepare).outcome, Outcome::Success);
}

/// A record directory that is not there, a record that is not a record, and one written by a
/// newer hook end the step with exit 1 and the record untouched.
///
/// Catches: a hook that starts over on a record it cannot read, or rewrites a newer one.
#[test]
fn a_record_directory_or_record_that_cannot_be_used_ends_the_step() {
    let rig = Rig::new("hook-run-missing-dir");
    std::fs::remove_dir(rig.record_dir()).unwrap();
    let ran = rig.run(Step::Prepare);
    assert_eq!(
        (ran.outcome, ran.reason()),
        (Outcome::Failure, "record_unusable")
    );
    assert!(
        ran.report
            .detail
            .unwrap()
            .contains("cannot open the record directory")
    );

    let rig = Rig::new("hook-run-corrupt");
    std::fs::write(rig.record_dir().join(FILE), "{ not json").unwrap();
    let ran = rig.run(Step::Prepare);
    assert_eq!(
        (ran.outcome, ran.reason(), ran.report.class),
        (Outcome::Failure, "record_unreadable", Class::Environment)
    );
    assert_eq!(
        std::fs::read_to_string(rig.record_dir().join(FILE)).unwrap(),
        "{ not json"
    );

    let rig = Rig::new("hook-run-newer");
    let newer = format!("{{\"schema\": {}}}", SCHEMA + 1);
    std::fs::write(rig.record_dir().join(FILE), &newer).unwrap();
    let ran = rig.run(Step::Rollback);
    assert_eq!(
        (ran.outcome, ran.reason()),
        (Outcome::Failure, "record_unreadable")
    );
    assert!(
        ran.report
            .detail
            .unwrap()
            .contains("newer than this hook's")
    );
    assert_eq!(
        std::fs::read_to_string(rig.record_dir().join(FILE)).unwrap(),
        newer
    );
    assert!(rig.calls().is_empty());
}

/// A world that cannot be built (no certificate, a role without duties) is the environment's
/// fault, said in the last line; the real wiring of a host without files says which file (one
/// in the rig's scratch directory that is not there).
///
/// Catches: a step that panics, or fails with no reason, on a host that is not set up.
#[test]
fn a_world_that_cannot_be_built_ends_the_step() {
    let rig = Rig::new("hook-run-world");
    let world = |_: &super::flags::Flags| Err(WorldError::Certificate(s("no certificate")));
    let setup = Setup {
        world: &world,
        before_write: None,
    };
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let outcome = run(&rig.args(Step::Prepare), &setup, &mut out, &mut err);
    assert_eq!(outcome, Outcome::Failure);
    let line = String::from_utf8(out).unwrap();
    assert!(
        line.contains("\"reason\":\"world\"") && line.contains("no certificate"),
        "{line}"
    );
    assert!(line.contains("\"class\":\"environment\""));

    let setup = Setup {
        world: &real::world,
        before_write: None,
    };
    let (mut out, mut err) = (Vec::new(), Vec::new());
    assert_eq!(
        run(&rig.args(Step::Prepare), &setup, &mut out, &mut err),
        Outcome::Failure
    );
    let line = String::from_utf8(out).unwrap();
    let cert = rig.dir.join("absent").join("cert.pem");
    assert!(line.contains(&cert.display().to_string()), "{line}");
    assert!(line.contains("\"reason\":\"world\""), "{line}");
}

/// Every path the rig's command line names is in the rig's own scratch directory: no run test
/// reads (or finds missing) a file of the host's real service directory, so no test's outcome
/// depends on how the host it runs on is set up.
///
/// Catches: a flag that names a host path such as `/etc/kbf/cert.pem`, whose test passes on a
/// bare machine and fails on a configured host (or the reverse).
#[test]
fn the_rigs_command_line_names_no_path_outside_its_scratch_directory() {
    let rig = Rig::new("hook-run-paths");
    for step in Step::ALL {
        let args = rig.args(step);
        let paths: Vec<&String> = args.iter().filter(|a| a.starts_with('/')).collect();
        // The record directory, the three certificate files, the params file, and the
        // artifact of `prepare`.
        let want = if step == Step::Prepare { 6 } else { 5 };
        assert_eq!(paths.len(), want, "{step}: {args:?}");
        for path in paths {
            assert!(
                std::path::Path::new(path).starts_with(&rig.dir),
                "{step}: {path} is outside {}",
                rig.dir.display()
            );
        }
    }
}

/// A standard output (and a log) that cannot be written to does not change the exit code.
///
/// Catches: a hook that panics, or exits 1, on a closed pipe or a full disk after the work is
/// done.
#[test]
fn a_standard_output_that_cannot_be_written_does_not_change_the_exit_code() {
    let full = || {
        std::fs::OpenOptions::new()
            .write(true)
            .open("/dev/full")
            .expect("/dev/full")
    };
    let rig = Rig::new("hook-run-closed");
    let world = |_: &super::flags::Flags| Ok(rig.world());
    let setup = Setup {
        world: &world,
        before_write: None,
    };
    assert_eq!(
        run(&rig.args(Step::Prepare), &setup, &mut full(), &mut full()),
        Outcome::Success
    );
}

/// The real entry: an argument that is not UTF-8 is refused with a last line, and a command line
/// with no step is refused too; neither touches the host.
///
/// Catches: a hook that panics on the bytes of a command line.
#[test]
fn the_entry_refuses_what_it_cannot_read() {
    use std::os::unix::ffi::OsStringExt;
    let code = main(vec![OsString::from_vec(vec![0xff, 0xfe])]);
    assert_eq!(
        format!("{code:?}"),
        format!("{:?}", std::process::ExitCode::from(1))
    );
    let code = main(vec![OsString::from("--record-dir")]);
    assert_eq!(
        format!("{code:?}"),
        format!("{:?}", std::process::ExitCode::from(1))
    );
}
