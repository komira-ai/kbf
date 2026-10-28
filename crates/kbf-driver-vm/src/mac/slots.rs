//! [`SlotLock`]: at most two macOS VMs run on one Mac (the licence allows two, and
//! Virtualization.framework refuses a third), counted by two lock files.
//!
//! A start takes one slot before it asks the helper or the framework for anything, and
//! gives it back only after its run has ended. The lock is `flock(2)` on a file of its
//! own per slot. One daemon owns a state directory ([`crate::mac::owner`]), so the locks
//! count its VMs; a daemon that dies releases its slots with its descriptors (its runs are
//! ended by the next start's reap, or have already stopped with the control pipe, and until
//! then the framework's own limit is what refuses a third).

use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::Path;

use rustix::fs::{FlockOperation, flock};
use rustix::io::Errno;

/// A held slot; dropping it frees the slot.
#[derive(Debug)]
pub struct SlotLock {
    file: File,
    index: usize,
}

impl SlotLock {
    /// Takes the first free of the slots `slot-0.lock` .. `slot-<count-1>.lock` in `dir`
    /// (made if needed); `None` when all are held.
    ///
    /// # Errors
    /// A lock file cannot be opened or locked for a reason other than being held.
    pub fn acquire(dir: &Path, count: usize) -> io::Result<Option<Self>> {
        std::fs::create_dir_all(dir)?;
        for index in 0..count {
            let file = OpenOptions::new()
                .create(true)
                .truncate(false)
                .write(true)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW)
                .open(dir.join(format!("slot-{index}.lock")))?;
            match flock(&file, FlockOperation::NonBlockingLockExclusive) {
                // Held by another: try the next slot.
                Err(Errno::WOULDBLOCK) => {}
                other => {
                    other?;
                    return Ok(Some(Self { file, index }));
                }
            }
        }
        Ok(None)
    }

    /// Which slot this is.
    #[must_use]
    pub fn index(&self) -> usize {
        self.index
    }
}

impl Drop for SlotLock {
    /// Unlocks before the descriptor closes: a lock belongs to the open file, so a
    /// process forked while this was held would otherwise keep the slot taken until it
    /// executes its program (the descriptor is close-on-exec, but that is later).
    fn drop(&mut self) {
        let _ = flock(&self.file, FlockOperation::Unlock);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Catches: a slot that stays taken after its holder is dropped because a process
    /// forked meanwhile holds a copy of the lock file's descriptor (a lock belongs to the
    /// open file, not to one descriptor of it).
    #[test]
    fn a_slot_is_free_once_dropped_though_a_copy_of_its_file_is_open() {
        let dir = crate::mac::tests::dir("slots-dup");
        let lock = SlotLock::acquire(dir.path(), 1).unwrap().unwrap();
        let copy = lock.file.try_clone().unwrap();
        drop(lock);
        assert!(SlotLock::acquire(dir.path(), 1).unwrap().is_some());
        drop(copy);
    }
}
