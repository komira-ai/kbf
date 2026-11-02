//! Whether some process holds a lock on a file: the install lock by asking for it without
//! waiting ([`flock_held`]), any lock from `/proc/locks` without taking one ([`held`]).
//!
//! Each line of `/proc/locks` is `<n>: <kind> <ADVISORY|MANDATORY> <READ|WRITE> <pid>
//! <major>:<minor>:<inode> <start> <end>`; a process waiting for a lock has `->` after the
//! number and does not hold it. `dpkg` and `apt` take `POSIX` locks, `flock(1)` takes
//! `FLOCK` ones; either counts.

use std::fs::{self, OpenOptions};
use std::io::{ErrorKind, Read};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use crate::trust::nofollow;

/// Whether some process holds the `flock` on `lock_file`, found by asking for it without
/// waiting: a shared lock is taken and let go at once. A missing file is not held; a file
/// that cannot be opened or asked is held, since idle is not proved.
pub fn flock_held(lock_file: &Path) -> bool {
    let file = match OpenOptions::new()
        .read(true)
        .custom_flags(nofollow())
        .open(lock_file)
    {
        Ok(file) => file,
        Err(e) if e.kind() == ErrorKind::NotFound => return false,
        Err(_) => return true,
    };
    // A shared request conflicts with the exclusive lock an install holds, and the lock it
    // takes is let go with the file at the end of this function.
    !matches!(file.try_lock_shared(), Ok(()))
}

/// Who holds the install lock, as far as this host can tell: the process id the holder wrote
/// into the lock file when it took the lock ([`crate::host::take_lock`]), and whether a process
/// with that id is alive. A holder whose process is gone still holds the lock when a process it
/// started kept the descriptor; a lock file that names no process was taken by a program that
/// writes no id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Holder {
    pub pid: Option<u32>,
    pub alive: bool,
}

impl Holder {
    /// The holder, in words: `pid 812, which is alive`, `pid 812, which is gone`, or `a process
    /// that wrote no id`.
    pub fn describe(&self) -> String {
        match (self.pid, self.alive) {
            (Some(pid), true) => format!("pid {pid}, which is alive"),
            (Some(pid), false) => format!("pid {pid}, which is gone"),
            (None, _) => "a process that wrote no id".to_owned(),
        }
    }
}

/// The most bytes of a lock file that are read for the holder's id.
const HOLDER_BYTES: u64 = 32;

/// The holder of the lock on `lock_file` (see [`Holder`]); a file that cannot be read, or that
/// does not hold one process id, names no one.
pub fn holder(lock_file: &Path) -> Holder {
    let pid = OpenOptions::new()
        .read(true)
        .custom_flags(nofollow())
        .open(lock_file)
        .and_then(|file| {
            let mut text = String::new();
            file.take(HOLDER_BYTES).read_to_string(&mut text)?;
            Ok(text)
        })
        .ok()
        .and_then(|text| text.trim_end().parse::<u32>().ok());
    Holder {
        pid,
        alive: pid.is_some_and(alive),
    }
}

/// Whether a process with id `pid` exists: a signal 0 is sent to it, which checks without
/// delivering. One that exists and may not be signalled (another user's) is alive; an id that is
/// no process's, or is not a process id at all (0, or past the largest), is not.
pub fn alive(pid: u32) -> bool {
    let Some(pid) = i32::try_from(pid)
        .ok()
        .and_then(rustix::process::Pid::from_raw)
    else {
        return false;
    };
    !matches!(
        rustix::process::test_kill_process(pid),
        Err(rustix::io::Errno::SRCH)
    )
}

/// For tests: wait (up to 500 looks, 10 ms apart: 5 s) until nobody holds or shares the
/// `flock` on `lock_file`, so that an exclusive lock can be had. A process another test thread
/// starts copies every open descriptor until it execs, and a plain `flock` lives as long as any
/// copy of the descriptor does, so a lock dropped by closing its file can outlive the drop by a
/// moment. (The install lock is not such a lock: `host::Lock` unlocks at its drop.) A
/// lock that never goes is left for the caller's own assertion to report.
#[cfg(test)]
pub fn wait_lockable(lock_file: &Path) {
    wait_lockable_paced(lock_file, &crate::pace::Pace::real());
}

/// [`wait_lockable`] on the clock `pace` pauses with.
#[cfg(test)]
pub fn wait_lockable_paced(lock_file: &Path, pace: &crate::pace::Pace) {
    use std::time::Duration;

    let Ok(file) = OpenOptions::new().read(true).open(lock_file) else {
        return;
    };
    for _ in 0..500 {
        if file.try_lock().is_ok() {
            break;
        }
        (pace.sleep)(Duration::from_millis(10));
    }
}

/// Whether a lock is held on `lock_file`, reading the lock table at `proc_locks`. A
/// missing lock file, or an unreadable table, is "not held".
pub fn held(proc_locks: &Path, lock_file: &Path) -> bool {
    // `stat` through rustix: the device number has the platform's own type, which
    // `major` and `minor` take.
    let (Ok(stat), Ok(table)) = (rustix::fs::stat(lock_file), fs::read_to_string(proc_locks))
    else {
        return false;
    };
    table_holds(
        &table,
        rustix::fs::major(stat.st_dev),
        rustix::fs::minor(stat.st_dev),
        stat.st_ino,
    )
}

/// Whether `table` has a held lock on the file with this device and inode.
pub fn table_holds(table: &str, major: u32, minor: u32, inode: u64) -> bool {
    table
        .lines()
        .filter(|line| !line.contains("->"))
        .any(|line| {
            line.split_whitespace().any(|field| {
                let mut parts = field.split(':');
                let (Some(a), Some(b), Some(c), None) =
                    (parts.next(), parts.next(), parts.next(), parts.next())
                else {
                    return false;
                };
                u32::from_str_radix(a, 16) == Ok(major)
                    && u32::from_str_radix(b, 16) == Ok(minor)
                    && c.parse() == Ok(inode)
            })
        })
}

#[cfg(test)]
mod tests;
