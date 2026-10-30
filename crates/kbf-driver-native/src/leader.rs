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
/// SIGKILLs the leader and reaps it on a thread of its own, unless a wait is already
/// doing that (as `kill_on_drop` did, through tokio).
#[derive(Debug)]
pub struct Leader {
    pid: i32,
    /// Whether the leader has been reaped, under the lock the kill takes.
    reaped: Arc<Mutex<bool>>,
    task: Option<JoinHandle<io::Result<Reaped>>>,
    answer: Option<Answer>,
    /// Whether `finish` started a thread to reap the leader.
    reaper: bool,
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
            reaper: false,
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

impl Leader {
    /// Kills the leader and, when no wait has started and no reaper is running, reaps it
    /// on a thread of its own. Gives that thread, or `None` when there is nothing to
    /// start (a wait is running or has run, or a reaper started before) or the thread
    /// could not start (the zombie then stays until the daemon exits).
    fn finish(&mut self) -> Option<std::thread::JoinHandle<()>> {
        let _ = self.start_kill();
        if self.task.is_some() || self.reaper {
            return None;
        }
        self.reaper = true;
        let (pid, reaped) = (self.pid, Arc::clone(&self.reaped));
        // A thread of its own: a drop may run outside any runtime.
        std::thread::Builder::new()
            .name("leader-reap".into())
            .spawn(move || {
                let _ = reap(pid, &reaped);
            })
            .ok()
    }
}

impl Drop for Leader {
    fn drop(&mut self) {
        drop(self.finish());
    }
}

/// Calls `call` again for as long as a signal interrupts it (EINTR).
fn retrying<T>(mut call: impl FnMut() -> io::Result<T>) -> io::Result<T> {
    loop {
        match call() {
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            done => return done,
        }
    }
}

/// Blocks until `pid`, a child of this process, has exited; reaps it and sets `reaped`
/// under its lock.
fn reap(pid: i32, reaped: &Mutex<bool>) -> io::Result<Reaped> {
    let id = libc::id_t::try_from(pid).map_err(io::Error::other)?;
    retrying(|| {
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
        (done == 0)
            .then_some(())
            .ok_or_else(io::Error::last_os_error)
    })?;
    let mut flag = reaped.lock().unwrap_or_else(PoisonError::into_inner);
    let result = retrying(|| wait4(pid));
    // Reaped or gone (ECHILD): the pid is no longer ours either way.
    *flag = true;
    result
}

/// One `wait4` of `pid`, which has exited.
fn wait4(pid: i32) -> io::Result<Reaped> {
    use std::os::unix::process::ExitStatusExt as _;
    let mut status: libc::c_int = 0;
    let mut usage = std::mem::MaybeUninit::<libc::rusage>::zeroed();
    // FFI-BOUNDARY: `status` and `usage` are this frame's; wait4 writes them and keeps no
    // pointer. The child is reaped by this call, once.
    // SAFETY: both pointers are writable and outlive the call.
    let got = unsafe { libc::wait4(pid, &raw mut status, 0, usage.as_mut_ptr()) };
    // Without WNOHANG, wait4 of one pid answers that pid or -1.
    if got != pid {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the call succeeded, so it filled the struct.
    let usage = unsafe { usage.assume_init() };
    let micros = |t: libc::timeval| {
        u64::try_from(t.tv_sec)
            .unwrap_or_default()
            .saturating_mul(1_000_000)
            .saturating_add(u64::try_from(t.tv_usec).unwrap_or_default())
    };
    Ok(Reaped {
        status: ExitStatus::from_raw(status),
        user_micros: micros(usage.ru_utime),
        system_micros: micros(usage.ru_stime),
    })
}

#[cfg(test)]
mod tests {
    use std::os::unix::process::ExitStatusExt as _;
    use std::process::Command;

    use super::*;

    /// A pid no process holds (above the kernel's pid limit).
    const NO_SUCH: i32 = 2_000_000_000;

    fn sleeper() -> Leader {
        Leader::new(Command::new("sleep").arg("60").spawn().expect("sleep"))
    }

    /// Catches: a leader that is not killed (the wait would last a minute) or a kill that
    /// reports the signal as an exit; the exit status carries SIGKILL.
    #[tokio::test]
    async fn a_kill_before_the_reap_ends_the_leader_with_sigkill() {
        let mut leader = sleeper();
        leader.start_kill().expect("signalled");
        let reaped = leader.wait().await.expect("reaped");
        assert_eq!(reaped.status.signal(), Some(libc::SIGKILL));
    }

    /// Catches: a signal sent to the pid of a leader already reaped, which the kernel
    /// may have handed to another process. After the reap `start_kill` succeeds and
    /// sends nothing: with a pid that no longer names a child it would fail with ESRCH
    /// if it did send.
    #[tokio::test]
    async fn a_kill_after_the_reap_signals_nothing() {
        let mut leader = Leader::new(Command::new("true").spawn().expect("true"));
        let reaped = leader.wait().await.expect("reaped");
        assert!(reaped.status.success());
        leader.pid = NO_SUCH;
        assert!(
            leader.start_kill().is_ok(),
            "a reaped leader is not signalled"
        );
    }

    /// Catches: a failed signal reported as success; a pid that names no process fails.
    #[test]
    fn a_kill_the_system_refuses_is_an_error() {
        let mut leader = sleeper();
        let real = std::mem::replace(&mut leader.pid, NO_SUCH);
        assert!(leader.start_kill().is_err());
        leader.pid = real;
    }

    /// Catches: a second wait that waits again (a reaped pid answers ECHILD, or blocks on
    /// another process) instead of the first answer, and an error forgotten after the
    /// first call. A leader that was never a child fails the wait, and fails it the same
    /// way every time.
    #[tokio::test]
    async fn every_wait_gives_the_first_answer() {
        let mut leader = Leader::new(Command::new("true").spawn().expect("true"));
        let first = leader.wait().await.expect("first");
        assert_eq!(leader.wait().await.expect("second"), first);

        let mut stranger = sleeper();
        let real = std::mem::replace(&mut stranger.pid, NO_SUCH);
        let first = stranger.wait().await.expect_err("not a child");
        let second = stranger.wait().await.expect_err("still not");
        assert_eq!(first.kind(), second.kind());
        assert_eq!(first.to_string(), second.to_string());
        stranger.pid = real;
    }

    /// Catches: a wait whose cancellation loses the reap: a call dropped before the
    /// leader exits (a timeout) must leave the wait running, and the next call joins it
    /// and gets the leader's status.
    #[tokio::test]
    async fn a_dropped_wait_is_joined_by_the_next() {
        let mut leader = sleeper();
        let waited =
            tokio::time::timeout(std::time::Duration::from_millis(50), leader.wait()).await;
        assert!(waited.is_err(), "the sleeper is still running");
        leader.start_kill().expect("signalled");
        let reaped = leader.wait().await.expect("joined");
        assert_eq!(reaped.status.signal(), Some(libc::SIGKILL));
    }

    /// Catches: an interrupted call given up on, and any other error retried for ever.
    #[test]
    fn only_an_interrupted_call_is_retried() {
        let mut calls = 0;
        let got = retrying(|| {
            calls += 1;
            if calls < 3 {
                Err(io::Error::from(io::ErrorKind::Interrupted))
            } else {
                Ok(calls)
            }
        });
        assert_eq!(got.expect("third call"), 3);
        let mut calls = 0;
        let got: io::Result<()> = retrying(|| {
            calls += 1;
            Err(io::Error::from(io::ErrorKind::NotFound))
        });
        assert_eq!(
            got.expect_err("not retried").kind(),
            io::ErrorKind::NotFound
        );
        assert_eq!(calls, 1);
    }

    /// Catches: a pid that is not a child waited for as if it were (the wait4 after a
    /// successful waitid failing is reported, not swallowed): with nothing to reap the
    /// call fails.
    #[test]
    fn wait4_of_a_process_that_is_not_a_child_fails() {
        assert!(wait4(NO_SUCH).is_err());
    }

    /// Catches: a leader dropped before its first wait left a zombie (killed, never
    /// reaped). The reap runs on its own thread, which the test joins; the pid then
    /// names no process, not even a zombie (`kill(pid, 0)` fails with ESRCH).
    #[test]
    fn a_leader_dropped_before_its_first_wait_is_reaped() {
        let mut leader = sleeper();
        let pid = leader.pid();
        let reaper = leader.finish().expect("a reaper thread");
        reaper.join().expect("reaped");
        assert!(leader.finish().is_none(), "a second reaper was started");
        // SAFETY: signal 0 sends nothing; it only asks whether the pid names a process.
        let found = unsafe { libc::kill(pid, 0) };
        assert_eq!(found, -1, "pid {pid} is still a process (a zombie)");
        assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::ESRCH));
    }

    /// Catches: a second reaper started for a leader that is already being waited for or
    /// is reaped (two waits for one pid): a dropped leader whose wait is running leaves
    /// the reap to it, and one already reaped is left alone.
    #[tokio::test]
    async fn a_leader_being_waited_for_or_reaped_is_dropped_without_a_second_reaper() {
        let mut leader = sleeper();
        let waited =
            tokio::time::timeout(std::time::Duration::from_millis(50), leader.wait()).await;
        assert!(waited.is_err(), "the sleeper is still running");
        assert!(
            leader.finish().is_none(),
            "a reaper beside the running wait"
        );
        let reaped = leader.wait().await.expect("reaped by the running wait");
        assert_eq!(reaped.status.signal(), Some(libc::SIGKILL));

        let mut leader = Leader::new(Command::new("true").spawn().expect("true"));
        leader.wait().await.expect("reaped");
        assert!(leader.finish().is_none(), "a reaper for a reaped leader");
    }

    /// Catches: a negative pid accepted for waitid's unsigned id.
    #[test]
    fn a_negative_pid_cannot_be_waited_for() {
        assert!(reap(-1, &Mutex::new(false)).is_err());
    }
}
