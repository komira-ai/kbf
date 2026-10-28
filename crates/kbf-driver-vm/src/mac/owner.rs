//! [`Owner`]: the one daemon that owns a state directory.
//!
//! The start-up sweep ends every run record it finds, on the reading that a record found
//! at start belongs to a daemon that is gone. That holds only if no live daemon shares the
//! directory, so a daemon takes `owner.lock` (`flock(2)`, held as long as the host lives)
//! before it sweeps, and a second one on the same directory is refused instead of ending
//! the first one's VMs. A daemon that dies releases the lock with its descriptors, which
//! is exactly when its records become the next daemon's to sweep.

use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::Path;

use rustix::fs::{FlockOperation, flock};
use rustix::io::Errno;

/// The file in the state directory that the owning daemon locks.
pub const FILE: &str = "owner.lock";

/// The held ownership; dropping it lets another daemon take the directory.
#[derive(Debug)]
pub struct Owner {
    file: File,
}

impl Owner {
    /// Takes ownership of `dir` (made if needed).
    ///
    /// # Errors
    /// `WouldBlock` when a live daemon owns it; another kind when the lock file cannot be
    /// opened or locked.
    pub fn take(dir: &Path) -> io::Result<Self> {
        std::fs::create_dir_all(dir)?;
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(dir.join(FILE))?;
        match flock(&file, FlockOperation::NonBlockingLockExclusive) {
            Err(Errno::WOULDBLOCK) => Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                format!(
                    "another daemon owns the VM state directory {}; one daemon per Mac",
                    dir.display()
                ),
            )),
            other => {
                other?;
                Ok(Self { file })
            }
        }
    }
}

impl Drop for Owner {
    /// Unlocks before the descriptor closes, for the reason [`crate::mac::slots`] does:
    /// a lock belongs to the open file, and a forked child holding a copy would keep it.
    fn drop(&mut self) {
        let _ = flock(&self.file, FlockOperation::Unlock);
    }
}
