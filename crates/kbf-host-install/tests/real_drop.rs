//! The runner's privilege drop to `nobody`, made for real by a root process on Linux: a program
//! started with `run_as` runs, ends with status 0, and is `nobody` with no supplementary group.
//!
//! These tests need root, so they are `#[ignore]`d and run through `cargo xtask sudo-test
//! -p kbf-host-install --test real_drop` in the `host-install-linux` workflow (a hosted Ubuntu
//! runner) with passwordless sudo. Run without root they fail, and say to use the xtask.

#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::time::Duration;

use kbf_host_install::test_runner::{Cmd, Exec, Identity, Runner, SystemRunner};
use kbf_testkit::dir::TestDir;

const LIMIT: Duration = Duration::from_secs(30);

fn assert_root() {
    assert!(
        rustix::process::geteuid().is_root(),
        "run through `cargo xtask sudo-test`"
    );
}

/// The account `nobody`: 65534.
fn nobody() -> Identity {
    Identity {
        uid: 65534,
        gid: 65534,
    }
}

fn run(cmd: &Cmd) -> Exec {
    SystemRunner
        .run(cmd)
        .unwrap_or_else(|e| panic!("{} did not run: {e}", cmd.program))
}

/// Not a test of its own: the process `a_dropped_child_has_the_ids_and_groups_of_nobody`
/// starts, which prints what it is (`getgroups`, not the memberships that `id` prints).
#[test]
#[ignore = "run by a_dropped_child_has_the_ids_and_groups_of_nobody, as another user"]
fn print_my_ids() {
    let groups: Vec<String> = rustix::process::getgroups()
        .unwrap()
        .iter()
        .map(|g| g.as_raw().to_string())
        .collect();
    println!("groups: {}", groups.join(" "));
    println!(
        "ids: {} {} {} {}",
        rustix::process::getuid().as_raw(),
        rustix::process::geteuid().as_raw(),
        rustix::process::getgid().as_raw(),
        rustix::process::getegid().as_raw()
    );
}

/// Catches a drop that cannot start a system program: `/usr/bin/true` run as `nobody` ends
/// with status 0.
#[test]
#[ignore = "needs root: cargo xtask sudo-test"]
fn a_system_program_runs_as_nobody_and_succeeds() {
    assert_root();
    let mut cmd = Cmd {
        program: "/usr/bin/true".into(),
        timeout: Some(LIMIT),
        ..Cmd::default()
    };
    cmd.run_as = Some(nobody());
    let exec = run(&cmd);
    assert!(
        exec.success(),
        "/usr/bin/true as nobody ended with {}: {}",
        exec.ended(),
        exec.stderr_text()
    );
}

/// Catches a drop that leaves root's identity or groups on the child: the child's real and
/// effective uid and gid are `nobody`'s, and it has no supplementary group.
#[test]
#[ignore = "needs root: cargo xtask sudo-test"]
fn a_dropped_child_has_the_ids_and_groups_of_nobody() {
    assert_root();
    let ids = nobody();
    assert_ne!(ids.uid, 0);
    assert_ne!(ids.gid, 0);
    // The test binary again, in a directory `nobody` can reach.
    let dir = TestDir::under("/var/tmp", "kbf-real-drop");
    let copy = dir.join("probe");
    fs::copy(std::env::current_exe().unwrap(), &copy).unwrap();
    fs::set_permissions(&copy, fs::Permissions::from_mode(0o755)).unwrap();
    fs::set_permissions(&*dir, fs::Permissions::from_mode(0o755)).unwrap();
    let mut cmd = Cmd {
        program: copy.display().to_string(),
        args: ["--ignored", "--exact", "print_my_ids", "--nocapture"]
            .map(str::to_owned)
            .to_vec(),
        timeout: Some(LIMIT),
        run_as: Some(ids),
        ..Cmd::default()
    };
    let exec = run(&cmd);
    assert!(
        exec.success(),
        "the probe ended with {}: {}",
        exec.ended(),
        exec.stderr_text()
    );
    let text = exec.stdout_text();
    let numbers = |key: &str| -> Vec<u32> {
        let line = text
            .lines()
            .find_map(|line| line.strip_prefix(key))
            .unwrap_or_else(|| panic!("no {key} line: {text}"));
        line.split_whitespace()
            .map(|n| n.parse().unwrap())
            .collect()
    };
    assert_eq!(
        numbers("ids:"),
        [ids.uid, ids.uid, ids.gid, ids.gid],
        "uid, euid, gid, egid"
    );
    assert_eq!(
        numbers("groups:"),
        Vec::<u32>::new(),
        "supplementary groups"
    );
    // No drop at all is root, so the probe above is a probe of the drop.
    cmd.run_as = None;
    let as_root = run(&cmd).stdout_text();
    assert!(as_root.contains("ids: 0 0 0 0"), "{as_root}");
}
