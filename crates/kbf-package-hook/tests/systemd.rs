//! The real host (`Systemd`), run against the fake program as `systemctl` and as `kbf` (it
//! answers as both over a world directory) and against programs every host has: no unit, no
//! root, no network.

use std::path::{Path, PathBuf};

use kbf_package_hook::{Check, Host, Systemd, UnitState};
use kbf_testkit::dir::TestDir;

fn fake() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_kbf-package-hook-fake"))
}

fn world() -> (TestDir, String) {
    let dir = TestDir::under(std::env::temp_dir(), "package-hook-systemd");
    std::fs::create_dir_all(dir.join("world").join("faults")).expect("world");
    let unit = dir.join("world").display().to_string();
    (dir, unit)
}

/// `state`, `stop`, `start` and `restart` run `systemctl <verb> <unit>` without a shell: the
/// state is the word systemctl prints, whatever its exit code, and the verbs change the unit.
#[test]
fn the_unit_is_read_and_driven_through_systemctl() {
    let (_dir, unit) = world();
    let host = Systemd::with_programs(fake(), fake());
    assert_eq!(host.state(&unit), Ok(UnitState::Inactive));
    host.start(&unit).expect("start");
    assert_eq!(host.state(&unit), Ok(UnitState::Active));
    host.restart(&unit).expect("restart");
    assert_eq!(host.state(&unit), Ok(UnitState::Active));
    host.stop(&unit).expect("stop");
    assert_eq!(host.state(&unit), Ok(UnitState::Inactive));
}

/// A verb systemctl fails is an error naming the verb, the unit, the exit code and what
/// systemctl said; a start fault of the fake is one.
#[test]
fn a_failing_systemctl_verb_is_an_error_naming_it() {
    let (dir, unit) = world();
    std::fs::write(dir.join("world").join("faults").join("start"), "fail").expect("fault");
    let host = Systemd::with_programs(fake(), fake());
    let error = host.start(&unit).expect_err("start fails");
    assert!(error.0.contains("systemctl start"), "{error}");
    assert!(error.0.contains("exited 1"), "{error}");
    let gone = Systemd::with_programs(dir.join("no-systemctl"), fake());
    let error = gone.stop(&unit).expect_err("no program");
    assert!(error.0.contains("cannot run"), "{error}");
    assert!(gone.state(&unit).is_err());
    let false_program = Systemd::with_programs(PathBuf::from("/bin/false"), fake());
    assert_eq!(
        false_program.state(&unit),
        Ok(UnitState::Other(String::new()))
    );
    assert!(false_program.restart(&unit).is_err());
}

/// The installed version is the native version of what `kbf --version --json` prints.
#[test]
fn the_installed_version_is_the_native_version_of_the_version_line() {
    let host = Systemd::with_programs(fake(), fake());
    assert_eq!(host.installed_version().as_deref(), Ok("0.1.7"));
}

/// A `kbf` that is missing, that exits non-zero, or that prints no version line is an error.
#[test]
fn an_installed_kbf_that_does_not_name_its_version_is_an_error() {
    let dir = TestDir::under(std::env::temp_dir(), "package-hook-systemd-version");
    let with = |kbf: PathBuf| Systemd::with_programs(fake(), kbf);
    let error = with(dir.join("absent"))
        .installed_version()
        .expect_err("absent");
    assert!(error.0.contains("cannot run"), "{error}");
    // The test program itself does not know `--version`: it exits non-zero.
    let me = std::env::current_exe().expect("current exe");
    let error = with(me).installed_version().expect_err("fails");
    assert!(error.0.contains("kbf --version --json exited"), "{error}");
    // `echo` exits 0 and prints its arguments, which are no version line.
    let error = with(PathBuf::from("/bin/echo"))
        .installed_version()
        .expect_err("not json");
    assert!(error.0.contains("did not name a version"), "{error}");
}

fn check(program: &Path, mode: &str) -> Check {
    Systemd::with_programs(fake(), fake())
        .check(program, mode)
        .expect("a check answers")
}

/// The staged binary's exit code is the answer: 0 ready, 75 not ready yet, 2 refused, and any
/// other end (another code, a signal, a binary that will not start) a failure with the reason.
#[test]
fn the_staged_binary_is_run_with_its_mode_and_its_exit_code_is_the_answer() {
    assert_eq!(check(&fake(), "ready"), Check::Ready);
    assert_eq!(check(&fake(), "not-ready"), Check::NotReady);
    assert_eq!(check(&fake(), "refused"), Check::Refused);
    let Check::Failed(why) = check(&fake(), "other") else {
        panic!("another code is a failure")
    };
    assert!(why.contains("--check exited 3"), "{why}");
    let Check::Failed(why) = check(&fake(), "crash") else {
        panic!("a signal is a failure")
    };
    assert!(why.contains("killed by a signal"), "{why}");
    let Check::Failed(why) = check(Path::new("/no/such/kbf"), "ready") else {
        panic!("a binary that will not start is a failure")
    };
    assert!(why.contains("cannot run /no/such/kbf"), "{why}");
}

/// `Systemd::new` names the system's own programs.
#[test]
fn the_default_host_uses_the_system_programs() {
    // Only its construction: the real systemctl is never run by a test.
    let _host: Systemd = Systemd::default();
}
