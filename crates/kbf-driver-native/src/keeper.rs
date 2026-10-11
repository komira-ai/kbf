//! The keeper: a process in each action's process group that ends the group when the
//! daemon dies (issue #155).
//!
//! A daemon killed by SIGKILL, the kernel's OOM killer or a panic that aborts cannot
//! end its actions, and the start-up sweep ([`crate::sweep`]) ends them only when a
//! daemon is started again on the node. Until then they would run, for as long as the
//! work takes, beside the copy the scheduler starts elsewhere once the handover grace
//! has passed. So each action gets a keeper: the daemon's own binary in its keeper mode
//! ([`kbf_daemon::keeper`], `group`), a member of the action's group, whose stdin is a
//! pipe only the daemon holds the other end of. The daemon never writes it; when the
//! daemon dies the kernel closes that end, the keeper reads end-of-file and SIGKILLs
//! its own group: the action's.
//!
//! Being in the group is what makes the keeper safe and needs no dismissal:
//! - while it lives the group id names this action's group and no other, since the
//!   kernel reuses no pid that names a live group;
//! - the daemon's own kill of the group at the end of a lease ([`crate::procs`]), and
//!   the next start's sweep, kill the keeper with the rest.
//!
//! It ignores SIGHUP, SIGINT and SIGTERM, so a service manager that stops the daemon's
//! unit with SIGTERM does not stop the keeper before it has read end-of-file. It
//! reaches the group only: a process that left it (`setsid`) is not killed, the gap
//! the lease's own kill has between two polls ([`crate::procs`]).

use std::io;
use std::os::unix::process::CommandExt as _;
use std::process::Child;

use kbf_daemon::keeper::{self, Mode};

/// One action's keeper. Dropped, it is SIGKILLed and reaped, without its group being
/// signalled: the pipe it reads closes only after it is dead.
#[derive(Debug)]
pub struct Keeper {
    child: Child,
}

impl Keeper {
    /// Starts the keeper of process group `pgid`, which must exist in the daemon's
    /// session. Returns once the keeper runs as a keeper in that group: a keeper that
    /// could not join it is an error, never a keeper elsewhere (its kill of its own
    /// group would then end that group, the daemon's own among them).
    ///
    /// # Errors
    /// The keeper could not be started in the group, or its program did not answer as
    /// a keeper.
    pub fn join(pgid: i32) -> io::Result<Self> {
        let mut command = keeper::command(&keeper::program()?, &Mode::Group);
        let mut this = Self {
            child: command.process_group(pgid).spawn()?,
        };
        // Dropped on an error, the keeper is killed and reaped.
        keeper::started(&mut this.child)?;
        Ok(this)
    }

    /// The keeper's pid.
    #[cfg(test)]
    #[must_use]
    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    /// What a daemon's death does: closes the daemon's end of the keeper's stdin.
    /// The keeper stays this process's child, reaped on drop.
    #[cfg(test)]
    pub fn orphan(&mut self) {
        drop(self.child.stdin.take());
    }
}

impl Drop for Keeper {
    fn drop(&mut self) {
        // SIGKILL to the keeper alone (it is this process's unreaped child, so the pid
        // is still its own), then reap it. Its stdin closes after, with the struct.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use std::time::{Duration, Instant};

    /// A `sleep 300` leading a process group of its own.
    fn group() -> Child {
        Command::new("sleep")
            .arg("300")
            .process_group(0)
            .spawn()
            .expect("sleep")
    }

    fn pid(child: &Child) -> i32 {
        i32::try_from(child.id()).expect("pid")
    }

    /// The pgid of `pid` (-1 once it is gone).
    fn pgid(pid: i32) -> i32 {
        // SAFETY: getpgid takes a plain integer.
        unsafe { libc::getpgid(pid) }
    }

    /// Whether `child` exits within `within`; reaps it if so.
    fn exits(child: &mut Child, within: Duration) -> bool {
        let deadline = Instant::now() + within;
        while Instant::now() < deadline {
            if child.try_wait().expect("wait").is_some() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        false
    }

    /// Catches a keeper that does not end the group when the daemon's end of its
    /// pipe closes (the bug: a killed daemon's action runs on), and one that is not in
    /// the action's group (`kill 0` would end another group: the daemon's own).
    #[test]
    fn a_keeper_ends_its_group_when_its_pipe_closes() {
        let mut action = group();
        let mut keeper = Keeper::join(pid(&action)).expect("keeper");
        let keeper_pid = i32::try_from(keeper.pid()).expect("pid");
        assert_eq!(pgid(keeper_pid), pid(&action), "the keeper is in the group");
        keeper.orphan();
        let ended = exits(&mut action, Duration::from_secs(5));
        // A red run must not leave the sleep behind (a no-op once it was reaped).
        let _ = action.kill();
        let _ = action.wait();
        assert!(ended, "the action outlived its keeper's pipe");
        assert!(
            exits(&mut keeper.child, Duration::from_secs(5)),
            "the keeper killed itself with the group"
        );
    }

    /// Catches a keeper that kills on anything but end-of-file, and one whose drop
    /// (the daemon dismissing it at the end of a lease) signals the group: the group
    /// lives on, while the daemon holds the pipe and after the keeper is dropped.
    #[test]
    fn a_dropped_keeper_kills_nothing() {
        let mut action = group();
        let keeper = Keeper::join(pid(&action)).expect("keeper");
        let held = !exits(&mut action, Duration::from_millis(200));
        drop(keeper);
        let alive = !exits(&mut action, Duration::from_millis(200));
        let _ = action.kill();
        let _ = action.wait();
        assert!(held, "killed while held");
        assert!(alive, "dropping the keeper killed the group");
    }

    /// Catches a keeper that a service manager's SIGTERM (or a SIGHUP, a SIGINT) stops
    /// before it has read end-of-file: it must outlive each, and still end the group.
    #[test]
    fn a_keeper_outlives_the_stop_signals() {
        let mut action = group();
        let mut keeper = Keeper::join(pid(&action)).expect("keeper");
        let keeper_pid = i32::try_from(keeper.pid()).expect("pid");
        // No wait: `join` returns once the keeper ignores the stop signals.
        for signal in [libc::SIGTERM, libc::SIGHUP, libc::SIGINT] {
            // SAFETY: kill(2) on the keeper, this process's unreaped child.
            assert_eq!(unsafe { libc::kill(keeper_pid, signal) }, 0);
        }
        let alive = !exits(&mut keeper.child, Duration::from_millis(300));
        keeper.orphan();
        let ended = exits(&mut action, Duration::from_secs(5));
        let _ = action.kill();
        let _ = action.wait();
        assert!(alive, "a stop signal ended the keeper");
        assert!(ended, "the action outlived its keeper's pipe");
    }

    /// Catches a keeper started outside the group it was asked to join (in the
    /// daemon's group, say) when that group does not exist: it must not start.
    #[test]
    fn a_keeper_joins_its_group_or_does_not_start() {
        let mut gone = group();
        let gone_pid = pid(&gone);
        gone.kill().expect("kill");
        gone.wait().expect("reap");
        let error = Keeper::join(gone_pid).expect_err("joined a group that is gone");
        assert_eq!(error.raw_os_error(), Some(libc::EPERM), "{error}");
    }
}
