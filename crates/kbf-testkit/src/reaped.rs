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
    #[cfg(target_os = "linux")]
    {
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
        if done == 0 {
            return Ok(());
        }
    }
    // SAFETY: `sysconf` reads a system limit.
    let limit = unsafe { libc::sysconf(libc::_SC_OPEN_MAX) };
    let limit = i32::try_from(limit).unwrap_or(4096).clamp(first, 4096);
    for fd in first..limit {
        // SAFETY: setting a flag on a descriptor number; one that is not open is EBADF,
        // which is the answer for most numbers and is ignored.
        unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };
    }
    Ok(())
}

/// Starts `command` with every descriptor from `keep_below` up close-on-exec, and returns
/// the guarded child. Hooks the caller put on `command` with `pre_exec` run first, so one
/// that moves a descriptor into place keeps it by putting it below `keep_below`.
///
/// # Errors
/// The `spawn` error.
pub fn spawn(command: &mut Command, keep_below: i32) -> io::Result<Reaped> {
    // SAFETY: the hook makes only async-signal-safe calls (`syscall`, `sysconf`, `fcntl`).
    unsafe { command.pre_exec(move || cloexec_from(keep_below)) };
    command.spawn().map(Reaped)
}
