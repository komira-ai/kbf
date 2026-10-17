use std::ffi::OsString;
use std::time::Duration;

use super::*;

/// A descriptor number no test of this crate holds, so running the pre-exec closure in
/// this process closes nothing that matters.
const HIGH_FD: u32 = 600;
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
    let started = Instant::now();
    let ran = SystemRunner.run(&s).expect("sleep runs");
    assert!(ran.timed_out);
    assert_eq!(ran.code, None);
    // The kill is not waited on forever: the run returns in the timeout and a little.
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "{:?}",
        started.elapsed()
    );
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
        before_exec(Some(me()), Inherited::Range(HIGH_FD))().map_err(|e| e.kind()),
        [Err(io::ErrorKind::PermissionDenied), Ok(())][usize::from(is_root)]
    );
}

/// Catches a program that inherits a descriptor the helper's parent left open (a lock file,
/// a socket). `before_exec` keeps every descriptor from the given number up from the
/// program by `close_range` (and, where that is not allowed, by marking the listed ones
/// itself), and leaves the lower ones as they were. The descriptors are fresh ones this
/// test opened and numbers it asks for, whatever other tests hold; a real child, started
/// with a fresh descriptor at the lowest free number from 3 (the boundary), lists none.
///
/// Mutants seen red: both ways doing nothing. (The boundary mutant, 4 for 3, is caught
/// only when the lowest free descriptor is 3.)
#[test]
fn the_child_inherits_no_descriptor_above_its_streams() {
    use std::os::fd::AsRawFd;
    let file = std::fs::File::open("/").expect("open");
    let dup = |at: libc::c_int| {
        // SAFETY: fcntl F_DUPFD takes a descriptor this test holds and a lowest number; the
        // new descriptor is a raw number closed below.
        unsafe { libc::fcntl(file.as_raw_fd(), libc::F_DUPFD, at) }
    };
    let flags = |fd: i32| unsafe { libc::fcntl(fd, libc::F_GETFD) };
    let cloexec = |fd: i32| flags(fd) & libc::FD_CLOEXEC != 0;
    let (above, below, listed) = (
        dup(HIGH_FD as libc::c_int),
        dup(20),
        dup(HIGH_FD as libc::c_int),
    );
    assert!(above >= HIGH_FD as libc::c_int && below >= 20 && listed >= HIGH_FD as libc::c_int);
    // The range way.
    before_exec(None, Inherited::Range(HIGH_FD))().expect("no user, nothing to switch");
    assert!(
        cloexec(above) && cloexec(listed),
        "the high ones close at exec"
    );
    assert!(!cloexec(below), "the low one is as it was");
    // The listing way, which macOS and a sandbox without close_range take: a fresh high
    // descriptor is found by the listing and marked.
    let fresh = dup(HIGH_FD as libc::c_int + 50);
    let found = listed_from(HIGH_FD).expect("the descriptor directory lists");
    assert!(
        found.contains(&fresh) && !found.contains(&below),
        "{found:?}"
    );
    before_exec(None, Inherited::Listed(found))().expect("marks them");
    assert!(cloexec(fresh));
    // A listed descriptor that is gone by then is fine; any other failure is not.
    before_exec(None, Inherited::Listed(vec![fresh + 1000]))().expect("a closed one is skipped");
    // The probe says the range call works here (this CI and dev box have Linux 5.11+).
    assert!(matches!(Inherited::of(3), Ok(Inherited::Range(3))));

    // A real child: a fresh descriptor at the lowest free number from 3 is not inherited.
    // `readlink` opens no descriptor of its own, so the number is the helper's alone: it
    // finds the link (exit 0) if the child holds it, nothing (exit 1) if not.
    let lowest = dup(3);
    assert!(lowest >= 3);
    let ran = SystemRunner
        .run(&spawn(
            "/usr/bin/readlink",
            &[&format!("/proc/self/fd/{lowest}")],
        ))
        .expect("readlink runs");
    assert_eq!(ran.code, Some(1), "the child holds descriptor {lowest}");
    for fd in [above, below, listed, fresh, lowest] {
        // SAFETY: closing the descriptors this test made.
        unsafe { libc::close(fd) };
    }
}

/// Catches what a program leaves running in its own group: the program ends at once, having
/// started a `sleep` that stays in the group (this test binary, run as the program, starts
/// it and exits); the run kills the group before returning, so the sleeper is gone.
///
/// Mutant seen red: the group not killed after a normal exit.
#[cfg(target_os = "linux")]
#[test]
fn what_a_program_leaves_in_its_group_dies_with_the_run() {
    let dir = scratch_dir("run-group");
    let mut s = spawn(
        std::env::current_exe()
            .expect("this binary")
            .to_str()
            .expect("utf-8"),
        &[
            "--ignored",
            "--exact",
            "run::tests::leaves_a_sleeper",
            "--nocapture",
        ],
    );
    s.cwd = dir.clone();
    let ran = SystemRunner.run(&s).expect("the test binary runs");
    assert_eq!(ran.code, Some(0), "{}", text(&ran.stderr));
    let pid: libc::pid_t = std::fs::read_to_string(dir.join("pid"))
        .expect("the pid file")
        .trim()
        .parse()
        .expect("a pid");
    // SAFETY: signal 0 only asks whether the process exists.
    let alive = || unsafe { libc::kill(pid, 0) } == 0;
    for _ in 0..100 {
        if !alive() {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    // A killed child that nobody waits for stays a zombie until its parent (init) reaps
    // it; `kill(pid, 0)` still succeeds then, so read the state instead.
    let state = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap_or_default();
    let dead = state.is_empty() || state.contains(") Z ") || state.contains(") X ");
    assert!(dead, "the sleeper is still running: {state}");
}

/// The program of `what_a_program_leaves_in_its_group_dies_with_the_run`: started only by
/// that test (it is ignored in a normal run). Starts a sleeper in its own group, records its
/// pid in `pid` in the working directory, and exits.
#[test]
#[ignore = "run as a child program by what_a_program_leaves_in_its_group_dies_with_the_run"]
// The sleeper is left running on purpose: the run under test has to kill it.
#[allow(clippy::zombie_processes)]
fn leaves_a_sleeper() {
    let sleeper = std::process::Command::new("/usr/bin/sleep")
        .arg("60")
        .spawn()
        .expect("sleep starts");
    std::fs::write("pid", sleeper.id().to_string()).expect("pid");
}

/// Catches a program named by a relative path being looked up on a `PATH` the helper
/// never meant: refused before anything starts.
#[test]
fn a_relative_program_is_refused() {
    let err = SystemRunner.run(&spawn("ls", &[])).expect_err("refused");
    assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
}

/// Catches a timeout defeated by a descendant that left the process group and keeps the
/// pipes: `setsid` forks `sleep` into a group of its own with the output open, the program
/// itself ends, and the run still returns within the read grace instead of when the sleep
/// does.
///
/// Mutant seen red: the readers joined with no grace (the run takes the sleep's thirty
/// seconds).
#[cfg(target_os = "linux")]
#[test]
fn a_descendant_that_left_the_group_cannot_hold_the_run() {
    let started = Instant::now();
    let ran = SystemRunner
        .run(&spawn("/usr/bin/setsid", &["/usr/bin/sleep", "30"]))
        .expect("setsid runs");
    assert!(
        started.elapsed() < Duration::from_secs(20),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(ran.code, Some(0));
    assert!(ran.stdout.is_empty());
}
