//! The action's leader process, reaped with `wait4` so that its resource usage is kept:
//! the CPU of the leader and of every descendant it reaped ([`crate::usage::Meter::reaped`]).
//!
//! The kernel adds a process's CPU to its parent's `RUSAGE_CHILDREN` when the parent
//! reaps it, and the `rusage` `wait4` gives for a child holds the descendants that child
//! reaped in turn. So the figure `wait4` gives for the leader is the CPU of the whole tree
//! its chain of parents reaped, with the leader's last tick: what polling a process table
//! every few hundred milliseconds cannot see. tokio's own wait returns no `rusage`, so the
//! leader is spawned as a standard-library child and reaped here, on a blocking thread.
//!
//! The wait is `waitid(WEXITED | WNOWAIT)` first, which returns when the leader has exited
//! and leaves it unreaped, and then `wait4` under a lock that also guards the kill. A
//! signal by pid is safe only while the process is unreaped (the kernel hands the pid out
//! again after), and the kill takes the same lock to see whether the reap has happened.

use std::io;
use std::process::{Child, ExitStatus};
use std::sync::{Arc, Mutex, PoisonError};

use tokio::task::JoinHandle;

/// What reaping the leader gave.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Reaped {
    /// The leader's exit status.
    pub status: ExitStatus,
    /// User CPU time of the leader and the descendants it reaped, in microseconds.
    pub user_micros: u64,
    /// System CPU time, likewise.
    pub system_micros: u64,
}

/// What `wait` answered, kept for the calls after it.
type Answer = Result<Reaped, (io::ErrorKind, String)>;

/// The leader of one action, owned from its spawn to its reap. Dropped unreaped, it
/// SIGKILLs the leader (as `kill_on_drop` did).
#[derive(Debug)]
pub struct Leader {
    pid: i32,
    /// Whether the leader has been reaped, under the lock the kill takes.
    reaped: Arc<Mutex<bool>>,
    task: Option<JoinHandle<io::Result<Reaped>>>,
    answer: Option<Answer>,
}

impl Leader {
    /// Takes the spawned `child`, which nothing else may wait for.
    #[must_use]
    pub fn new(child: Child) -> Self {
        Self {
            pid: i32::try_from(child.id()).unwrap_or(i32::MAX),
            reaped: Arc::default(),
            task: None,
            answer: None,
        }
    }

    /// The leader's pid.
    #[must_use]
    pub fn pid(&self) -> i32 {
        self.pid
    }

    /// Waits for the leader to exit and reaps it. Cancel-safe: a dropped call leaves the
    /// wait running, and the next call joins it; once reaped every call gives the same
    /// answer.
    ///
    /// # Errors
    /// The wait failed, or its thread did not finish.
    pub async fn wait(&mut self) -> io::Result<Reaped> {
        if let Some(answer) = &self.answer {
            return answer
                .clone()
                .map_err(|(kind, why)| io::Error::new(kind, why));
        }
        let (pid, reaped) = (self.pid, Arc::clone(&self.reaped));
        let task = self
            .task
            .get_or_insert_with(|| tokio::task::spawn_blocking(move || reap(pid, &reaped)));
        let got = task.await.map_err(io::Error::other).and_then(|r| r);
        self.answer = Some(match &got {
            Ok(reaped) => Ok(*reaped),
            Err(e) => Err((e.kind(), e.to_string())),
        });
        got
    }

    /// SIGKILLs the leader unless it is reaped already (then the pid may be another
    /// process's). A leader that has exited but is not reaped is signalled harmlessly.
    ///
    /// # Errors
    /// The signal could not be sent.
    pub fn start_kill(&self) -> io::Result<()> {
        let reaped = self.reaped.lock().unwrap_or_else(PoisonError::into_inner);
        if *reaped {
            return Ok(());
        }
        // FFI-BOUNDARY: kill(2) takes two integers and keeps nothing.
        // SAFETY: the pid is this process's own unreaped child (the lock shows the reap
        // has not happened), so it names no other process.
        let done = unsafe { libc::kill(self.pid, libc::SIGKILL) };
        if done == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }
}

impl Drop for Leader {
    fn drop(&mut self) {
        let _ = self.start_kill();
    }
}

/// Blocks until `pid`, a child of this process, has exited; reaps it and sets `reaped`
/// under its lock.
fn reap(pid: i32, reaped: &Mutex<bool>) -> io::Result<Reaped> {
    let id = libc::id_t::try_from(pid).map_err(io::Error::other)?;
    loop {
        let mut info = std::mem::MaybeUninit::<libc::siginfo_t>::zeroed();
        // FFI-BOUNDARY: `info` is this frame's; waitid writes a siginfo_t into it and keeps
        // no pointer. WNOWAIT leaves the child unreaped, so its pid stays this process's.
        // SAFETY: `info` is a writable siginfo_t; P_PID with the pid of this process's child.
        let done = unsafe {
            libc::waitid(
                libc::P_PID,
                id,
                info.as_mut_ptr(),
                libc::WEXITED | libc::WNOWAIT,
            )
        };
        if done == 0 {
            break;
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
    let mut flag = reaped.lock().unwrap_or_else(PoisonError::into_inner);
    let result = wait4(pid);
    // Reaped or gone (ECHILD): the pid is no longer ours either way.
    *flag = true;
    result
}

/// `wait4` of `pid`, which has exited.
fn wait4(pid: i32) -> io::Result<Reaped> {
    use std::os::unix::process::ExitStatusExt as _;
    loop {
        let mut status: libc::c_int = 0;
        let mut usage = std::mem::MaybeUninit::<libc::rusage>::zeroed();
        // FFI-BOUNDARY: `status` and `usage` are this frame's; wait4 writes them and
        // keeps no pointer. The child is reaped by this call, once.
        // SAFETY: both pointers are writable and outlive the call.
        let got = unsafe { libc::wait4(pid, &raw mut status, 0, usage.as_mut_ptr()) };
        if got == pid {
            // SAFETY: the call succeeded, so it filled the struct.
            let usage = unsafe { usage.assume_init() };
            let micros = |t: libc::timeval| {
                u64::try_from(t.tv_sec)
                    .unwrap_or(0)
                    .saturating_mul(1_000_000)
                    .saturating_add(u64::try_from(t.tv_usec).unwrap_or(0))
            };
            return Ok(Reaped {
                status: ExitStatus::from_raw(status),
                user_micros: micros(usage.ru_utime),
                system_micros: micros(usage.ru_stime),
            });
        }
        if got >= 0 {
            return Err(io::Error::other("wait4 returned another pid"));
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}
