//! The fake host and the fake program, in process.

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::ExitCode;

use kbf_testkit::dir::TestDir;

use crate::fake::{FAKE_VERSION_LINE, FakeHost, main};
use crate::host::{Check, Host, UnitState};

fn os(args: &[&str]) -> Vec<OsString> {
    args.iter().map(OsString::from).collect()
}

fn world() -> (TestDir, FakeHost) {
    let dir = TestDir::under(std::env::temp_dir(), "package-hook-fake");
    std::fs::create_dir_all(dir.join("world")).expect("world");
    std::fs::create_dir_all(dir.join("faults")).expect("faults");
    let host = FakeHost::new(
        dir.join("world"),
        dir.join("faults"),
        Some("2.0.0".to_owned()),
        Some("1.0.0".to_owned()),
    );
    (dir, host)
}

/// The world starts with the unit stopped; start installs the target, restart and stop put the
/// version from before back.
#[test]
fn the_fake_unit_follows_its_verbs() {
    let (_dir, host) = world();
    assert_eq!(host.state("u"), Ok(UnitState::Inactive));
    assert!(host.installed_version().is_err());
    host.start("u").expect("start");
    assert_eq!(host.state("u"), Ok(UnitState::Active));
    assert_eq!(host.installed_version().as_deref(), Ok("2.0.0"));
    host.restart("u").expect("restart");
    assert_eq!(host.installed_version().as_deref(), Ok("1.0.0"));
    host.start("u").expect("start");
    host.stop("u").expect("stop");
    assert_eq!(host.state("u"), Ok(UnitState::Inactive));
    assert_eq!(host.installed_version().as_deref(), Ok("1.0.0"));
}

/// Without versions to move between, start and restart change the unit only.
#[test]
fn the_fake_keeps_the_version_when_it_has_none_to_install() {
    let dir = TestDir::under(std::env::temp_dir(), "package-hook-fake-none");
    std::fs::create_dir_all(dir.join("world")).expect("world");
    let host = FakeHost::new(dir.join("world"), dir.join("faults"), None, None);
    host.start("u").expect("start");
    host.restart("u").expect("restart");
    assert!(host.installed_version().is_err());
    assert_eq!(host.state("u"), Ok(UnitState::Active));
}

/// The faults are files: the check refuses or is not ready, a start fails, and a started unit
/// shows the state the verify fault names (a stopped unit does not).
#[test]
fn the_fake_faults_are_files() {
    let (dir, host) = world();
    let kbf = PathBuf::from("kbf");
    assert_eq!(host.check(&kbf, "m"), Ok(Check::Ready));
    std::fs::write(dir.join("faults").join("check"), "refuse\n").expect("fault");
    assert_eq!(host.check(&kbf, "m"), Ok(Check::Refused));
    std::fs::write(dir.join("faults").join("check"), "not-ready").expect("fault");
    assert_eq!(host.check(&kbf, "m"), Ok(Check::NotReady));
    std::fs::write(dir.join("faults").join("verify"), "failed").expect("fault");
    assert_eq!(host.state("u"), Ok(UnitState::Inactive));
    host.start("u").expect("start");
    assert_eq!(host.state("u"), Ok(UnitState::Failed));
    std::fs::write(dir.join("faults").join("start"), "fail").expect("fault");
    assert!(host.start("u").is_err());
}

/// A world that cannot be written is an error of the host, not a panic.
#[test]
fn the_fake_reports_a_world_it_cannot_write() {
    let dir = TestDir::under(std::env::temp_dir(), "package-hook-fake-ro");
    let host = FakeHost::new(dir.join("absent"), dir.join("faults"), None, None);
    let error = host.stop("u").expect_err("no world");
    assert!(
        error.0.starts_with("cannot write unit in the world"),
        "{error}"
    );
}

/// As `systemctl` the program answers from the world named by the unit argument: the state word,
/// exit 0 only for an active unit, and the verbs change the world.
#[test]
fn the_program_answers_as_systemctl_over_a_world() {
    let (dir, _host) = world();
    let world = dir.join("world").display().to_string();
    assert_eq!(main(&os(&["is-active", &world])), ExitCode::from(3));
    assert_eq!(main(&os(&["start", &world])), ExitCode::SUCCESS);
    assert_eq!(main(&os(&["is-active", &world])), ExitCode::SUCCESS);
    assert_eq!(main(&os(&["restart", &world])), ExitCode::SUCCESS);
    assert_eq!(main(&os(&["stop", &world])), ExitCode::SUCCESS);
    // A world that is not there: the write fails.
    let absent = dir.join("absent").display().to_string();
    assert_eq!(main(&os(&["stop", &absent])), ExitCode::from(1));
    assert_eq!(main(&os(&["start", &absent])), ExitCode::from(1));
    assert_eq!(main(&os(&["restart", &absent])), ExitCode::from(1));
    assert_eq!(main(&os(&["stop"])), ExitCode::from(1));
}

/// As the installed `kbf` it prints its version line, and as a staged `kbf` it exits as its mode
/// says.
#[test]
fn the_program_answers_as_kbf() {
    assert_eq!(main(&os(&["--version", "--json"])), ExitCode::SUCCESS);
    assert_eq!(
        crate::parse_version_output(FAKE_VERSION_LINE).as_deref(),
        Ok("0.1.7")
    );
    assert_eq!(main(&os(&["--mode=ready", "--check"])), ExitCode::SUCCESS);
    assert_eq!(
        main(&os(&["--mode=not-ready", "--check"])),
        ExitCode::from(75)
    );
    assert_eq!(main(&os(&["--mode=refused", "--check"])), ExitCode::from(2));
    assert_eq!(main(&os(&["--mode=other", "--check"])), ExitCode::from(3));
}

/// As the hook it needs a world that is a directory and a faults flag, and refuses with the
/// contract's last line and exit 1 when it has neither; with both, it runs the step.
#[test]
fn the_program_as_the_hook_checks_its_flags() {
    let (dir, _host) = world();
    let one = ExitCode::from(1);
    assert_eq!(main(&os(&[])), one);
    assert_eq!(main(&os(&["package-hook"])), one);
    assert_eq!(main(&os(&["package-hook", "--world", "w"])), one);
    let faults = dir.join("faults").display().to_string();
    assert_eq!(
        main(&os(&[
            "package-hook",
            "--world",
            "",
            "--faults",
            &faults,
            "verify"
        ])),
        one
    );
    let absent = dir.join("absent").display().to_string();
    assert_eq!(
        main(&os(&[
            "package-hook",
            "--world",
            &absent,
            "--faults",
            &faults,
            "verify"
        ])),
        one
    );
    let world = dir.join("world").display().to_string();
    // A step with no params file still ends in the contract's failure.
    assert_eq!(
        main(&os(&[
            "package-hook",
            "--world",
            &world,
            "--faults",
            &faults,
            "verify"
        ])),
        one
    );
}
