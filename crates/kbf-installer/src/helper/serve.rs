//! The `run` service loop: one pass every [`PASS_INTERVAL`], and a new image of the program
//! between passes when the program's own file was replaced.
//!
//! The package manager replaces `/usr/bin/kbf-installer` under a running service (the
//! installer never updates itself). A pass that began with the old code must not go on
//! acting on orders with it, so the check comes before each pass, never in the middle of
//! one: a pass finishes the orders it started, and the next order is read by the new
//! program. Maintainer scripts call `kbf-installer quiesce` first, so no swap is on trial
//! when the files change.

use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use kbf_host::Exec;

use crate::helper::cycle::{Helper, HelperError};

/// The time between passes. Fixed: it is not a setting.
pub const PASS_INTERVAL: Duration = Duration::from_secs(5);

/// Whether the file this program runs from has been replaced since it started.
pub trait BinaryWatch {
    fn replaced(&self) -> bool;
}

/// A file as it was when the program started: replaced means a different file (or none)
/// is at the path now.
pub struct FileIdentity {
    path: PathBuf,
    seen: (u64, u64, u64, i64, i64),
}

fn identity(path: &Path) -> io::Result<(u64, u64, u64, i64, i64)> {
    let m = std::fs::metadata(path)?;
    Ok((m.dev(), m.ino(), m.size(), m.mtime(), m.mtime_nsec()))
}

impl FileIdentity {
    pub fn of(path: &Path) -> io::Result<FileIdentity> {
        Ok(FileIdentity {
            path: path.to_owned(),
            seen: identity(path)?,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl BinaryWatch for FileIdentity {
    fn replaced(&self) -> bool {
        identity(&self.path).ok() != Some(self.seen)
    }
}

/// What a pass came to.
#[derive(Debug, PartialEq, Eq)]
pub enum Flow {
    /// Sleep [`PASS_INTERVAL`] and pass again.
    Continue,
    /// The program's file was replaced: start the new one in its place. No pass was made.
    Reexec,
}

/// The helper and the watch on the program's own file.
pub struct Serve<E: Exec> {
    helper: Helper<E>,
    watch: Box<dyn BinaryWatch>,
}

impl<E: Exec> Serve<E> {
    pub fn new(helper: Helper<E>, watch: Box<dyn BinaryWatch>) -> Serve<E> {
        Serve { helper, watch }
    }

    pub fn helper(&mut self) -> &mut Helper<E> {
        &mut self.helper
    }

    /// One turn of the loop at `now` (seconds since the epoch).
    pub fn turn(&mut self, now: u64) -> Result<Flow, HelperError> {
        if self.watch.replaced() {
            return Ok(Flow::Reexec);
        }
        self.helper.cycle(now)?;
        Ok(Flow::Continue)
    }
}

#[cfg(test)]
#[path = "serve_tests.rs"]
mod tests;
