//! The fake host's own tests: its flags, its ports over the files of a directory, and the hook's
//! steps run over it in this process, the way the conformance harness runs them as processes.

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use update_hook::{Outcome, Params, Report, Scalar, Step};

use super::fake::{
    DEADLINE, FakeClock, FakeDrain, FakeHealth, FakeLocal, Own, OwnError, Paths, fake_run, host,
    main, own_flags,
};
use crate::agent::local::{Local, LocalError};
use crate::inventory::Role;
use crate::ports::{Clock, Drain, Health, HealthCheck, Placed};
use crate::testutil;
use crate::version::Version;

const DIGEST: &str = "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

/// A fresh scratch directory with the fake host's `world/`, `faults/`, `record/` and
/// `artifact/` in it; the host runs `1.0.0` and its node serves.
fn scratch(name: &str) -> PathBuf {
    let dir = testutil::scratch(&format!("hook-fake-{name}"));
    for sub in ["world", "faults", "record", "artifact"] {
        fs::create_dir_all(dir.join(sub)).expect("a scratch directory");
    }
    fs::write(
        dir.join("world/helper.json"),
        r#"{"version":"1.0.0","before":null,"pending":false}"#,
    )
    .expect("helper.json");
    fs::write(dir.join("world/node"), "serving").expect("node");
    dir
}

fn words(list: &[&str]) -> Vec<String> {
    list.iter().map(|w| (*w).to_owned()).collect()
}

fn paths(dir: &Path) -> Arc<Paths> {
    Arc::new(Paths {
        world: dir.join("world"),
        faults: dir.join("faults"),
    })
}

fn v(text: &str) -> Version {
    Version::parse(text).expect("a version")
}

fn fault(dir: &Path, step: &str, word: &str) {
    fs::write(dir.join("faults").join(step), word).expect("a fault");
}

/// Runs `step` over the fake host in `dir` as the harness would, the command line in full.
fn step(dir: &Path, step: Step) -> (Outcome, Report) {
    let mut params = Params::default();
    for (key, value) in [
        (
            "updater.staged_release",
            dir.join("artifact").display().to_string(),
        ),
        ("updater.target_version", "2.0.0".to_owned()),
        ("updater.artifact_digest", DIGEST.to_owned()),
    ] {
        params.insert(key, Scalar::Text(value));
    }
    let file = dir.join("params.toml");
    fs::write(&file, params.to_toml()).expect("a params file");
    let at = |sub: &str| dir.join(sub).display().to_string();
    let mut args = vec![
        "update-hook".to_owned(),
        "--world".to_owned(),
        at("world"),
        "--faults".to_owned(),
        at("faults"),
        "--record-dir".to_owned(),
        at("record"),
    ];
    args.extend(words(&[
        "--channel",
        "stable",
        "--client-cert",
        "/absent/cert.pem",
        "--client-key",
        "/absent/key.pem",
        "--ca",
        "/absent/ca.pem",
        "--drain-deadline-secs",
        "30",
        "--retry-secs",
        "60",
    ]));
    args.push(step.name().to_owned());
    if step == Step::Prepare {
        args.push(at("artifact"));
    }
    args.extend(["--params".to_owned(), file.display().to_string()]);
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let outcome = fake_run(&args, &mut out, &mut err);
    let out = String::from_utf8(out).expect("text");
    let report = Report::from_stdout(&out).expect("a last line the strict reader accepts");
    (outcome, report)
}

fn read(dir: &Path, name: &str) -> String {
    fs::read_to_string(dir.join("world").join(name)).unwrap_or_default()
}

// ---- the flags ----

/// `--world` and `--faults` come first, in either order, and the rest is handed on untouched;
/// each is required, needs a value, and is given once.
///
/// Catches: a fake flag handed to the hook (which refuses it), a hook flag eaten by the fake,
/// or a path silently left empty, which would make the fake act on the working directory.
#[test]
fn the_fakes_own_flags_come_first_and_the_rest_is_the_hooks() {
    let args = words(&[
        "--faults",
        "f",
        "--world",
        "w",
        "--record-dir",
        "r",
        "prepare",
    ]);
    let (own, rest) = own_flags(&args).unwrap();
    assert_eq!(
        own,
        Own {
            world: PathBuf::from("w"),
            faults: PathBuf::from("f"),
        }
    );
    assert_eq!(rest, ["--record-dir", "r", "prepare"]);
    // Nothing after the fake's flags is the hook's to refuse, not the fake's.
    let args = words(&["--world", "w", "--faults", "f"]);
    assert!(own_flags(&args).unwrap().1.is_empty());

    assert_eq!(
        own_flags(&words(&["--faults", "f", "prepare"])).unwrap_err(),
        OwnError::Required { flag: "--world" }
    );
    assert_eq!(
        own_flags(&words(&["--world", "w", "prepare"])).unwrap_err(),
        OwnError::Required { flag: "--faults" }
    );
    assert_eq!(
        own_flags(&words(&["--world"])).unwrap_err(),
        OwnError::NoValue { flag: "--world" }
    );
    assert_eq!(
        own_flags(&words(&["--world", "a", "--world", "b"])).unwrap_err(),
        OwnError::Twice { flag: "--world" }
    );
    assert_eq!(
        own_flags(&words(&["--world", "w", "--faults", "f", "--faults", "g"])).unwrap_err(),
        OwnError::Twice { flag: "--faults" }
    );
}

/// A directory flag with an empty value, or with another flag where its value should be, is
/// refused by name: an empty path would make the fake act on the working directory, and a
/// flag taken as a path would swallow the flag.
///
/// Catches: `--world ""` accepted (the host's files then land in the working directory), or
/// `--world --faults f` read as a world named `--faults`.
#[test]
fn a_directory_flag_that_is_empty_or_another_flag_is_refused() {
    assert_eq!(
        own_flags(&words(&["--world", "", "--faults", "f", "prepare"])).unwrap_err(),
        OwnError::Empty { flag: "--world" }
    );
    assert_eq!(
        own_flags(&words(&["--world", "w", "--faults", "", "prepare"])).unwrap_err(),
        OwnError::Empty { flag: "--faults" }
    );
    assert_eq!(
        own_flags(&words(&["--world", "--faults", "f", "prepare"])).unwrap_err(),
        OwnError::NoValue { flag: "--world" }
    );
    assert_eq!(
        own_flags(&words(&["--world", "w", "--faults", "--record-dir", "r"])).unwrap_err(),
        OwnError::NoValue { flag: "--faults" }
    );
}

/// Run as the harness would, with an empty `--world` and a step that would write the host
/// (`prepare` drains the node): the step is refused with `bad_arguments` naming the flag, and
/// the working directory is exactly as it was.
///
/// Catches: a refusal that comes after the fake has already written `node`, `mark` or
/// `helper.json` into the working directory.
#[test]
fn an_empty_world_changes_nothing_in_the_working_directory() {
    let listing = || {
        let mut names: Vec<_> = fs::read_dir(".")
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        names.sort();
        names
    };
    let before = listing();
    let dir = scratch("empty-world");
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let args = words(&[
        "update-hook",
        "--world",
        "",
        "--faults",
        &dir.join("faults").display().to_string(),
        "--record-dir",
        &dir.join("record").display().to_string(),
        "prepare",
    ]);
    assert_eq!(fake_run(&args, &mut out, &mut err), Outcome::Failure);
    let report = Report::from_stdout(&String::from_utf8(out).unwrap()).unwrap();
    assert_eq!(report.reason.as_deref(), Some("bad_arguments"));
    assert!(
        report
            .detail
            .unwrap()
            .contains("--world must name a directory")
    );
    assert_eq!(listing(), before);
    for name in ["node", "mark", "helper.json", "node.tmp", "mark.tmp"] {
        assert!(!Path::new(name).exists(), "{name}");
    }
    assert!(fs::read_dir(dir.join("record")).unwrap().next().is_none());
}

/// The program's entry: a command line it cannot use exits 1, as the hook's does.
///
/// Catches: an entry that panics, or exits 0, on a command line without the fake's flags.
#[test]
fn the_entry_exits_1_on_a_command_line_it_cannot_use() {
    let code = main(&[OsString::from("update-hook"), OsString::from("prepare")]);
    assert_eq!(
        format!("{code:?}"),
        format!("{:?}", std::process::ExitCode::from(1))
    );
}

/// A command line without the fake's flags ends like any bad command line of the hook: exit 1
/// and a last line with `bad_arguments` that names the flag, and nothing touched.
///
/// Catches: a fake that runs on an empty path, or panics, when the harness gives it no flags.
#[test]
fn a_command_line_without_the_fakes_flags_is_refused_with_a_last_line() {
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let outcome = fake_run(&words(&["update-hook", "prepare"]), &mut out, &mut err);
    assert_eq!(outcome, Outcome::Failure);
    let report = Report::from_stdout(&String::from_utf8(out).unwrap()).unwrap();
    assert_eq!(report.reason.as_deref(), Some("bad_arguments"));
    assert!(report.detail.unwrap().contains("--world is required"));
}

// ---- the steps over the fake host ----

/// The update over the fake host, step by step: `prepare` drains the serving node and marks the
/// cordon, `cutover` puts `2.0.0` on trial, `verify` sees it healthy, `cleanup` confirms it and
/// gives the node back, its mark gone.
///
/// Catches: a fake port that does not keep its state in its files, which the harness's
/// separate processes would then not see.
#[test]
fn an_update_over_the_fake_host_ends_on_the_new_release_serving() {
    let dir = scratch("update");
    assert_eq!(step(&dir, Step::Prepare).0, Outcome::Success);
    assert_eq!(
        (read(&dir, "node").as_str(), read(&dir, "mark").as_str()),
        ("drained", "ours")
    );
    assert_eq!(step(&dir, Step::Cutover).0, Outcome::Success);
    assert!(
        read(&dir, "helper.json").contains(r#""version":"2.0.0","before":"1.0.0","pending":true"#)
    );
    assert_eq!(step(&dir, Step::Verify).0, Outcome::Success);
    assert_eq!(step(&dir, Step::Cleanup).0, Outcome::Success);
    assert!(read(&dir, "helper.json").contains(r#""pending":false"#));
    assert_eq!(read(&dir, "node"), "serving");
    assert!(!dir.join("world/mark").exists());
}

/// A rollback after the switch puts `1.0.0` back and gives the node back.
///
/// Catches: a revert that leaves the trial on, or the node drained.
#[test]
fn a_rollback_over_the_fake_host_puts_the_old_release_back() {
    let dir = scratch("rollback");
    assert_eq!(step(&dir, Step::Prepare).0, Outcome::Success);
    assert_eq!(step(&dir, Step::Cutover).0, Outcome::Success);
    let (outcome, report) = step(&dir, Step::Rollback);
    assert_eq!(outcome, Outcome::Success);
    assert!(report.reversible);
    assert_eq!(
        read(&dir, "helper.json"),
        r#"{"version":"1.0.0","before":null,"pending":false}"#
    );
    assert_eq!(read(&dir, "node"), "serving");
}

/// A deferral of `prepare` is a lease that never goes: at the drain deadline (30 s of the
/// fake's clock, which moves only when the hook waits) the step exits 75 with the retry time
/// of the flags, and the cordon it placed is lifted.
///
/// Catches: a fake clock that does not move on a wait (the step would never end), or a
/// deferral the fake cannot make.
#[test]
fn a_deferred_prepare_exits_75_at_the_drain_deadline() {
    let dir = scratch("defer");
    fault(&dir, "prepare", "defer");
    let (outcome, report) = step(&dir, Step::Prepare);
    assert_eq!(outcome, Outcome::Deferred);
    assert_eq!(report.retry_after_secs, Some(60));
    assert_eq!(read(&dir, "node"), "serving");
}

/// Each fault fails the call its step makes: the install, the switch, the health, the revert,
/// the confirm.
///
/// Catches: a fault file read under the wrong step's name.
#[test]
fn each_fault_fails_its_steps_call() {
    let dir = scratch("faults");
    let p = paths(&dir);
    let local = FakeLocal(p.clone());
    for (name, word) in [
        ("prepare", "fail"),
        ("cutover", "fail"),
        ("cleanup", "fail"),
        ("rollback", "fail"),
        ("verify", "fail"),
    ] {
        fault(&dir, name, word);
    }
    assert_eq!(
        local.install(&v("2.0.0"), "stable").unwrap_err(),
        LocalError::Refused("verify_failed".to_owned())
    );
    assert_eq!(
        local.switch(&v("2.0.0")).unwrap_err(),
        LocalError::Failed("switch_failed".to_owned())
    );
    assert_eq!(
        local.confirm(&v("2.0.0")).unwrap_err(),
        LocalError::Failed("state_unavailable".to_owned())
    );
    assert_eq!(
        local.revert().unwrap_err(),
        LocalError::Failed("revert_failed".to_owned())
    );
    fs::write(
        dir.join("world/helper.json"),
        r#"{"version":"2.0.0","before":"1.0.0","pending":true}"#,
    )
    .unwrap();
    let health = FakeHealth(p.clone());
    assert_eq!(
        health.check(&host(), &v("2.0.0")).unwrap(),
        Health::Abandoned("the fault says so".to_owned())
    );
    // The fault is the trial's: the old release is healthy all the same.
    fs::write(
        dir.join("world/helper.json"),
        r#"{"version":"1.0.0","before":null,"pending":false}"#,
    )
    .unwrap();
    assert_eq!(
        health.check_held(&host(), &v("1.0.0")).unwrap(),
        Health::Healthy
    );
    assert_eq!(FakeDrain(p).leases(&host()), Ok(0));
}

/// The helper as the fake keeps it: what runs and what is on trial (with the helper's deadline),
/// an install of what runs already is current, a revert with nothing on trial is refused, the
/// health of a release that does not run is a wait, and the host reports no refusal.
///
/// Catches: a fake that takes a revert with nothing pending, or reads a trial it does not have.
#[test]
fn the_fake_helper_answers_from_its_file() {
    let dir = scratch("helper");
    let p = paths(&dir);
    let local = FakeLocal(p.clone());
    let state = local.query().unwrap();
    assert_eq!(state.version, Some(v("1.0.0")));
    assert!(state.pending.is_none() && !state.busy);
    assert!(local.install(&v("1.0.0"), "stable").unwrap().current);
    assert!(!local.install(&v("2.0.0"), "stable").unwrap().current);
    assert_eq!(
        local.revert().unwrap_err(),
        LocalError::Refused("nothing_pending".to_owned())
    );
    assert_eq!(local.switch(&v("2.0.0")).unwrap(), DEADLINE);
    let trial = local.query().unwrap().pending.expect("a trial");
    assert_eq!(
        (trial.version, trial.deadline_unix),
        (Some(v("2.0.0")), Some(DEADLINE))
    );
    let health = FakeHealth(p);
    assert_eq!(health.refusal(&host()), None);
    assert_eq!(
        health.check(&host(), &v("3.0.0")).unwrap(),
        Health::Waiting("2.0.0 runs, not 3.0.0".to_owned())
    );
    // A helper file that is not there reads as nothing installed.
    fs::remove_file(dir.join("world/helper.json")).unwrap();
    assert_eq!(local.query().unwrap().version, None);
}

/// The node as the fake keeps it: an operator's drain (no mark) reads as out of placement, and
/// draining it again adds no mark; this update's drain marks the node first and reads as in
/// placement; giving it back removes the mark, and a mark that is gone already is no error.
///
/// Catches: a fake that uncordons an operator's drain, or reads its own cordon as one.
#[test]
fn the_fake_node_tells_an_operators_drain_from_this_updates() {
    let dir = scratch("node");
    let drain = FakeDrain(paths(&dir));
    assert_eq!(drain.placed(&host()), Ok(Placed::In));
    drain.drain(&host(), None).unwrap();
    assert!(dir.join("world/mark").exists());
    assert_eq!(drain.placed(&host()), Ok(Placed::In));
    drain.undrain(&host()).unwrap();
    assert_eq!(read(&dir, "node"), "serving");
    assert!(!dir.join("world/mark").exists());
    drain.undrain(&host()).unwrap();

    fs::write(dir.join("world/node"), "drained").unwrap();
    assert_eq!(drain.placed(&host()), Ok(Placed::Out));
    drain.drain(&host(), Some(Duration::from_secs(30))).unwrap();
    assert!(!dir.join("world/mark").exists());
}

/// A host directory that cannot be written fails every call that changes it, with the call
/// named, and no panic.
///
/// Catches: a fake that unwraps a failed write, which the harness would see as a hook that died
/// of a signal.
#[test]
fn a_host_that_cannot_be_written_fails_each_change() {
    let dir = scratch("unwritable");
    fs::write(
        dir.join("world/helper.json"),
        r#"{"version":"2.0.0","before":"1.0.0","pending":true}"#,
    )
    .unwrap();
    // The host's files are read from the world directory, and written beside them: a world
    // that is a file reads as an empty host and refuses every write.
    let p = Arc::new(Paths {
        world: dir.join("world/helper.json"),
        faults: dir.join("faults"),
    });
    let local = FakeLocal(p.clone());
    let failed = |e: LocalError, what: &str| {
        assert!(
            matches!(&e, LocalError::Failed(text) if text.starts_with(what)),
            "{e:?}"
        );
    };
    failed(local.switch(&v("2.0.0")).unwrap_err(), "switch:");
    failed(local.confirm(&v("2.0.0")).unwrap_err(), "confirm:");
    // A world that reads but cannot take a write: each file is written to `<name>.tmp` first,
    // and a directory there refuses the write, whoever runs the test (root included, which
    // permission bits would not stop).
    let p_ok = paths(&dir);
    for tmp in ["helper.json.tmp", "mark.tmp", "node.tmp"] {
        fs::create_dir_all(dir.join("world").join(tmp).join("x")).unwrap();
    }
    failed(FakeLocal(p_ok.clone()).revert().unwrap_err(), "revert:");
    let drain = FakeDrain(p_ok.clone());
    assert!(
        drain.drain(&host(), None).is_err(),
        "the mark cannot be written"
    );
    assert!(
        drain.undrain(&host()).is_err(),
        "the node cannot be written"
    );
    assert_eq!(read(&dir, "node"), "serving");
    assert!(!dir.join("world/mark").exists());
    fs::remove_dir_all(dir.join("world/node.tmp")).unwrap();
    // A mark that cannot be removed is an error too.
    fs::create_dir(dir.join("world/mark")).unwrap();
    fs::write(dir.join("world/mark/x"), "").unwrap();
    assert!(FakeDrain(p_ok).undrain(&host()).is_err());
    assert!(FakeDrain(p).drain(&host(), None).is_err());
}

/// The fake's clock moves only when it is waited on, by exactly the wait; the host it plays is
/// node `n1`, a daemon's.
///
/// Catches: a fake clock that reads the system's, which would make a drain deadline take real
/// time.
#[test]
fn the_fake_clock_moves_only_when_waited_on() {
    let clock = FakeClock::default();
    assert_eq!(clock.now(), Duration::ZERO);
    clock.sleep(Duration::from_secs(30));
    assert_eq!(clock.now(), Duration::from_secs(30));
    let host = host();
    assert_eq!((host.alias.as_str(), host.role), ("n1", Role::Daemon));
    assert_eq!(host.node_id.as_deref(), Some("n1"));
}
