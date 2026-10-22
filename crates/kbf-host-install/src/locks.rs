//! Whether some process holds a lock on a file, from `/proc/locks`, without taking the
//! lock and without waiting.
//!
//! Each line of `/proc/locks` is `<n>: <kind> <ADVISORY|MANDATORY> <READ|WRITE> <pid>
//! <major>:<minor>:<inode> <start> <end>`; a process waiting for a lock has `->` after the
//! number and does not hold it. `dpkg` and `apt` take `POSIX` locks, `flock(1)` takes
//! `FLOCK` ones; either counts.

use std::path::Path;

/// Whether a lock is held on `lock_file`, reading the lock table at `proc_locks`. A
/// missing lock file, or an unreadable table, is "not held".
pub fn held(proc_locks: &Path, lock_file: &Path) -> bool {
    let _ = (proc_locks, lock_file);
    todo!()
}

/// Whether `table` has a held lock on the file with this device and inode.
pub fn table_holds(table: &str, major: u32, minor: u32, inode: u64) -> bool {
    let _ = (table, major, minor, inode);
    todo!()
}

#[cfg(test)]
mod tests;
