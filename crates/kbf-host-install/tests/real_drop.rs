//! The runner's privilege drop to `nobody`, made for real by a root process on macOS and on
//! Linux: a program started with `run_as` runs, ends with status 0, and is `nobody` with the
//! groups it should have.
//!
//! On macOS a process that calls `chroot(2)` and then executes a program is killed by the
//! system at once (`SIGKILL`, even for `/` and `/usr/bin/true`), so a drop through
//! `chroot(8)` ends every command it starts that way. No unit test sees that: it needs the
//! system and root.
//!
//! These tests need root, so they are `#[ignore]`d and run through `cargo xtask sudo-test
//! -p kbf-host-install --test real_drop` in the `host-install-mac` workflow (a GitHub-hosted
//! macOS runner) and in the `host-install-linux` workflow (a hosted Ubuntu runner), both with
//! passwordless sudo. Run without root they fail, and say to use the xtask.

#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::time::Duration;

use kbf_host_install::mac_tools::{Cmd, Exec, Identity, Runner, SystemRunner};
use kbf_testkit::dir::TestDir;

const LIMIT: Duration = Duration::from_secs(30);

fn assert_root() {
    assert!(
        rustix::process::geteuid().is_root(),
        "run through `cargo xtask sudo-test`"
    );
}

/// The account `nobody`: `dscl` says on macOS (the same ids the install reads); 65534 on Linux.
fn nobody() -> Identity {
    #[cfg(target_os = "macos")]
    {
        use kbf_host_install::mac_tools::tools;
        let exec = run(&tools::dscl_ids("nobody", LIMIT));
        assert!(exec.success(), "{}", exec.stderr_text());
        let text = exec.stdout_text();
        tools::parse_ids(&text).unwrap_or_else(|| panic!("the ids are not read from: {text}"))
    }
    #[cfg(not(target_os = "macos"))]
    {
        Identity {
            uid: 65534,
            gid: 65534,
        }
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
/// with status 0. (A drop through `chroot(8)` on macOS ends it with `SIGKILL`.)
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
/// effective uid and gid are `nobody`'s, and its supplementary groups are only `nobody`'s
/// group on macOS (exactly that one, never group 0) and none on Linux.
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
    let want: Vec<u32> = if cfg!(target_os = "macos") {
        vec![ids.gid]
    } else {
        vec![]
    };
    assert_eq!(numbers("groups:"), want, "supplementary groups");
    // No drop at all is root, so the probe above is a probe of the drop.
    cmd.run_as = None;
    let as_root = run(&cmd).stdout_text();
    assert!(as_root.contains("ids: 0 0 0 0"), "{as_root}");
}
