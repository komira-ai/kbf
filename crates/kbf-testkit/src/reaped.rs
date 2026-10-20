//! A child process a test starts, and the guarantee that it does not outlive the test.
//!
//! [`Reaped`] holds a [`Child`] and, when dropped (a passing test, a failed assertion, a
//! panic, an early return), asks it to stop with SIGTERM, waits up to [`GRACE`], then
//! kills it with SIGKILL, and waits for it, so no process is left running and none is
//! left a zombie. [`spawn`] is the one way tests here start such a child: it also makes
//! every descriptor from a given number up close on exec, so the child holds only the
//! descriptors the test meant it to. Without that, a test run under a tool that keeps a
//! lock open on descriptor 9 (the build box's slot lock does) hands the lock to every child
//! it starts, and a leaked child holds the lock for as long as it lives.
//!
//! See also [`crate::spawn`] (a start retried while the program is busy) and
//! [`crate::forked`] (a child held between `fork` and `exec`): they cover the other ways a
//! forked child touches a test's descriptors.

use std::io::{self, BufRead as _, BufReader};
use std::ops::{Deref, DerefMut};
use std::os::unix::process::CommandExt as _;
use std::process::{Child, ChildStdout, Command};
use std::time::{Duration, Instant};

/// How long [`Reaped`] waits for a child to exit after SIGTERM before it sends SIGKILL.
pub const GRACE: Duration = Duration::from_secs(3);

/// A child that is stopped and reaped when this value is dropped.
#[derive(Debug)]
pub struct Reaped(Child);

impl Deref for Reaped {
    type Target = Child;
    fn deref(&self) -> &Child {
        &self.0
    }
}

impl DerefMut for Reaped {
    fn deref_mut(&mut self) -> &mut Child {
        &mut self.0
    }
}

impl Drop for Reaped {
    fn drop(&mut self) {
        if matches!(self.0.try_wait(), Ok(Some(_))) {
            return;
        }
        let pid = self.0.id().cast_signed();
        // SAFETY: `kill` of our own child, which has not been waited for, so its pid is
        // not reused.
        unsafe { libc::kill(pid, libc::SIGTERM) };
        let asked = Instant::now();
        while asked.elapsed() < GRACE {
            if matches!(self.0.try_wait(), Ok(Some(_))) {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Reads the first line `stdout` delivers, waiting at most `within`, and returns it with the
/// reader (keep it: dropping it closes the pipe under the child). A child that never prints
/// fails the test with `TimedOut` instead of hanging it. The read runs on a thread that stays
/// blocked until the child writes or exits, so the guard must still stop the child.
///
/// # Errors
/// The read error, or `TimedOut`.
pub fn first_line(
    stdout: ChildStdout,
    within: Duration,
) -> io::Result<(String, BufReader<ChildStdout>)> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        let _ = tx.send(reader.read_line(&mut line).map(|_| (line, reader)));
    });
    rx.recv_timeout(within)
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "no line within the deadline"))?
}

/// Marks every descriptor from `first` up close-on-exec in the calling process. Meant for a
/// `pre_exec` hook, after the hook that places the descriptors the child keeps.
///
/// Close-on-exec, not closed: the standard library's own pipe that reports a failed exec to
/// the parent is above `first` and must stay open until the exec. Linux 5.11 and later do it
/// in one call; anything else walks the numbers up to the limit (4096 at most).
pub fn cloexec_from(first: i32) -> io::Result<()> {
    cloexec_from_with(first, close_range_cloexec);
    Ok(())
}

/// The kernel's one-call range marking, and whether it did the work.
#[cfg(target_os = "linux")]
fn close_range_cloexec(first: i32) -> bool {
    const CLOSE_RANGE_CLOEXEC: libc::c_uint = 4;
    // SAFETY: `close_range` takes three integers and touches only descriptor flags.
    let done = unsafe {
        libc::syscall(
            libc::SYS_close_range,
            first as libc::c_uint,
            libc::c_uint::MAX,
            CLOSE_RANGE_CLOEXEC,
        )
    };
    done == 0
}

#[cfg(not(target_os = "linux"))]
fn close_range_cloexec(_first: i32) -> bool {
    false
}

/// [`cloexec_from`] with the one-call way as `range`: when it did not do the work, walk the
/// numbers.
#[inline(never)]
fn cloexec_from_with(first: i32, range: fn(i32) -> bool) {
    if range(first) {
        return;
    }
    // SAFETY: `sysconf` reads a system limit.
    let limit = unsafe { libc::sysconf(libc::_SC_OPEN_MAX) };
    let limit = i32::try_from(limit).unwrap_or(4096).clamp(first, 4096);
    for fd in first..limit {
        // SAFETY: setting a flag on a descriptor number; one that is not open is EBADF,
        // which is the answer for most numbers and is ignored.
        unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };
    }
}

/// Asks the kernel to SIGKILL this process when its parent dies, for a `pre_exec` hook. `parent`
/// is the pid of the process that forked; if it is no longer this process's parent (it died
/// between `fork` and here, so the signal would never come) the hook fails and the spawn with
/// it.
#[cfg(target_os = "linux")]
fn die_with_parent(parent: i32) -> io::Result<()> {
    // SAFETY: `prctl` with `PR_SET_PDEATHSIG` takes two integers; `getppid` reads a pid.
    let (set, now) = unsafe {
        (
            libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL),
            libc::getppid(),
        )
    };
    death_signal_outcome(set, now, parent)
}

/// The verdict on the `prctl` result `set` and the current parent pid `now`. Its own function
/// so a test can reach the refusal of a failed `prctl`, which the kernel never gives here.
#[cfg(target_os = "linux")]
fn death_signal_outcome(set: i32, now: i32, parent: i32) -> io::Result<()> {
    if set != 0 {
        return Err(io::Error::last_os_error());
    }
    if now != parent {
        return Err(io::Error::other("the parent died before the child started"));
    }
    Ok(())
}

/// Starts `command` with every descriptor from `keep_below` up close-on-exec, and returns
/// the guarded child. On Linux the child is also killed (SIGKILL) when the parent dies
/// without running destructors, as when the harness is killed. The kernel ties that signal
/// to the thread that forked, not the process: call this from a thread that outlives the
/// child (a test's own thread does; a short-lived helper thread would kill the child as it
/// ends). Hooks the caller put on `command` with `pre_exec` run first, so one
/// that moves a descriptor into place keeps it by putting it below `keep_below`.
///
/// # Errors
/// The `spawn` error.
pub fn spawn(command: &mut Command, keep_below: i32) -> io::Result<Reaped> {
    // SAFETY: the hook makes only async-signal-safe calls (`syscall`, `sysconf`, `fcntl`).
    unsafe { command.pre_exec(cloexec_hook(keep_below)) };
    #[cfg(target_os = "linux")]
    {
        // SAFETY: the hook makes only async-signal-safe calls (`prctl`, `getppid`).
        let parent = unsafe { libc::getpid() };
        unsafe { command.pre_exec(death_hook(parent)) };
    }
    command.spawn().map(Reaped)
}

/// The `pre_exec` hook [`spawn`] installs for the death signal. Its own function so a test can
/// run it in the test process: inside the forked child no coverage is recorded.
#[cfg(target_os = "linux")]
fn death_hook(parent: i32) -> impl FnMut() -> io::Result<()> + Send + Sync + 'static {
    move || die_with_parent(parent)
}

/// The `pre_exec` hook [`spawn`] installs. Its own function so a test can run it in the
/// test process: inside the forked child no coverage is recorded.
fn cloexec_hook(keep_below: i32) -> impl FnMut() -> io::Result<()> + Send + Sync + 'static {
    move || cloexec_from(keep_below)
}

#[cfg(test)]
mod tests {
    use std::os::fd::AsRawFd as _;
    use std::sync::{Mutex, MutexGuard};

    use super::*;

    /// Every test here marks descriptors "from N up" in this process, which flips the flag of
    /// any other test's descriptor above N. Held for a test's whole body, so they run one at
    /// a time. A poisoned lock (another test failed) is still a lock.
    static FD_TESTS: Mutex<()> = Mutex::new(());

    fn exclusive() -> MutexGuard<'static, ()> {
        FD_TESTS.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Catches a lock that turns one failed test into a failure of all the others: after a
    /// test panicked holding it, the next one still gets it.
    #[test]
    fn a_test_that_panicked_holding_the_lock_does_not_block_the_next() {
        let panicked = std::thread::spawn(|| {
            let _held = exclusive();
            panic!("a test failed holding the lock");
        })
        .join();
        assert!(panicked.is_err());
        drop(exclusive());
    }

    /// Catches a walk that marks the wrong range when the kernel's one-call way is not there
    /// (before Linux 5.11, or not Linux): a descriptor below the first stays inheritable, one
    /// at it and above does not. The descriptors are placed from 1100 and 1200, above anything
    /// the other tests here open.
    #[test]
    fn the_walk_marks_only_descriptors_from_the_first_up() {
        let _fd_tests = exclusive();
        let null = std::fs::File::open("/dev/null").expect("open");
        let dup_from = |base: i32| {
            // SAFETY: duplicates a descriptor this test holds, without close-on-exec.
            let fd = unsafe { libc::fcntl(null.as_raw_fd(), libc::F_DUPFD, base) };
            assert!(fd >= base, "no descriptor from {base}");
            fd
        };
        let flags = |fd: i32| {
            // SAFETY: reading the flags of a descriptor this test holds.
            unsafe { libc::fcntl(fd, libc::F_GETFD) }
        };
        let (below, above) = (dup_from(1100), dup_from(1200));
        cloexec_from_with(1200, |_| false);
        assert_eq!(flags(below) & libc::FD_CLOEXEC, 0, "below the first");
        assert_eq!(
            flags(above) & libc::FD_CLOEXEC,
            libc::FD_CLOEXEC,
            "the first"
        );
        // SAFETY: closing the two duplicates this test made.
        unsafe {
            libc::close(below);
            libc::close(above);
        }
    }

    /// Catches a walk that runs although the one-call way did the work: nothing is marked
    /// then, so a descriptor above the first stays inheritable. Placed from 1300.
    #[test]
    fn the_walk_does_not_run_when_the_range_call_did_the_work() {
        let _fd_tests = exclusive();
        let null = std::fs::File::open("/dev/null").expect("open");
        // SAFETY: duplicates a descriptor this test holds, without close-on-exec.
        let fd = unsafe { libc::fcntl(null.as_raw_fd(), libc::F_DUPFD, 1300) };
        assert!(fd >= 1300);
        cloexec_from_with(1300, |_| true);
        // SAFETY: reading the flags of a descriptor this test holds.
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        assert_eq!(flags & libc::FD_CLOEXEC, 0, "the walk ran");
        // SAFETY: closing the duplicate this test made.
        unsafe { libc::close(fd) };
    }

    /// Catches a hook that marks the wrong range or ignores `keep_below`: the hook `spawn`
    /// installs, run here (a forked child records no coverage), marks from its number up
    /// and leaves a lower descriptor alone. Placed from 1400 and 1500.
    #[test]
    fn the_spawn_hook_marks_from_keep_below_up() {
        let _fd_tests = exclusive();
        let null = std::fs::File::open("/dev/null").expect("open");
        let dup_from = |base: i32| {
            // SAFETY: duplicates a descriptor this test holds, without close-on-exec.
            let fd = unsafe { libc::fcntl(null.as_raw_fd(), libc::F_DUPFD, base) };
            assert!(fd >= base);
            fd
        };
        let (below, above) = (dup_from(1400), dup_from(1500));
        cloexec_hook(1500)().expect("hook");
        let flags = |fd: i32| {
            // SAFETY: reading the flags of a descriptor this test holds.
            unsafe { libc::fcntl(fd, libc::F_GETFD) }
        };
        assert_eq!(flags(below) & libc::FD_CLOEXEC, 0, "below keep_below");
        assert_eq!(flags(above) & libc::FD_CLOEXEC, libc::FD_CLOEXEC, "from it");
        // SAFETY: closing the two duplicates this test made.
        unsafe {
            libc::close(below);
            libc::close(above);
        }
    }

    /// Catches a hook that accepts a parent that is not the real one (the "parent died before
    /// the child started" race would then leave an unguarded child): run in the test process,
    /// a wrong parent pid is refused.
    #[cfg(target_os = "linux")]
    #[test]
    fn the_death_signal_hook_refuses_a_parent_that_is_gone() {
        // SAFETY: reads this process's own parent pid.
        let real = unsafe { libc::getppid() };
        let refused = die_with_parent(real + 1);
        assert!(refused.is_err());
        // Clear the signal this call set on the test thread: it must not outlive the check.
        // SAFETY: `prctl` with `PR_SET_PDEATHSIG` takes two integers.
        unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, 0) };
    }

    /// Catches a hook that refuses the real parent, or that sets no signal: run here with the
    /// true parent pid it succeeds and the thread's death signal is SIGKILL.
    #[cfg(target_os = "linux")]
    #[test]
    fn the_death_signal_hook_accepts_the_real_parent_and_arms_sigkill() {
        // SAFETY: reads this process's own parent pid.
        let real = unsafe { libc::getppid() };
        death_hook(real)().expect("the real parent is accepted");
        let mut armed: libc::c_int = 0;
        // SAFETY: `PR_GET_PDEATHSIG` writes one int through the pointer.
        let got = unsafe { libc::prctl(libc::PR_GET_PDEATHSIG, &mut armed as *mut libc::c_int) };
        // Clear the signal before asserting: it must not outlive the check.
        // SAFETY: `prctl` with `PR_SET_PDEATHSIG` takes two integers.
        unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, 0) };
        assert_eq!(got, 0);
        assert_eq!(armed, libc::SIGKILL);
    }

    /// Catches a guard that waits out the whole grace period (or kills) for a child that stops
    /// on SIGTERM: such a child is gone well before [`GRACE`] ends.
    #[test]
    fn dropping_the_guard_of_a_child_that_stops_on_sigterm_does_not_wait_out_the_grace() {
        let child = spawn(Command::new("sleep").arg("30"), 3).expect("spawn");
        let pid = child.id().cast_signed();
        let started = Instant::now();
        drop(child);
        assert!(started.elapsed() < GRACE);
        // SAFETY: signal 0 only probes; the pid was reaped by the guard.
        assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
    }

    /// Catches a verdict that lets a failed `prctl` through: a non-zero result is an error even
    /// when the parent matches, and a zero result with the right parent is accepted.
    #[cfg(target_os = "linux")]
    #[test]
    fn the_death_signal_verdict_refuses_a_failed_prctl() {
        assert!(death_signal_outcome(-1, 7, 7).is_err());
        assert!(death_signal_outcome(0, 7, 7).is_ok());
    }

    /// Catches a guard that returns without reaping, or that never escalates: a child that
    /// ignores SIGTERM is killed after the grace period and is gone (no process, no zombie)
    /// when the guard is dropped.
    #[test]
    fn dropping_the_guard_kills_a_child_that_ignores_sigterm() {
        let mut command = Command::new("sleep");
        command.arg("30");
        // SAFETY: the hook only calls `signal`, which is async-signal-safe.
        unsafe {
            command.pre_exec(|| {
                libc::signal(libc::SIGTERM, libc::SIG_IGN);
                Ok(())
            });
        }
        let child = spawn(&mut command, 3).expect("spawn");
        let pid = child.id().cast_signed();
        drop(child);
        // SAFETY: signal 0 only probes; the pid was reaped by the guard.
        let probe = unsafe { libc::kill(pid, 0) };
        assert_eq!(probe, -1, "the child is still there");
        assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::ESRCH));
    }

    /// Catches a guard that signals a child that already exited and was waited for.
    #[test]
    fn dropping_the_guard_of_an_exited_child_returns_at_once() {
        let mut child = spawn(&mut Command::new("true"), 3).expect("spawn");
        child.wait().expect("wait");
        let started = Instant::now();
        drop(child);
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    fn piped(program: &str, args: &[&str]) -> Reaped {
        let mut command = Command::new(program);
        command.args(args).stdout(std::process::Stdio::piped());
        spawn(&mut command, 3).expect("spawn")
    }

    /// Catches a `first_line` that hangs or mislabels a silent child: no line within the
    /// deadline is `TimedOut`.
    #[test]
    fn first_line_of_a_silent_child_times_out() {
        let mut child = piped("sleep", &["30"]);
        let out = child.stdout.take().expect("stdout");
        let err = first_line(out, Duration::from_millis(100)).expect_err("silent");
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
    }

    /// Catches a `first_line` that returns a read error as a line or swallows it: bytes that
    /// are not UTF-8 surface as the read error itself, not as `TimedOut`.
    #[test]
    fn first_line_passes_a_read_error_through() {
        let mut child = piped("printf", &["\\377\\n"]);
        let out = child.stdout.take().expect("stdout");
        let err = first_line(out, Duration::from_secs(10)).expect_err("not UTF-8");
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    /// Catches a `first_line` that drops the line or the reader: both come back, and the
    /// reader still holds what follows.
    #[test]
    fn first_line_returns_the_line_and_the_reader() {
        let mut child = piped("printf", &["one\\ntwo\\n"]);
        let out = child.stdout.take().expect("stdout");
        let (line, mut reader) = first_line(out, Duration::from_secs(10)).expect("line");
        assert_eq!(line, "one\n");
        let mut next = String::new();
        reader.read_line(&mut next).expect("next");
        assert_eq!(next, "two\n");
    }

    /// Catches a `spawn` that wraps or hides the start error: a program that does not exist
    /// is the OS error.
    #[test]
    fn spawn_of_a_missing_program_is_the_start_error() {
        let err =
            spawn(&mut Command::new("/nonexistent/kbf-testkit-program"), 3).expect_err("missing");
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }
}
