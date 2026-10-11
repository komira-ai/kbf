use std::io;
use std::process::Command;

use super::*;
use crate::exec::HostExec;
use crate::path::Root;
use crate::testkit::{FakeExec, reply, scratch};

const PASSWD: &str = "kbf:x:990:991:kbf build daemon:/var/lib/kbf:/usr/sbin/nologin\n";

/// A host whose `getent` and `id -G` print `passwd` and `groups`.
fn host(passwd: &'static str, groups: &'static str) -> FakeExec {
    let mut exec = FakeExec::new(0);
    exec.answer = Box::new(move |cmd| {
        Ok(match cmd.program.as_str() {
            GETENT if passwd.is_empty() => reply(2, "", ""),
            GETENT => reply(0, passwd, ""),
            _ => reply(0, groups, ""),
        })
    });
    exec
}

/// Catches a lookup that reads the wrong passwd field (the gecos for the home, the gid
/// for the uid) or drops the supplementary groups `id -G` lists.
#[test]
fn a_user_is_read_with_its_ids_groups_and_home() {
    let exec = host(PASSWD, "991 44 990\n");
    let user = lookup(&exec, "kbf").expect("lookup").expect("a user");
    let want = RunUser {
        name: "kbf".to_owned(),
        uid: 990,
        gid: 991,
        groups: vec![991, 44, 990],
        home: "/var/lib/kbf".to_owned(),
    };
    assert_eq!(user, want);
    let asked: Vec<String> = exec
        .queries
        .borrow()
        .iter()
        .map(ToString::to_string)
        .collect();
    assert_eq!(
        asked,
        ["/usr/bin/getent passwd -- kbf", "/usr/bin/id -G -- kbf"]
    );
}

/// Catches a missing user (`getent` exits 2) read as an error, or as a user.
#[test]
fn a_missing_user_is_none() {
    let exec = host("", "");
    assert_eq!(lookup(&exec, "kbf").expect("lookup"), None);
}

/// Catches an answer the lookup does not understand being taken as a user: each row is
/// one malformed answer, and each must be refused.
#[test]
fn an_answer_that_is_not_a_user_is_refused() {
    let rows: [(&str, &'static str, &'static str, i32); 7] = [
        ("too few fields", "kbf:x:990:991\n", "991\n", 0),
        ("uid not a number", "kbf:x:u:991::/h:/s\n", "991\n", 0),
        ("gid not a number", "kbf:x:990:g::/h:/s\n", "991\n", 0),
        ("another user's line", "kbd:x:990:991::/h:/s\n", "991\n", 0),
        ("a group not a number", PASSWD, "991 staff\n", 0),
        ("no primary group", PASSWD, "44\n", 0),
        ("id fails", PASSWD, "", 1),
    ];
    for (why, passwd, groups, id_code) in rows {
        let mut exec = FakeExec::new(0);
        exec.answer = Box::new(move |cmd| {
            Ok(if cmd.program.as_str() == GETENT {
                reply(0, passwd, "")
            } else {
                reply(id_code, groups, "no such user")
            })
        });
        let err = lookup(&exec, "kbf").expect_err(why);
        let ok = match id_code {
            0 => matches!(err, UserError::Output { .. }),
            _ => matches!(err, UserError::Query { .. }),
        };
        assert!(ok, "{why}: {err}");
    }
}

/// Catches a failing `getent` (exit 1: no user database) or one that cannot start being
/// read as "no such user".
#[test]
fn a_getent_that_fails_is_an_error() {
    let mut exec = FakeExec::new(0);
    exec.answer = Box::new(|_| Ok(reply(1, "", "nsswitch broken")));
    let err = lookup(&exec, "kbf").expect_err("exit 1");
    assert!(err.to_string().contains("nsswitch broken"), "{err}");
    exec.answer = Box::new(|_| Err(io::Error::from(io::ErrorKind::NotFound)));
    let err = lookup(&exec, "kbf").expect_err("no getent");
    assert!(matches!(err, UserError::Query { .. }), "{err}");
}

fn me() -> (u32, u32) {
    (
        rustix::process::getuid().as_raw(),
        rustix::process::getgid().as_raw(),
    )
}

fn id_as(user: RunUser, flag: &str) -> io::Result<std::process::Output> {
    let exec = HostExec::new(Root::host());
    exec.query(
        &Cmd::new(HostPath::new(ID).expect("path"))
            .arg(flag)
            .user(user),
    )
}

/// Run as a user, not root. Catches a command whose user is ignored: then `id` would
/// run as the test's user and succeed; the switch must fail before it starts.
#[test]
fn as_a_user_a_command_for_another_user_never_starts() {
    let (uid, gid) = me();
    let err = id_as(other_user(uid + 1, gid), "-u").expect_err("only root may switch users");
    assert_eq!(err.raw_os_error(), Some(libc::EPERM), "{err}");
    // In this process, the closure the child would run: its first call already needs
    // root.
    let err = switch_to(&other_user(uid, gid))().expect_err("setgroups needs root");
    assert_eq!(err.raw_os_error(), Some(libc::EPERM), "{err}");
}

const ROOT_TEST: &str = "user::tests::as_root_a_child_has_the_users_ids_and_groups";

fn sorted(text: &str) -> Vec<u32> {
    let mut ids: Vec<u32> = text
        .split_whitespace()
        .map(|w| w.parse().expect("a number"))
        .collect();
    ids.sort_unstable();
    ids
}

/// Root only; [`the_root_tests_pass_under_sudo`] runs it. Catches a child that keeps
/// root's groups (the user's groups were never set: `setgroups` dropped) or root's
/// uid or gid, switch calls in the wrong order (once the uid is dropped, setgid and
/// setgroups fail), and a switch that goes on after a refused setgid or setuid.
#[test]
#[ignore = "needs root: the_root_tests_pass_under_sudo runs it through sudo"]
fn as_root_a_child_has_the_users_ids_and_groups() {
    assert_eq!(rustix::process::geteuid().as_raw(), 0, "run as root");
    // The driver checks this line: under a coverage run the profile must reach here.
    println!("profile file: {:?}", std::env::var_os("LLVM_PROFILE_FILE"));
    let nobody = RunUser {
        name: "nobody".to_owned(),
        uid: 65534,
        gid: 65534,
        groups: vec![65534, 4242],
        home: "/".to_owned(),
    };
    let text = |flag| {
        let out = id_as(nobody.clone(), flag).expect("id runs");
        assert!(out.status.success(), "{flag}: {out:?}");
        String::from_utf8(out.stdout).expect("utf-8")
    };
    assert_eq!(text("-u").trim(), "65534");
    assert_eq!(text("-g").trim(), "65534");
    assert_eq!(sorted(&text("-G")), [4242, 65534], "the child's groups");

    // This process now becomes the user sudo ran it for, through the same calls, so
    // the coverage profile it writes at exit is that user's to merge.
    let id = |key| -> u32 {
        std::env::var(key)
            .expect("sudo sets SUDO_UID and SUDO_GID")
            .parse()
            .expect("a number")
    };
    let (uid, gid) = (id("SUDO_UID"), id("SUDO_GID"));
    // An id the kernel refuses (-1 names no one) stops the switch at that call, so the
    // process never goes on with root's group or uid.
    let err = become_user(uid, u32::MAX, &[gid]).expect_err("gid -1");
    assert_eq!(err.raw_os_error(), Some(libc::EINVAL), "setgid: {err}");
    let err = become_user(u32::MAX, gid, &[gid]).expect_err("uid -1");
    assert_eq!(err.raw_os_error(), Some(libc::EINVAL), "setuid: {err}");
    assert_eq!(rustix::process::geteuid().as_raw(), 0, "still root");
    become_user(uid, gid, &[gid, 4242]).expect("root may switch");
    assert_eq!(me(), (uid, gid));
    let groups = rustix::process::getgroups().expect("getgroups");
    let mut groups: Vec<u32> = groups.iter().map(|g| g.as_raw()).collect();
    groups.sort_unstable();
    let mut want = vec![gid, 4242];
    want.sort_unstable();
    assert_eq!(groups, want, "this process's groups");
}

/// Whether the root tests run here: they need passwordless sudo, and CI (which sets
/// `CI`) must have it, so a runner without it fails instead of skipping them.
fn root_tests_run(sudo: bool, ci: bool) -> Result<bool, &'static str> {
    match (sudo, ci) {
        (true, _) => Ok(true),
        (false, false) => {
            eprintln!("skipped: no passwordless sudo here, so the root tests did not run");
            Ok(false)
        }
        (false, true) => Err("CI must allow passwordless sudo: the root tests did not run"),
    }
}

/// Catches a CI runner without sudo silently skipping the root tests.
#[test]
fn the_root_tests_are_skipped_only_off_ci() {
    assert_eq!(root_tests_run(true, true), Ok(true));
    assert_eq!(root_tests_run(true, false), Ok(true));
    assert_eq!(root_tests_run(false, false), Ok(false));
    assert!(root_tests_run(false, true).is_err());
}

/// Runs the root tests in this binary through `sudo -n`; off CI without passwordless
/// sudo it says so and passes. Catches a root test that fails or is not found (a
/// filter that matches nothing passes with "0 passed").
#[test]
fn the_root_tests_pass_under_sudo() {
    let sudo = Command::new("/usr/bin/sudo")
        .args(["-n", "true"])
        .output()
        .is_ok_and(|o| o.status.success());
    if root_tests_run(sudo, std::env::var_os("CI").is_some()).expect("sudo") {
        // sudo clears the environment. A coverage run names its profile file in
        // LLVM_PROFILE_FILE; `env` sets it again for the root process, which (as the
        // caller by then) writes its profile there at exit.
        let profile = std::env::var_os("LLVM_PROFILE_FILE");
        let mut sudo = Command::new("/usr/bin/sudo");
        sudo.args(["-n", "/usr/bin/env"]);
        if let Some(p) = &profile {
            let mut set = std::ffi::OsString::from("LLVM_PROFILE_FILE=");
            set.push(p);
            sudo.arg(set);
        }
        let out = sudo
            .arg(std::env::current_exe().expect("the test binary"))
            .args([
                "--exact",
                ROOT_TEST,
                "--ignored",
                "--test-threads=1",
                "--nocapture",
            ])
            .current_dir(scratch("sudo"))
            .output()
            .expect("sudo runs");
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(out.status.success(), "{stdout}\n{stderr}");
        assert!(stdout.contains("test result: ok. 1 passed"), "{stdout}");
        let seen = format!("profile file: {profile:?}");
        assert!(
            stdout.contains(&seen),
            "the root test saw another profile: {stdout}"
        );
        assert!(!stderr.contains("LLVM Profile Error"), "{stderr}");
    }
}

fn other_user(uid: u32, gid: u32) -> RunUser {
    RunUser {
        name: "other".to_owned(),
        uid,
        gid,
        groups: vec![gid],
        home: "/".to_owned(),
    }
}
