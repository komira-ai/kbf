//! Child processes the tests start that cannot outlive the test.
//!
//! A `std::process::Child` is not killed when it is dropped. So a test that panics
//! between spawning `kbf-daemon` and stopping it (a failed assert, a timed-out wait)
//! used to leave the daemon running for as long as the machine stayed up, holding
//! whatever the test gave it: ports, the scratch directory, and on a shared box the
//! build slot its test run took. [`Guarded`] kills and reaps its process when it is
//! dropped, which a panic does. On Linux the process is also started with
//! `PR_SET_PDEATHSIG(SIGKILL)`, so it ends when the thread that started it ends,
//! even when the test binary is killed and no destructor runs.

use std::ops::{Deref, DerefMut};
use std::process::{Child, Command};

/// A started process, killed and reaped when dropped if it has not ended.
pub(crate) struct Guarded(Child);

impl Deref for Guarded {
    type Target = Child;

    fn deref(&self) -> &Child {
        &self.0
    }
}

impl DerefMut for Guarded {
    fn deref_mut(&mut self) -> &mut Child {
        &mut self.0
    }
}

impl Drop for Guarded {
    fn drop(&mut self) {
        // Ended and reaped (by the test, or here): nothing to do. `try_wait` reaps one
        // that ended unseen, so its pid is not left as a zombie either.
        if matches!(self.0.try_wait(), Ok(Some(_))) {
            return;
        }
        // Errors are ignored: the process may have ended between the two calls, and a
        // destructor that panics during a panic aborts the test binary.
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// `spawn`, but the process cannot outlive the test (see the module docs).
pub(crate) trait SpawnGuarded {
    /// Starts the command as [`Command::spawn`] does, returning it [`Guarded`].
    fn spawn_guarded(&mut self) -> std::io::Result<Guarded>;
}

impl SpawnGuarded for Command {
    fn spawn_guarded(&mut self) -> std::io::Result<Guarded> {
        #[cfg(target_os = "linux")]
        {
            use std::os::unix::process::CommandExt as _;
            // SAFETY: prctl(2) is async-signal-safe and touches no memory of the
            // parent, as code between fork and exec must.
            unsafe {
                self.pre_exec(|| {
                    if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
        }
        self.spawn().map(Guarded)
    }
}

/// Catches a guard that does not kill its process when a test panics: the process a
/// panicking closure started must be gone once the panic has unwound past it. The
/// process is a long `sleep`, which never ends by itself within the test.
#[test]
fn a_panicking_test_leaves_no_process_behind() {
    let pid = std::sync::Arc::new(std::sync::Mutex::new(0_i32));
    let seen = std::sync::Arc::clone(&pid);
    let unwound = std::panic::catch_unwind(move || {
        let child = Command::new("sleep")
            .arg("300")
            .spawn_guarded()
            .expect("spawn sleep");
        *seen.lock().expect("pid lock") = i32::try_from(child.id()).expect("pid");
        panic!("a test failing while its process runs");
    });
    assert!(unwound.is_err(), "the closure panics");
    let pid = *pid.lock().expect("pid lock");
    assert_ne!(pid, 0, "the process started");
    // SAFETY: kill(2) with signal 0 only asks whether `pid` exists. It was this test's
    // child and has been reaped by the guard, so the pid can only be reused by an
    // unrelated process; a reuse within this test is not plausible.
    let alive = unsafe { libc::kill(pid, 0) } == 0;
    assert!(!alive, "process {pid} outlived the panicking test");
}
