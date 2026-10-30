//! The cordon the updater made, remembered across retries and restarts.
//!
//! The frontend's cordon carries no owner: `placement.state` says a node is out of placement,
//! not who put it there. A roll must leave out a node an operator drained and give back one
//! the updater drained, and the second can look like the first: an update that failed with the
//! node left drained (the drain deadline passed, an undrain failed) or an agent that restarted
//! between the drain and the undrain finds its own cordon on the next attempt.
//!
//! [`OwnCordon`] keeps that on disk. A drain of a node that is in placement first writes a
//! mark file for the node (and syncs it and its directory), then drains. The mark is removed
//! only after an undrain worked. A node with a mark reads as [`Placed::In`] whatever the
//! frontend says; a node with none reads as the frontend says. The mark lives in the agent's
//! cordon directory (`--cordon-dir`, else `--state-dir`, else the directory of
//! `--client-key`), one file per node, so it survives a restart of the agent and of the host.
//!
//! What this does not know: a cordon an operator adds to a node the updater has drained is
//! still the updater's, and a node drained by an agent that predates the marks reads as an
//! operator's.

use std::fs::{File, OpenOptions};
use std::io::{ErrorKind, Write as _};
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use crate::inventory::{Host, valid_node_id};
use crate::ports::{Drain, Placed, PortError};

const MARK: &[u8] = b"cordoned by kbf-updater\n";

/// A [`Drain`] that remembers, on disk, the nodes it cordoned itself.
pub struct OwnCordon {
    inner: Arc<dyn Drain>,
    dir: PathBuf,
}

impl OwnCordon {
    pub fn new(inner: Arc<dyn Drain>, dir: impl Into<PathBuf>, _unit: &str) -> Self {
        Self {
            inner,
            dir: dir.into(),
        }
    }

    /// Whether the directory takes marks.
    ///
    /// # Errors
    /// It does not.
    pub fn check_writable(&self) -> Result<(), String> {
        Ok(())
    }

    /// The mark of `host`'s node. A host with no usable node id has none: the inner drain
    /// refuses it by itself.
    fn mark(&self, host: &Host) -> Option<PathBuf> {
        let node = host.node_id.as_deref().filter(|id| valid_node_id(id))?;
        Some(self.dir.join(format!("cordon-{node}")))
    }

    fn fail(what: &str, path: &Path, e: &std::io::Error) -> PortError {
        PortError::other(format!("cannot {what} {}: {e}", path.display()))
    }

    fn is_set(mark: &Path) -> Result<bool, PortError> {
        mark.try_exists()
            .map_err(|e| Self::fail("look for the cordon mark", mark, &e))
    }

    /// Write the mark and make it, and the directory entry, durable before returning.
    fn set(&self, mark: &Path) -> Result<(), PortError> {
        OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(mark)
            .and_then(|mut file| file.write_all(MARK).and_then(|()| file.sync_all()))
            .and_then(|()| self.sync_dir())
            .map_err(|e| Self::fail("record the cordon in", mark, &e))
    }

    /// Remove the mark, if there is one, and make that durable.
    fn clear(&self, mark: &Path) -> Result<(), PortError> {
        std::fs::remove_file(mark)
            .map(|()| true)
            .or_else(|e| (e.kind() == ErrorKind::NotFound).then_some(false).ok_or(e))
            .and_then(|removed| if removed { self.sync_dir() } else { Ok(()) })
            .map_err(|e| Self::fail("clear the cordon mark", mark, &e))
    }

    fn sync_dir(&self) -> std::io::Result<()> {
        File::open(&self.dir).and_then(|dir| dir.sync_all())
    }
}

impl Drain for OwnCordon {
    fn placed(&self, host: &Host) -> Result<Placed, PortError> {
        if let Some(mark) = self.mark(host)
            && Self::is_set(&mark)?
        {
            return Ok(Placed::In);
        }
        self.inner.placed(host)
    }

    /// The mark is written before the drain call, and kept if the call fails (the frontend
    /// may have applied it). A node already out and unmarked is an operator's: no mark.
    fn drain(&self, host: &Host, deadline: Option<Duration>) -> Result<(), PortError> {
        if let Some(mark) = self.mark(host)
            && !Self::is_set(&mark)?
            && self.inner.placed(host)? == Placed::In
        {
            self.set(&mark)?;
        }
        self.inner.drain(host, deadline)
    }

    fn leases(&self, host: &Host) -> Result<usize, PortError> {
        self.inner.leases(host)
    }

    /// The mark is cleared only after the node was returned to placement.
    fn undrain(&self, host: &Host) -> Result<(), PortError> {
        self.inner.undrain(host)?;
        match self.mark(host) {
            Some(mark) => self.clear(&mark),
            None => Ok(()),
        }
    }
}
