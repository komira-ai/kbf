//! `node-admin`'s guards, over a fake Mac: the verb list, root only, the caller sudo
//! names, the tree check, the stale-file sweep, the audit log and the lock. Each test
//! names the defect it catches.

mod admin_fake;

use std::ffi::OsString;
use std::fs::{self, File};
use std::os::unix::ffi::OsStringExt as _;
use std::os::unix::fs::PermissionsExt as _;
use std::time::Duration;

use admin_fake::{FakeMac, NOW};
use kbf_mac_session::node_admin::{self, LOCK_WAIT, Sudo, Verb};
use rustix::fs::FlockOperation;

fn log_lines(mac: &FakeMac) -> Vec<String> {
    let text = fs::read_to_string(mac.admin_log()).unwrap_or_default();
    text.lines().map(str::to_owned).collect()
}

fn chmod(path: &std::path::Path, mode: u32) {
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
}

/// Catches: a call with no verb that does something instead of printing the usage.
#[test]
fn no_verb_prints_the_usage() {
    let mac = FakeMac::new("no-verb");
    mac.run(&[]).usage();
    assert!(mac.calls().is_empty(), "{:?}", mac.calls());
}

/// Catches: a verb list that is not closed (a verb this mode does not have, half a
/// verb, or a verb and its object joined into one argument is run or guessed at).
#[test]
fn only_the_listed_verbs_run() {
    let mac = FakeMac::new("verbs");
    for args in [
        &["daemon", "exec", "/bin/sh"][..],
        &["role-user", "status"],
        &["phase0"],
        &["phase0 status"],
        &["phase0", "Status"],
    ] {
        mac.run(args).usage();
    }
    assert!(mac.calls().is_empty(), "{:?}", mac.calls());
}

/// Catches: a verb that takes no arguments accepting extra ones (each is refused, and
/// nothing is read or changed).
#[test]
fn extra_arguments_are_refused() {
    let mac = FakeMac::new("extra");
    for verb in ["status", "apply", "revert"] {
        mac.run(&["phase0", verb, "extra"])
            .refused_for(&format!("'phase0 {verb}' takes no arguments (got 1)"));
    }
    assert!(mac.calls().is_empty(), "{:?}", mac.calls());
    assert!(!mac.record().exists());
}

/// Catches: the verbs' names and which of them change the Mac drifting apart from
/// what the lock is taken for.
#[test]
fn only_status_runs_without_the_lock() {
    let parse = |words: &[&str]| Verb::parse(&words.iter().map(OsString::from).collect::<Vec<_>>());
    let verbs = [Verb::Phase0Status, Verb::Phase0Apply, Verb::Phase0Revert];
    let names: Vec<String> = verbs.iter().map(ToString::to_string).collect();
    assert_eq!(names, ["phase0 status", "phase0 apply", "phase0 revert"]);
    for (verb, name) in verbs.iter().zip(&names) {
        let words: Vec<&str> = name.split(' ').collect();
        assert_eq!(parse(&words), Ok(*verb));
    }
    let changes: Vec<bool> = verbs.iter().map(|verb| verb.changes()).collect();
    assert_eq!(changes, [false, true, true]);
}

/// Catches: the helper running for a caller that is not root (nothing is logged,
/// read or changed).
#[test]
fn it_refuses_unless_root() {
    let mac = FakeMac::new("not-root");
    mac.state.borrow_mut().euid = 501;
    mac.run(&["phase0", "apply"]).refused_for("run it as root");
    assert!(mac.calls().is_empty());
    assert!(log_lines(&mac).is_empty());
}

/// Catches: a `SUDO_UID` that is not a number, or only one of `SUDO_USER` and
/// `SUDO_UID`, being logged as some caller.
#[test]
fn the_sudo_caller_must_be_well_formed() {
    let mac = FakeMac::new("sudo");
    let args = [OsString::from("phase0"), OsString::from("status")];
    let sudo = |user: Option<&str>, uid: Option<&str>| Sudo {
        user: user.map(OsString::from),
        uid: uid.map(OsString::from),
    };
    mac.run_as(&sudo(Some("ops"), Some("5x1")), &args)
        .refused_for("SUDO_UID \"5x1\" is not a number");
    let not_utf8 = Sudo {
        user: Some("ops".into()),
        uid: Some(OsString::from_vec(vec![0xff])),
    };
    mac.run_as(&not_utf8, &args).refused_for("is not a number");
    mac.run_as(&sudo(Some("ops"), None), &args)
        .refused_for("only one of them is set");
    mac.run_as(&sudo(None, Some("501")), &args)
        .refused_for("only one of them is set");
    assert!(log_lines(&mac).is_empty());
    // Root without sudo: logged as root.
    mac.run_as(&sudo(None, None), &args).succeeded();
    let lines = log_lines(&mac);
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(
        lines[0].contains(" caller=\"root\" uid=0 argv: "),
        "{lines:?}"
    );
}

/// Catches: an audit line an argument can forge or break: a shell expansion run or
/// lost, a newline that starts a second line, a terminal escape written raw.
#[test]
fn the_log_quotes_every_argument() {
    let mac = FakeMac::new("log-quoted");
    mac.run(&["phase0", "status", "$(touch /x)", "a\nb", "\u{1b}]0;x\u{7}"])
        .refused_for("takes no arguments (got 3)");
    assert_eq!(
        log_lines(&mac),
        [concat!(
            "2027-01-15T08:00:00Z caller=\"ops\" uid=501 argv: \"phase0\" \"status\" ",
            "\"$(touch /x)\" \"a\\nb\" \"\\u{1b}]0;x\\u{7}\""
        )]
    );
}

/// Catches: a call that is not logged because it was refused or not understood.
#[test]
fn every_call_is_logged() {
    let mac = FakeMac::new("log-every");
    mac.run(&[]).usage();
    mac.run(&["daemon", "exec", "/bin/sh"]).usage();
    mac.run(&["phase0", "status", "x"])
        .refused_for("takes no arguments");
    mac.run(&["phase0", "status"]).succeeded();
    let lines = log_lines(&mac);
    let argv: Vec<&str> = lines
        .iter()
        .map(|line| line.split_once("argv:").unwrap().1)
        .collect();
    assert_eq!(
        argv,
        [
            "",
            " \"daemon\" \"exec\" \"/bin/sh\"",
            " \"phase0\" \"status\" \"x\"",
            " \"phase0\" \"status\""
        ]
    );
}

/// Catches: a caller filling the disk through the log: an argument kept whole, or
/// every argument of a long list kept.
#[test]
fn the_log_cuts_long_arguments_and_long_lists() {
    let long = "a".repeat(5000);
    let line = node_admin::log_line(
        std::time::UNIX_EPOCH + Duration::from_secs(NOW),
        "ops",
        501,
        &[OsString::from(&long)],
    );
    let kept = format!(" argv: \"{}\"...[5000 bytes]\n", "a".repeat(512));
    assert!(line.ends_with(&kept), "{line}");
    let exact = "b".repeat(512);
    let line = node_admin::log_line(std::time::UNIX_EPOCH, "ops", 501, &[OsString::from(&exact)]);
    assert!(line.ends_with(&format!("\"{exact}\"\n")), "{line}");
    let many: Vec<OsString> = (1..=100).map(|n| OsString::from(n.to_string())).collect();
    let line = node_admin::log_line(std::time::UNIX_EPOCH, "ops", 501, &many);
    assert!(
        line.ends_with(" \"15\" \"16\" ...(100 arguments in all)\n"),
        "{line}"
    );
    let sixteen = &many[..16];
    let line = node_admin::log_line(std::time::UNIX_EPOCH, "ops", 501, sixteen);
    assert!(line.ends_with(" \"16\"\n"), "{line}");
    assert!(line.starts_with("1970-01-01T00:00:00Z "), "{line}");
}

/// Catches: a log that grows without bound (never renamed past 1 MiB), or one renamed
/// already at exactly 1 MiB.
#[test]
fn the_log_is_renamed_past_one_mebibyte() {
    let mac = FakeMac::new("log-rotate");
    let log = mac.admin_log();
    let mut exact = vec![b'x'; 1024 * 1024];
    exact[1024 * 1024 - 1] = b'\n';
    fs::write(&log, exact).unwrap();
    mac.run(&["phase0", "status"]).succeeded();
    assert!(!log.with_extension("log.1").exists());
    assert_eq!(log_lines(&mac).len(), 2);
    mac.run(&["phase0", "status"]).succeeded();
    let rotated = fs::metadata(log.with_extension("log.1")).unwrap();
    assert!(rotated.len() > 1024 * 1024);
    assert_eq!(log_lines(&mac).len(), 1);
}

/// Catches: a failed rename or write of the log passing as logged.
#[test]
fn a_log_that_cannot_be_written_fails_the_call() {
    let mac = FakeMac::new("log-fails");
    let log = mac.admin_log();
    fs::write(&log, vec![b'x'; 1024 * 1024 + 1]).unwrap();
    // admin.log.1 is a folder that is not empty: the rename fails.
    fs::create_dir_all(log.with_extension("log.1").join("x")).unwrap();
    mac.run(&["phase0", "status"]).failed_for("admin.log: ");
    assert!(mac.calls().is_empty());
    if rustix::process::geteuid().is_root() {
        return; // root writes in a read-only folder
    }
    let mac = FakeMac::new("log-dir-read-only");
    chmod(&mac.base().join("log"), 0o555);
    mac.run(&["phase0", "status"]).failed_for("admin.log: ");
    chmod(&mac.base().join("log"), 0o755);
}

/// Catches: a log the helper appends to through a link (root writing wherever the
/// link points), or a log someone else owns.
#[test]
fn the_log_must_be_roots_regular_file() {
    let mac = FakeMac::new("log-link");
    let elsewhere = mac.root.join("elsewhere");
    std::os::unix::fs::symlink(&elsewhere, mac.admin_log()).unwrap();
    mac.run(&["phase0", "status"])
        .refused_for("admin.log is not a regular file (it is a symbolic link)");
    assert!(!elsewhere.exists());
    let mac = FakeMac::new("log-owner");
    fs::write(mac.admin_log(), "").unwrap();
    mac.own(&mac.admin_log(), 501);
    mac.run(&["phase0", "status"])
        .refused_for("admin.log is owned by uid 501, not root");
}

/// Catches: a tree the caller could change under the helper: a folder writable by
/// group or others, owned by someone else, with an access control list, a link, or
/// missing, anywhere from `/` down to `state`.
#[test]
fn the_tree_must_be_roots_alone() {
    let mac = FakeMac::new("tree");
    let base = mac.base();
    chmod(&base.join("bin"), 0o775);
    mac.run(&["phase0", "status"])
        .refused_for("bin is writable by group or others (mode 775)");
    chmod(&base.join("bin"), 0o755);
    mac.state.borrow_mut().acls.insert(base.join("etc"));
    mac.run(&["phase0", "status"])
        .refused_for("etc has an access control list");
    mac.state.borrow_mut().acls.clear();
    mac.own(&base.join("state"), 501);
    mac.run(&["phase0", "status"])
        .refused_for("state is owned by uid 501, not root");
    mac.own(&base.join("state"), 0);
    mac.own(&mac.root.join("usr/local"), 501);
    mac.run(&["phase0", "status"])
        .refused_for("usr/local is owned by uid 501, not root");
    mac.own(&mac.root.join("usr/local"), 0);
    chmod(&mac.root, 0o777);
    mac.run(&["phase0", "status"])
        .refused_for("is writable by group or others (mode 777)");
    chmod(&mac.root, 0o755);
    fs::remove_dir(base.join("etc")).unwrap();
    mac.run(&["phase0", "status"])
        .refused_for("etc is not a directory (it is missing)");
    std::os::unix::fs::symlink(base.join("bin"), base.join("etc")).unwrap();
    mac.run(&["phase0", "status"])
        .refused_for("etc is not a directory (it is a symbolic link)");
    fs::remove_file(base.join("etc")).unwrap();
    let _socket = std::os::unix::net::UnixListener::bind(base.join("etc")).unwrap();
    mac.run(&["phase0", "status"])
        .refused_for("etc is not a directory (it is neither a file nor a directory)");
    fs::remove_file(base.join("etc")).unwrap();
    fs::write(base.join("etc"), "").unwrap();
    mac.run(&["phase0", "status"])
        .refused_for("etc is not a directory (it is a regular file)");
    assert!(mac.calls().is_empty(), "{:?}", mac.calls());
    assert!(log_lines(&mac).is_empty());
}

/// Catches: a binary the caller could replace being run as root: the running binary,
/// or a folder above it, not root's alone, or a binary outside the checked tree.
#[test]
fn the_running_binary_must_be_roots_alone() {
    let mac = FakeMac::new("exe");
    let exe = mac.base().join("sbin/kbf-mac-session");
    mac.own(&exe, 501);
    mac.run(&["phase0", "status"])
        .refused_for("kbf-mac-session is owned by uid 501, not root");
    mac.own(&exe, 0);
    chmod(&exe, 0o757);
    mac.run(&["phase0", "status"])
        .refused_for("kbf-mac-session is writable by group or others");
    chmod(&exe, 0o755);
    // A binary deeper down: every folder between the tree and it is checked too.
    let deep = mac.base().join("sbin/kbf/1");
    fs::create_dir_all(&deep).unwrap();
    fs::write(deep.join("kbf-mac-session"), "the helper").unwrap();
    mac.state.borrow_mut().exe = Some(deep.join("kbf-mac-session"));
    mac.own(&mac.base().join("sbin/kbf"), 501);
    mac.run(&["phase0", "status"])
        .refused_for("sbin/kbf is owned by uid 501, not root");
    mac.own(&mac.base().join("sbin/kbf"), 0);
    mac.run(&["phase0", "status"]).succeeded();
    mac.state.borrow_mut().exe = Some("/somewhere/else/kbf-mac-session".into());
    mac.run(&["phase0", "status"])
        .refused_for("the running binary /somewhere/else/kbf-mac-session is not under");
    mac.state.borrow_mut().exe = None;
    mac.run(&["phase0", "status"])
        .failed_for("the running binary: no executable path");
}

/// Catches: an lstat that fails being taken for a path that is not there.
#[test]
fn a_failed_look_fails_the_call() {
    let mac = FakeMac::new("lstat-fails");
    let state = mac.state_dir();
    mac.state.borrow_mut().lstat_fails.insert(state.clone());
    mac.run(&["phase0", "status"])
        .failed_for("state: lstat failed");
}

/// Catches: temporary files a call cut short left behind never being removed, or the
/// sweep removing a call's fresh file (or a folder) still in use.
#[test]
fn stale_temporary_files_are_swept() {
    let mac = FakeMac::new("sweep");
    let state = mac.state_dir();
    let now = std::time::UNIX_EPOCH + Duration::from_secs(NOW);
    let set_age = |name: &str, age: u64| {
        let file = File::create(state.join(name)).unwrap();
        file.set_modified(now - Duration::from_secs(age)).unwrap();
    };
    set_age(".incoming.old", 601);
    set_age(".incoming.young", 599);
    set_age("other.old", 6000);
    fs::create_dir(state.join(".incoming.folder")).unwrap();
    let future = File::create(state.join(".incoming.future")).unwrap();
    future.set_modified(now + Duration::from_secs(60)).unwrap();
    mac.run(&["phase0", "status"]).succeeded();
    let mut left: Vec<String> = fs::read_dir(&state)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    left.sort();
    assert_eq!(
        left,
        [
            ".incoming.folder",
            ".incoming.future",
            ".incoming.young",
            "other.old"
        ]
    );
}

/// Holds the lock file's flock as another call would.
fn hold(mac: &FakeMac) -> File {
    let file = File::create(mac.lock()).unwrap();
    rustix::fs::flock(&file, FlockOperation::NonBlockingLockExclusive).unwrap();
    file
}

/// Catches: a changing verb that runs while another call holds the lock, or waits for
/// it without a bound; and `status` waiting for a lock it does not need.
#[test]
fn a_held_lock_is_waited_for_then_refused() {
    let mac = FakeMac::new("lock-held");
    let _held = hold(&mac);
    let ran = mac.run(&["phase0", "apply"]);
    ran.refused_for(&format!("still holds {} after 60s", mac.lock().display()));
    assert_eq!(mac.state.borrow().pauses, LOCK_WAIT);
    assert_eq!(
        ran.out,
        "waiting for another kbf-mac-session node-admin call to finish\n"
    );
    assert!(mac.calls().is_empty(), "{:?}", mac.calls());
    assert!(!mac.record().exists());
    mac.run(&["phase0", "revert"]).refused_for("still holds");
    mac.run(&["phase0", "status"]).succeeded();
}

/// Catches: a lock that is not released when the call ends, or a waiting call that
/// does not take the lock once its holder lets go.
#[test]
fn the_lock_is_taken_once_free_and_released_at_the_end() {
    let mac = FakeMac::new("lock-free");
    let mut held = Some(hold(&mac));
    mac.state.borrow_mut().on_pause = Some(Box::new(move |count| {
        if count == 3 {
            held.take();
        }
    }));
    mac.run(&["phase0", "apply"]).succeeded();
    assert_eq!(mac.state.borrow().pauses, 3);
    let after = File::open(mac.lock()).unwrap();
    rustix::fs::flock(&after, FlockOperation::NonBlockingLockExclusive).unwrap();
}

/// Catches: a lock file opened through a link (root creating or locking a file
/// wherever it points) or owned by someone else.
#[test]
fn the_lock_file_must_be_roots_regular_file() {
    let mac = FakeMac::new("lock-link");
    let elsewhere = mac.root.join("elsewhere");
    std::os::unix::fs::symlink(&elsewhere, mac.lock()).unwrap();
    mac.run(&["phase0", "apply"])
        .failed_for("node-admin.lock: ");
    assert!(!elsewhere.exists());
    assert!(mac.changes().is_empty());
    let mac = FakeMac::new("lock-owner");
    fs::write(mac.lock(), "").unwrap();
    mac.own(&mac.lock(), 501);
    mac.run(&["phase0", "apply"])
        .refused_for("node-admin.lock is owned by uid 501, not root");
    assert!(mac.changes().is_empty());
}

/// Catches: an lstat error other than "not there" (a path through a file) taken for a
/// missing path, which a guard would then read as "nothing to check".
#[test]
fn lstat_reports_errors_other_than_missing() {
    let mac = FakeMac::new("lstat-real");
    let file = mac.root.join("file");
    fs::write(&file, "").unwrap();
    assert_eq!(node_admin::lstat(&mac.root.join("missing")).unwrap(), None);
    assert!(node_admin::lstat(&file.join("below")).is_err());
}
