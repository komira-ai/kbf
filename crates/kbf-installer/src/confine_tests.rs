//! The confined start ([`before_exec_with`]): which calls a program that handles unvouched
//! bytes is started with, in which order, and that a failed one ends the start. The calls are
//! played (a real run would limit the test process); the real ones are made once, with
//! values that change nothing.

use std::sync::{Arc, Mutex};

use super::*;

/// Plays every call: records it, and fails the `fail`-th (counting from 0) if told to.
#[derive(Clone)]
struct Played {
    calls: Arc<Mutex<Vec<String>>>,
    fail: Option<usize>,
    /// What `parent_pid` answers; `pid` answers 100.
    parent: libc::pid_t,
}

impl Played {
    fn new(fail: Option<usize>, parent: libc::pid_t) -> Played {
        Played {
            calls: Arc::default(),
            fail,
            parent,
        }
    }

    fn call(&self, what: String) -> io::Result<()> {
        let mut calls = self.calls.lock().expect("lock");
        calls.push(what);
        if self.fail == Some(calls.len() - 1) {
            Err(io::Error::from(io::ErrorKind::PermissionDenied))
        } else {
            Ok(())
        }
    }

    fn seen(&self) -> Vec<String> {
        self.calls.lock().expect("lock").clone()
    }
}

impl Creds for Played {
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

impl Confiner for Played {
    fn pid(&self) -> libc::pid_t {
        100
    }

    fn parent_pid(&self) -> libc::pid_t {
        let _ = self.call("ppid".to_owned());
        self.parent
    }

    fn limit(&self, resource: libc::c_int, value: u64) -> io::Result<()> {
        self.call(format!("limit {resource} {value}"))
    }

    fn address_space(&self, bytes: u64) -> io::Result<()> {
        self.call(format!("as {bytes}"))
    }

    fn no_new_privs(&self) -> io::Result<()> {
        self.call("nnp".to_owned())
    }

    fn die_with_parent(&self) -> io::Result<()> {
        self.call("pdeathsig".to_owned())
    }

    fn keep_open(&self, fd: libc::c_int) -> io::Result<()> {
        self.call(format!("keep {fd}"))
    }

    fn signal_others(&self, signal: libc::c_int) -> io::Result<()> {
        self.call(format!("signal {signal}"))
    }
}

const USER: RunAs = RunAs { uid: 7, gid: 8 };

fn limit(resource: libc::c_int, value: u64) -> String {
    format!("limit {resource} {value}")
}

/// The calls of the whole start, in order.
fn full() -> Vec<String> {
    vec![
        "keep 9".to_owned(),
        limit(libc::RLIMIT_FSIZE as libc::c_int, 0),
        limit(libc::RLIMIT_CORE as libc::c_int, 0),
        limit(libc::RLIMIT_NOFILE as libc::c_int, OPEN_FILES),
        format!("as {}", Confine::FETCH.address_space),
        "nnp".to_owned(),
        "groups".to_owned(),
        "gid 8".to_owned(),
        "uid 7".to_owned(),
        format!("signal {}", libc::SIGKILL),
        limit(libc::RLIMIT_NPROC as libc::c_int, PROCESSES),
        "pdeathsig".to_owned(),
        "ppid".to_owned(),
    ]
}

fn start(
    sys: &Played,
    user: Option<RunAs>,
    confine: Option<Confine>,
    keep: Option<i32>,
) -> io::Result<()> {
    before_exec_with(
        sys.clone(),
        user,
        Inherited::Listed(Vec::new()),
        confine,
        keep,
    )()
}

/// Catches a confinement that is incomplete or in the wrong order: the lent descriptor kept,
/// the limits that need root (file size, core, descriptors, address space) and
/// `no_new_privs` set while still root, then the supplementary groups, group and user, then
/// the kill of the user's other processes (what an earlier child left behind; it must be after
/// the switch, or it would be root signalling every process on the host), then the process
/// limit (it counts every process of the user it is set for) and the parent-death signal (a
/// change of user clears it), then the check that the parent is still there.
///
/// Mutants seen red: the parent-death signal set before the switch of user; any one call left
/// out; `RLIMIT_NPROC` set before the switch; the kill of the user's processes before the
/// switch of user.
#[test]
fn the_confined_start_makes_every_call_in_its_order() {
    let sys = Played::new(None, 100);
    start(&sys, Some(USER), Some(Confine::FETCH), Some(9)).expect("starts");
    assert_eq!(sys.seen(), full());
}

/// Catches a process limit set for a program that was not switched to a user of its own (it
/// counts the caller's processes, so the program could not start a thread), a start that
/// changes the user when told not to, and a kill of "the user's other processes" when the
/// program is still the caller's own user (it would kill the caller's session, or, from a root
/// helper, every process on the host).
///
/// Mutant seen red: `if switched` removed.
#[test]
fn without_a_user_there_is_no_switch_no_process_limit_and_no_kill() {
    let sys = Played::new(None, 100);
    start(&sys, None, Some(Confine::FETCH), Some(9)).expect("starts");
    let want: Vec<String> = full()
        .into_iter()
        .filter(|c| !["groups", "gid 8", "uid 7", "signal 9"].contains(&c.as_str()))
        .filter(|c| *c != limit(libc::RLIMIT_NPROC as libc::c_int, PROCESSES))
        .collect();
    assert_eq!(sys.seen(), want);
}

/// The kill of a user's other processes is never made for root: a start that was switched to
/// uid 0 (a caller that skipped the refusal of a root child) fails before the signal, because
/// `kill(-1)` as root reaches every process on the host.
///
/// Catches the signal sent whoever the user is.
///
/// Mutant: the uid check in `after_switch` removed.
#[test]
fn the_kill_is_never_made_as_root() {
    let sys = Played::new(None, 100);
    let root = RunAs { uid: 0, gid: 8 };
    let err = start(&sys, Some(root), Some(Confine::FETCH), Some(9)).expect_err("refused");
    assert!(err.to_string().contains("root"), "{err}");
    assert!(
        !sys.seen().iter().any(|c| c.starts_with("signal")),
        "{:?}",
        sys.seen()
    );
}

/// What `kill(-1, ..)` answering means: success is success; no other process, and only other
/// users' processes (the usual answer on a host with nothing of the fetch user left), are
/// "nothing to kill" and not failures; any other failure is one.
///
/// Catches the usual answer (`EPERM`) failing every fetch, and a real failure (a signal number
/// the kernel refuses) passing for success.
#[test]
fn nothing_to_kill_is_not_a_failure_and_another_error_is() {
    let errno = |n: i32| move || io::Error::from_raw_os_error(n);
    assert!(signalled(0, &|| unreachable!("no failure")).is_ok());
    assert!(signalled(-1, &errno(libc::ESRCH)).is_ok());
    assert!(signalled(-1, &errno(libc::EPERM)).is_ok());
    let got = signalled(-1, &errno(libc::EINVAL));
    assert_eq!(got.map_err(|e| e.raw_os_error()), Err(Some(libc::EINVAL)));
}

/// Catches an unconfined start that applies a limit or keeps a descriptor anyway.
#[test]
fn without_confinement_only_the_switch_is_made() {
    let sys = Played::new(None, 100);
    start(&sys, Some(USER), None, None).expect("starts");
    assert_eq!(sys.seen(), ["groups", "gid 8", "uid 7"]);
}

/// Catches a start that goes on after a failed call (a program that might hold more than it
/// should is run): whichever call fails, it is the last one made and the start is an error.
///
/// Mutant seen red: the result of any one call dropped.
#[test]
fn a_failed_call_ends_the_start() {
    let all = full();
    // The last call is the read of the parent's id, which cannot fail.
    for at in 0..all.len() - 1 {
        let sys = Played::new(Some(at), 100);
        let result = start(&sys, Some(USER), Some(Confine::FETCH), Some(9));
        assert_eq!(sys.seen(), all[..=at], "failing call {at}");
        assert_eq!(
            result.map_err(|e| e.kind()),
            Err(io::ErrorKind::PermissionDenied),
            "failing call {at}"
        );
    }
}

/// Catches a start that goes on when the parent is already gone (the parent-death signal would
/// never come, and the program would outlive the installer).
///
/// Mutant seen red: the comparison with the parent's id removed.
#[test]
fn a_parent_that_is_gone_ends_the_start() {
    let sys = Played::new(None, 1);
    let err = start(&sys, Some(USER), Some(Confine::FETCH), Some(9)).expect_err("fails");
    assert_eq!(err.to_string(), "the parent is gone");
    assert_eq!(sys.seen(), full());
}

/// Catches a call that is not the system call it names, by making each once with values that
/// change nothing for this process (a limit set to its current hard limit; the parent-death
/// signal and `no_new_privs`, which only ever apply to children and to this process's
/// parent's death), and by a call that is refused coming back as its error.
#[test]
fn the_real_calls_are_the_system_calls_they_name() {
    // SAFETY: getpid and getppid take no arguments.
    assert_eq!(Libc.pid(), unsafe { libc::getpid() });
    // SAFETY: as above.
    assert_eq!(Libc.parent_pid(), unsafe { libc::getppid() });
    for resource in [libc::RLIMIT_CORE, libc::RLIMIT_NOFILE] {
        let mut now = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        // SAFETY: getrlimit writes the struct it is given.
        assert_eq!(unsafe { libc::getrlimit(resource, &raw mut now) }, 0);
        Libc.limit(resource as libc::c_int, now.rlim_max)
            .expect("the limit it already has");
    }
    #[cfg(target_os = "linux")]
    {
        let mut now = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        // SAFETY: getrlimit writes the struct it is given.
        assert_eq!(unsafe { libc::getrlimit(libc::RLIMIT_AS, &raw mut now) }, 0);
        Libc.address_space(now.rlim_max)
            .expect("the address space it already has");
    }
    #[cfg(not(target_os = "linux"))]
    Libc.address_space(1).expect("not enforced here");
    Libc.no_new_privs().expect("no_new_privs");
    Libc.die_with_parent().expect("the parent-death signal");
    let file = std::fs::File::open("/").expect("a directory");
    // SAFETY: dup of a descriptor this test owns; the copy is closed below.
    let held = unsafe { libc::dup(std::os::fd::AsRawFd::as_raw_fd(&file)) };
    assert!(held > 2);
    Libc.keep_open(held).expect("keeps it");
    // SAFETY: closing the descriptor made above.
    unsafe { libc::close(held) };
    // Signal 0 is the permission check of `kill`: it reaches every process the caller could
    // signal and sends none. (The real signal is only ever sent after a switch of user.)
    Libc.signal_others(0).expect("the null signal");
    assert!(Libc.signal_others(9999).is_err(), "no such signal");
    assert!(Libc.limit(9999, 0).is_err(), "no such resource");
    assert!(Libc.keep_open(-1).is_err(), "no such descriptor");
}
