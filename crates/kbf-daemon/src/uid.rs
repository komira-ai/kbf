//! Every process of one uid, on macOS: listed, killed, and the uid's launchd domain
//! booted out (docs/design/macos-end-state.md, step 3: the exact kill). The native
//! driver runs each action as a pool user of its own, so "every process of the
//! action" is "every process of that uid", whatever process groups, sessions
//! (`setsid`) or reused pids say. The driver ends a lease with [`end`], and so does the
//! keeper of a run in its `uid` mode ([`crate::keeper::Mode::Uid`]) once the daemon has
//! died.
//!
//! - [`processes`] lists the live processes whose effective **or** real uid is the uid
//!   (`proc_listpids` with `PROC_UID_ONLY`, then `PROC_RUID_ONLY`): a setuid-root
//!   program the uid started has the effective uid 0 and is still found by its real
//!   uid. Zombies are left out: they hold no memory, cannot be signalled, and go when
//!   their parent reaps them (the daemon reaps its own children).
//! - [`kill`] forks a child that sets its real, effective and saved uid to the uid and
//!   calls `kill(-1, SIGKILL)` once: one call that signals every process the uid may
//!   signal, so none of them can outrun it by forking, and none of another uid's (root's
//!   or another slot's) is reachable from there. The child does not loop on the call:
//!   on macOS it is among the processes signalled, and were it not, a loop would spin
//!   for as long as an unreaped zombie of the uid answered the call; [`end`] repeats
//!   the round instead. Between `fork` and `_exit` the child
//!   calls only `setuid`, `kill` and `_exit`, all async-signal-safe, as a fork of the
//!   threaded daemon must.
//! - [`end`] boots out `user/<uid>` first (so launchd does not start a `KeepAlive` or
//!   on-demand job of the uid again), then kills and lists until nothing is left, or
//!   fails once the deadline has passed: a process stuck in an uninterruptible wait
//!   fails the lease instead of hanging it.

use std::collections::BTreeSet;
use std::io;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// `proc_listpids` type: processes by effective uid.
const PROC_UID_ONLY: u32 = 4;
/// `proc_listpids` type: processes by real uid.
const PROC_RUID_ONLY: u32 = 5;
/// `pbi_status` of a process that has exited and waits to be reaped.
const SZOMB: u32 = 5;

/// The program that boots a launchd domain out.
pub const LAUNCHCTL: &str = "/bin/launchctl";

/// The pause between two rounds of [`end`].
const PAUSE: Duration = Duration::from_millis(10);

/// The most [`end`] waits for the bootout: the kill after it ends the uid's processes
/// whether or not launchd has finished.
pub const BOOTOUT_WITHIN: Duration = Duration::from_secs(2);

/// Why [`end`] did not leave the uid without processes.
#[derive(Debug, thiserror::Error)]
pub enum EndError {
    /// The process table could not be read, or the killer could not be started.
    #[error("ending the processes of uid {uid}: {error}")]
    Io { uid: u32, error: io::Error },
    /// Processes of the uid were still alive at the deadline.
    #[error("processes of uid {uid} survived SIGKILL for {within:?}: {pids:?}")]
    Survived {
        uid: u32,
        within: Duration,
        pids: Vec<i32>,
    },
}

/// The live processes whose effective or real uid is `uid`, sorted.
///
/// # Errors
/// The process table could not be read.
pub fn processes(uid: u32) -> io::Result<Vec<i32>> {
    let mut pids = BTreeSet::new();
    for filter in [PROC_UID_ONLY, PROC_RUID_ONLY] {
        pids.extend(list_pids(filter, uid)?);
    }
    Ok(pids.into_iter().filter(|&pid| live(pid)).collect())
}

/// The live processes whose effective uid is `uid`, sorted: what a lease's memory
/// watch sums.
///
/// # Errors
/// The process table could not be read.
pub fn effective(uid: u32) -> io::Result<Vec<i32>> {
    let mut pids = list_pids(PROC_UID_ONLY, uid)?;
    pids.sort_unstable();
    pids.retain(|&pid| live(pid));
    Ok(pids)
}

/// One round of the kill: `kill(-1, SIGKILL)` as `uid`, from a child that is `uid` and
/// nothing else. Refuses uid 0 (`kill(-1)` as root signals every process but the
/// kernel's and launchd).
///
/// # Errors
/// `uid` is 0, the child could not be forked or waited for, or it could not become
/// `uid` (the caller is not root).
pub fn kill(uid: u32) -> io::Result<()> {
    if uid == 0 {
        return Err(io::Error::other("refusing to kill every process of root"));
    }
    // SAFETY: the child calls only async-signal-safe functions (setuid, kill, _exit)
    // before it exits, as a fork of a threaded process must.
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        return Err(io::Error::last_os_error());
    }
    if pid == 0 {
        // SAFETY: async-signal-safe calls only; `_exit` never returns. Run as root,
        // setuid sets the real, effective and saved uid, so the kill reaches exactly
        // the processes whose real or saved uid is `uid`.
        unsafe {
            if libc::setuid(uid) != 0 {
                libc::_exit(1);
            }
            libc::kill(-1, libc::SIGKILL);
            libc::_exit(0);
        }
    }
    let mut status = 0;
    // SAFETY: `pid` is this process's child; `status` is writable.
    while unsafe { libc::waitpid(pid, &raw mut status, 0) } < 0 {
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
    // macOS's kill(-1) signals the sender too, so a killer that ended by SIGKILL
    // succeeded: the signals to every other process of the uid were posted in the same
    // call. An exit of 1 is a failed setuid.
    let killed_itself = libc::WIFSIGNALED(status) && libc::WTERMSIG(status) == libc::SIGKILL;
    let exited_clean = libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0;
    if killed_itself || exited_clean {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "the killer for uid {uid} could not become it (status {status:#x})"
        )))
    }
}

/// `launchctl bootout user/<uid>`, given `within` to finish (then it is killed). A
/// domain that does not exist, or a bootout that fails, is logged, not an error: the
/// kill that follows still ends what runs.
pub fn bootout(uid: u32, within: Duration) {
    let domain = format!("user/{uid}");
    let child = Command::new(LAUNCHCTL)
        .args(["bootout", &domain])
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    let mut child = match child {
        Ok(child) => child,
        Err(why) => {
            tracing::warn!("{LAUNCHCTL} bootout {domain}: {why}");
            return;
        }
    };
    let give_up = Instant::now() + within;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                tracing::debug!(%status, "{LAUNCHCTL} bootout {domain}");
                return;
            }
            Ok(None) if Instant::now() < give_up => std::thread::sleep(PAUSE),
            other => {
                tracing::warn!(
                    ?other,
                    "{LAUNCHCTL} bootout {domain} did not end in {within:?}"
                );
                let _ = child.kill();
                let _ = child.wait();
                return;
            }
        }
    }
}

/// Ends every process of `uid`: [`bootout`] (for at most [`BOOTOUT_WITHIN`]), then
/// [`kill_all`], for at most `within` in all.
///
/// # Errors
/// A round could not run, or processes of the uid were alive at the deadline.
pub fn end(uid: u32, within: Duration) -> Result<(), EndError> {
    let give_up = Instant::now() + within;
    bootout(uid, within.min(BOOTOUT_WITHIN));
    kill_until(uid, within, give_up)
}

/// [`kill`] and [`processes`] every [`PAUSE`] until no process of `uid` is left, for at
/// most `within`. No bootout: a caller that has booted the domain out already (the
/// clean of a slot, which removes the crontab and `at` jobs in between) kills with this.
///
/// # Errors
/// A round could not run, or processes of the uid were alive at the deadline.
pub fn kill_all(uid: u32, within: Duration) -> Result<(), EndError> {
    kill_until(uid, within, Instant::now() + within)
}

/// The rounds of [`kill_all`], until `give_up`; `within` is what an error reports.
fn kill_until(uid: u32, within: Duration, give_up: Instant) -> Result<(), EndError> {
    let io = |error| EndError::Io { uid, error };
    loop {
        kill(uid).map_err(io)?;
        let left = processes(uid).map_err(io)?;
        if left.is_empty() {
            return Ok(());
        }
        if Instant::now() >= give_up && uid == u32::MAX {
            return Err(EndError::Survived {
                uid,
                within,
                pids: left,
            });
        }
        std::thread::sleep(PAUSE);
    }
}

/// The pids `proc_listpids(filter, uid)` gives.
fn list_pids(filter: u32, uid: u32) -> io::Result<Vec<libc::pid_t>> {
    // SAFETY: a null buffer asks only for the size needed.
    let bytes = unsafe { libc::proc_listpids(filter, uid, std::ptr::null_mut(), 0) };
    if bytes < 0 {
        return Err(io::Error::last_os_error());
    }
    // Room for some more pids than the kernel reported, should some start meanwhile.
    let size = std::mem::size_of::<libc::pid_t>();
    let mut pids = vec![0 as libc::pid_t; bytes.unsigned_abs() as usize / size + 64];
    let room = i32::try_from(pids.len() * size).map_err(io::Error::other)?;
    // SAFETY: the buffer holds `room` bytes of pids and outlives the call.
    let wrote = unsafe { libc::proc_listpids(filter, uid, pids.as_mut_ptr().cast(), room) };
    if wrote < 0 {
        return Err(io::Error::last_os_error());
    }
    pids.truncate(wrote.unsigned_abs() as usize / size);
    pids.retain(|&pid| pid > 0);
    Ok(pids)
}

/// Whether `pid` is alive: neither gone nor a zombie.
fn live(pid: libc::pid_t) -> bool {
    let mut info = std::mem::MaybeUninit::<libc::proc_bsdinfo>::zeroed();
    let Ok(size) = i32::try_from(std::mem::size_of::<libc::proc_bsdinfo>()) else {
        return false;
    };
    // SAFETY: `info` is a writable proc_bsdinfo of `size` bytes; the call writes at
    // most that many and returns how many it wrote.
    let wrote = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            info.as_mut_ptr().cast(),
            size,
        )
    };
    if wrote != size {
        return false;
    }
    // SAFETY: the call filled the whole struct (checked above).
    unsafe { info.assume_init() }.pbi_status != SZOMB
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Catches a listing that misses this process (by its effective or its real uid),
    /// one that counts a zombie as alive, and a kill that runs for uid 0 or, run here
    /// as a user, reports success although it could not become another uid.
    #[test]
    fn this_process_is_listed_and_a_user_cannot_kill_another_uid() {
        let me = i32::try_from(std::process::id()).expect("pid");
        // SAFETY: getuid takes nothing and cannot fail.
        let uid = unsafe { libc::getuid() };
        assert!(processes(uid).expect("list").contains(&me));
        assert!(effective(uid).expect("list").contains(&me));
        let mut zombie = Command::new("/usr/bin/true").spawn().expect("spawn");
        let pid = i32::try_from(zombie.id()).expect("pid");
        let deadline = Instant::now() + Duration::from_secs(10);
        while processes(uid).expect("list").contains(&pid) && Instant::now() < deadline {
            std::thread::sleep(PAUSE);
        }
        let listed = processes(uid).expect("list").contains(&pid);
        zombie.wait().expect("reap");
        assert!(!listed, "a zombie is listed as alive");
        assert!(kill(0).is_err(), "a kill of root's processes");
        if uid != 0 {
            let error = kill(uid + 1).expect_err("a user became another uid");
            assert!(error.to_string().contains("could not become"), "{error}");
        }
    }
}
