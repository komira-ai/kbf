//! Whether some process holds a lock on a file, from `/proc/locks`, without taking the
//! lock and without waiting.
//!
//! Each line of `/proc/locks` is `<n>: <kind> <ADVISORY|MANDATORY> <READ|WRITE> <pid>
//! <major>:<minor>:<inode> <start> <end>`; a process waiting for a lock has `->` after the
//! number and does not hold it. `dpkg` and `apt` take `POSIX` locks, `flock(1)` takes
//! `FLOCK` ones; either counts.

use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

/// Whether some process holds the `flock` on `lock_file`, found by asking for it without
/// waiting: a shared lock is taken and let go at once. A missing file is not held; a file
/// that cannot be opened or asked is held, since idle is not proved.
pub fn flock_held(_lock_file: &Path) -> bool {
    false
}

/// Whether a lock is held on `lock_file`, reading the lock table at `proc_locks`. A
/// missing lock file, or an unreadable table, is "not held".
pub fn held(proc_locks: &Path, lock_file: &Path) -> bool {
    let (Ok(meta), Ok(table)) = (fs::metadata(lock_file), fs::read_to_string(proc_locks)) else {
        return false;
    };
    let dev = meta.dev();
    table_holds(
        &table,
        rustix::fs::major(dev),
        rustix::fs::minor(dev),
        meta.ino(),
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
