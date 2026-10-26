//! Whether some process holds a lock on a file: the install lock by asking for it without
//! waiting ([`flock_held`]), any lock from `/proc/locks` without taking one ([`held`]).
//!
//! Each line of `/proc/locks` is `<n>: <kind> <ADVISORY|MANDATORY> <READ|WRITE> <pid>
//! <major>:<minor>:<inode> <start> <end>`; a process waiting for a lock has `->` after the
//! number and does not hold it. `dpkg` and `apt` take `POSIX` locks, `flock(1)` takes
//! `FLOCK` ones; either counts.

use std::fs::{self, OpenOptions};
use std::io::ErrorKind;
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

/// For tests: wait (up to 500 looks, 10 ms apart: 5 s) until nobody holds or shares the
/// `flock` on `lock_file`, so that an exclusive lock can be had. A process another test thread
/// starts copies every open descriptor until it execs, and a lock lives as long as any copy of
/// the descriptor does, so a lock this test has dropped can outlive the drop by a moment. (The
/// program itself starts nothing while it holds or probes a lock.) A lock that never goes is
/// left for the caller's own assertion to report.
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
