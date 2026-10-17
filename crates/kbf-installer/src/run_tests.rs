use std::ffi::OsString;
use std::time::Duration;

use super::*;
use crate::fixtures::{me, scratch_dir};

fn spawn(program: &str, args: &[&str]) -> Spawn {
    Spawn {
        program: PathBuf::from(program),
        args: args.iter().map(OsString::from).collect(),
        env: Vec::new(),
        cwd: PathBuf::from("/"),
        run_as: None,
        timeout: Duration::from_secs(60),
    }
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8(bytes.to_vec()).expect("utf-8")
}

/// Catches a program that inherits the caller's environment (a token, a `PATH`, a proxy
/// the host sets): the test process has `PATH` and more, and the child must see exactly
/// the variables it was given, nothing else.
///
/// Mutant seen red: `env_clear()` removed from [`SystemRunner::run`].
#[test]
fn a_program_gets_exactly_the_environment_it_is_given() {
    let mut s = spawn("/usr/bin/env", &[]);
    s.env = vec![
        ("KBF_ONE".into(), "1".into()),
        ("KBF_TWO".into(), "x y".into()),
    ];
    let ran = SystemRunner.run(&s).expect("env runs");
    assert_eq!(ran.code, Some(0));
    assert_eq!(text(&ran.stdout), "KBF_ONE=1\nKBF_TWO=x y\n");
    assert!(!ran.timed_out);

    s.env.clear();
    let ran = SystemRunner.run(&s).expect("env runs");
    assert_eq!(text(&ran.stdout), "");
}

/// Catches a program started in the caller's directory instead of the one it is given.
#[test]
fn a_program_runs_in_the_directory_it_is_given() {
    let dir = scratch_dir("run-cwd");
    let mut s = spawn("/bin/pwd", &[]);
    s.cwd = dir.clone();
    let ran = SystemRunner.run(&s).expect("pwd runs");
    let printed = PathBuf::from(text(&ran.stdout).trim_end());
    assert_eq!(
        printed.canonicalize().expect("exists"),
        dir.canonicalize().expect("exists")
    );
}

/// Catches an exit status read as success, a program that cannot be started reported as
/// a result, and standard error mixed into standard output.
#[test]
fn the_exit_status_and_both_outputs_are_reported_apart() {
    let ran = SystemRunner
        .run(&spawn("/usr/bin/false", &[]))
        .expect("false runs");
    assert_eq!((ran.code, ran.timed_out), (Some(1), false));
    let ran = SystemRunner
        .run(&spawn("/bin/ls", &["/nonexistent-kbf-installer"]))
        .expect("ls runs");
    assert_ne!(ran.code, Some(0));
    assert_eq!(ran.stdout, b"");
    assert!(text(&ran.stderr).contains("nonexistent-kbf-installer"));

    let err = SystemRunner
        .run(&spawn("/nonexistent/kbf-installer-program", &[]))
        .expect_err("not found");
    assert_eq!(err.kind(), io::ErrorKind::NotFound);
}

/// Catches a program that is waited for forever: a long sleep is killed at the timeout
/// and reported as timed out with no exit code. (The test asserts the outcome, not how
/// long it took.)
///
/// Mutant seen red: the timeout branch removed, which leaves the test waiting on the
/// sleep until the harness kills it.
#[test]
fn a_program_past_its_timeout_is_killed_and_reported() {
    let mut s = spawn("/bin/sleep", &["600"]);
    s.timeout = Duration::from_millis(200);
    let ran = SystemRunner.run(&s).expect("sleep runs");
    assert!(ran.timed_out);
    assert_eq!(ran.code, None);
}

/// Catches an output kept without bound (a program printing without end would fill the
/// helper's memory) and one not drained (the program would block on a full pipe and look
/// like a timeout): 200000 bytes are printed, [`CAPTURE`] kept, the exit seen.
#[test]
fn an_output_is_kept_to_the_capture_size_and_the_rest_drained() {
    let ran = SystemRunner
        .run(&spawn("/usr/bin/head", &["-c", "200000", "/dev/zero"]))
        .expect("head runs");
    assert_eq!(ran.code, Some(0));
    assert_eq!(ran.stdout.len() as u64, CAPTURE);
    assert!(!ran.timed_out);
}

/// Catches the switch to another user being skipped or being made when the caller
/// already is that user. When the caller already is that user nothing is switched and it
/// works unprivileged. A switch to anyone else needs root: as a user that is not root it
/// must fail before the program starts (so a helper that is not root cannot run a program
/// as someone it is not), and as root the program runs as that user with that group.
#[test]
fn a_program_runs_as_the_user_it_is_given_or_does_not_run() {
    let mut s = spawn("/usr/bin/id", &["-u"]);
    s.run_as = Some(me());
    let ran = SystemRunner.run(&s).expect("id runs");
    assert_eq!(text(&ran.stdout).trim_end(), me().uid.to_string());

    let other = RunAs {
        uid: me().uid + 1,
        gid: me().gid + 1,
    };
    s.run_as = Some(other);
    let result = SystemRunner.run(&s);
    let is_root = rustix::process::geteuid().is_root();
    assert_eq!(result.is_ok(), is_root);
    // As root: the uid is the one asked for. Otherwise the error is EPERM.
    let outcome = result
        .map(|r| text(&r.stdout).trim_end().to_owned())
        .map_err(|e| e.kind());
    let want = [
        Err(io::ErrorKind::PermissionDenied),
        Ok(other.uid.to_string()),
    ];
    assert_eq!(outcome, want[usize::from(is_root)]);
}

/// Catches the default account being wrong: Linux's `nobody` is 65534, macOS's is -2.
#[test]
fn nobody_is_the_platforms_nobody() {
    use crate::layout::Platform;
    assert_eq!(
        nobody(Platform::Linux),
        RunAs {
            uid: 65534,
            gid: 65534
        }
    );
    assert_eq!(
        nobody(Platform::MacOs),
        RunAs {
            uid: 4_294_967_294,
            gid: 4_294_967_294
        }
    );
}

/// Plays the three calls: records each and fails the one `fail` starts ("none": none).
struct FakeCreds {
    calls: std::cell::RefCell<Vec<String>>,
    fail: &'static str,
}

impl FakeCreds {
    fn call(&self, what: String) -> io::Result<()> {
        let refused = what.starts_with(self.fail);
        self.calls.borrow_mut().push(what);
        if refused {
            Err(io::Error::from(io::ErrorKind::PermissionDenied))
        } else {
            Ok(())
        }
    }
}

impl Creds for FakeCreds {
    fn set_groups_none(&self) -> io::Result<()> {
        self.call("groups".to_owned())
    }

    fn set_gid(&self, gid: u32) -> io::Result<()> {
        self.call(format!("gid {gid}"))
    }

    fn set_uid(&self, uid: u32) -> io::Result<()> {
        self.call(format!("uid {uid}"))
    }
}

/// Catches the privilege drop made in the wrong order or incompletely: the supplementary
/// groups are cleared first, then the group, then the user (a process that has given up
/// root cannot change its group), and a call that fails stops the switch before the next
/// (a process left root with its old group is never run).
///
/// Mutants seen red: the group set after the user; the supplementary groups not cleared;
/// the result of the group call dropped (the user is then set after a failed group).
#[test]
fn the_switch_clears_groups_then_sets_the_group_then_the_user_and_stops_at_a_failure() {
    let user = RunAs { uid: 7, gid: 8 };
    let all = ["groups", "gid 8", "uid 7"];
    for (fail, want) in [
        ("none", &all[..]),
        ("groups", &all[..1]),
        ("gid", &all[..2]),
        ("uid", &all[..]),
    ] {
        let creds = FakeCreds {
            calls: Default::default(),
            fail,
        };
        let result = switch_user(&creds, user);
        assert_eq!(*creds.calls.borrow(), want, "failing {fail:?}");
        assert_eq!(result.is_err(), fail != "none", "failing {fail:?}");
    }
}

/// Catches a leaf that is not the system call it names, by making each call in this
/// process with the ids it already has: the group and the user change nothing; clearing
/// the supplementary groups succeeds only for root, and is refused (not mistaken for
/// success) for anyone else. An error number of a failed call comes back as the error.
#[test]
fn the_real_calls_with_the_ids_the_process_has_succeed_or_are_refused_for_a_user_without_privilege()
{
    Libc.set_gid(me().gid).expect("own group");
    Libc.set_uid(me().uid).expect("own user");
    let cleared = Libc.set_groups_none();
    let is_root = rustix::process::geteuid().is_root();
    assert_eq!(
        cleared.map_err(|e| e.kind()),
        [Err(io::ErrorKind::PermissionDenied), Ok(())][usize::from(is_root)]
    );
    // The closure a child runs between fork and exec, run in this process (a child's
    // coverage is not written out): the same three calls, the first of which already
    // needs root.
    assert_eq!(
        in_child(me())().map_err(|e| e.kind()),
        [Err(io::ErrorKind::PermissionDenied), Ok(())][usize::from(is_root)]
    );
}
