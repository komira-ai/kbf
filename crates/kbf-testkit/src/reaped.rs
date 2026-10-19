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

use std::io;
use std::ops::{Deref, DerefMut};
use std::os::unix::process::CommandExt as _;
use std::process::{Child, Command};
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

/// Starts `command` with every descriptor from `keep_below` up close-on-exec, and returns
/// the guarded child. Hooks the caller put on `command` with `pre_exec` run first, so one
/// that moves a descriptor into place keeps it by putting it below `keep_below`.
///
/// # Errors
/// The `spawn` error.
pub fn spawn(command: &mut Command, keep_below: i32) -> io::Result<Reaped> {
    // SAFETY: the hook makes only async-signal-safe calls (`syscall`, `sysconf`, `fcntl`).
    unsafe { command.pre_exec(cloexec_hook(keep_below)) };
    command.spawn().map(Reaped)
}

/// The `pre_exec` hook [`spawn`] installs. Its own function so a test can run it in the
/// test process: inside the forked child no coverage is recorded.
fn cloexec_hook(keep_below: i32) -> impl FnMut() -> io::Result<()> + Send + Sync + 'static {
    move || cloexec_from(keep_below)
}

#[cfg(test)]
mod tests {
    use std::os::fd::AsRawFd as _;

    use super::*;

    /// Catches a walk that marks the wrong range when the kernel's one-call way is not there
    /// (before Linux 5.11, or not Linux): a descriptor below the first stays inheritable, one
    /// at it and above does not. The descriptors are placed from 1100 and 1200, above anything
    /// the other tests here open.
    #[test]
    fn the_walk_marks_only_descriptors_from_the_first_up() {
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
}
