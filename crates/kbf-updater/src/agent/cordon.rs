//! The cordon the updater made, remembered across retries and restarts.
//!
//! The frontend's cordon carries no owner: `placement.state` says a node is out of placement,
//! not who put it there. A roll must leave out a node an operator drained and give back one
//! the updater drained, and the second can look like the first: an update that failed with the
//! node left drained (the drain deadline passed, an undrain failed) or an agent that restarted
//! between the drain and the undrain finds its own cordon on the next attempt.
//!
//! [`OwnCordon`] keeps that on disk. A drain of a node that is in placement first writes a
//! mark file for the node and the unit being updated (`cordon-<node>.<unit>`, `kbf` for the
//! agent's own update, the service's name for a service's; synced, with its directory), then
//! drains. The mark is removed only after an undrain worked. A node with any mark reads as
//! [`Placed::In`]; a node with none reads as the frontend says. The marks live in the agent's
//! cordon directory (`--cordon-dir`, else `--state-dir`, else the directory of `--client-key`),
//! so they survive a restart of the agent and of the host.
//!
//! - **A mark means something only while its cordon is there.** Whenever the agent reads a
//!   marked node and finds it serving (in [`Drain::placed`], and in [`Drain::observe`], which
//!   the agent calls for every unit at start and at each idle heartbeat), the cordon was given
//!   back by hand and every mark of the node is removed. A frontend that cannot be asked
//!   leaves the marks and fails the read. The frontend's node read carries nothing that names
//!   one cordon (no generation, no time), so a cordon given back and made again by an operator between two
//!   looks is not told from the updater's.
//! - **Only a drain is ours.** The updater drains; it never makes a plain cordon. A marked node
//!   the frontend reads as cordoned (not draining, drained or paused) was cordoned by an
//!   operator, over the updater's drain or after it was given back, and its marks are removed:
//!   it reads as [`Placed::Out`].
//! - **One mark per unit.** An undrain removes its own mark and uncordons only when no mark of
//!   another unit remains, so one unit finishing does not give back a node another is still
//!   rolling on.
//! - **No node, no mark.** A host the frontend knows no node for (a storage host with no daemon)
//!   is drained as nothing and marked as nothing.
//!
//! What this does not know: a drain an operator makes of a node the updater has drained is
//! still the updater's, and a node drained by an agent that predates the marks reads as an
//! operator's.

use std::fs::{File, OpenOptions};
use std::io::{ErrorKind, Write as _};
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use crate::inventory::{Host, valid_node_id};
use crate::ports::{Drain, Placed, PortError, Seen};

const MARK: &[u8] = b"cordoned by kbf-updater\n";

/// A [`Drain`] that remembers, on disk, the nodes it cordoned itself.
pub struct OwnCordon {
    inner: Arc<dyn Drain>,
    dir: PathBuf,
    /// The unit this drain serves: `kbf`, or a service's name.
    unit: String,
}

impl OwnCordon {
    pub fn new(inner: Arc<dyn Drain>, dir: impl Into<PathBuf>, unit: &str) -> Self {
        Self {
            inner,
            dir: dir.into(),
            unit: unit.to_owned(),
        }
    }

    /// Whether the directory takes marks: a probe file is created, synced and removed.
    ///
    /// # Errors
    /// It does not; the text names `--cordon-dir`.
    pub fn check_writable(&self) -> Result<(), String> {
        let probe = self.dir.join(format!(
            ".cordon-probe-{}-{}",
            self.unit,
            std::process::id()
        ));
        let tried = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&probe)
            .and_then(|mut file| file.write_all(MARK).and_then(|()| file.sync_all()))
            .and_then(|()| self.sync_dir())
            .and_then(|()| std::fs::remove_file(&probe))
            .and_then(|()| self.sync_dir());
        tried.map_err(|e| {
            format!(
                "--cordon-dir {}: the agent cannot write there ({e}); it records the cordons it \
                 makes in that directory and refuses to drain a node it cannot record. Give \
                 --cordon-dir a directory the agent can write (under the packaged unit, one \
                 inside its StateDirectory)",
                self.dir.display()
            )
        })
    }

    /// The node of `host`. A host with no usable node id has none: the inner drain refuses it
    /// by itself.
    fn node(host: &Host) -> Option<&str> {
        host.node_id.as_deref().filter(|id| valid_node_id(id))
    }

    /// This unit's mark of `host`'s node. A node id is a label, which holds no `.`, so the
    /// marks of one node are exactly the files that start with `cordon-<node>.`.
    fn mark(&self, host: &Host) -> Option<PathBuf> {
        let node = Self::node(host)?;
        Some(self.dir.join(format!("cordon-{node}.{}", self.unit)))
    }

    /// Every mark of `host`'s node, of any unit.
    fn marks(&self, host: &Host) -> Result<Vec<PathBuf>, PortError> {
        let Some(node) = Self::node(host) else {
            return Ok(Vec::new());
        };
        let prefix = format!("cordon-{node}.");
        let listed =
            std::fs::read_dir(&self.dir).and_then(|entries| entries.collect::<Result<Vec<_>, _>>());
        let entries = match listed {
            Ok(entries) => entries,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(Self::fail("look for the cordon marks in", &self.dir, &e)),
        };
        Ok(entries
            .into_iter()
            .filter(|entry| entry.file_name().to_string_lossy().starts_with(&prefix))
            .map(|entry| entry.path())
            .collect())
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
    /// A marked node reads as in placement: the cordon on it is ours, unless the frontend says
    /// it serves (given back: its marks are removed) or cordoned (an operator's: marks removed,
    /// node out). A frontend that cannot be asked fails the read and leaves the marks.
    fn placed(&self, host: &Host) -> Result<Placed, PortError> {
        let marks = self.marks(host)?;
        if marks.is_empty() {
            return self.inner.placed(host);
        }
        match self.inner.seen_if_known(host) {
            // Still in a drain: the cordon is ours.
            Ok(Some(Seen::Draining)) => Ok(Placed::In),
            // The frontend cannot say, so nothing says the cordon is still ours: the marks stay
            // and the caller fails, leaving the node drained.
            Err(e) => Err(e),
            // The updater only drains. A plain cordon was put there by an operator, over our
            // drain or after it was given back: the node is theirs.
            Ok(Some(Seen::Cordoned)) => {
                marks.iter().try_for_each(|mark| self.clear(mark))?;
                Ok(Placed::Out)
            }
            // Given back by hand, or no node here.
            Ok(Some(Seen::Serving) | None) => {
                marks.iter().try_for_each(|mark| self.clear(mark))?;
                Ok(Placed::In)
            }
        }
    }

    fn placed_if_known(&self, host: &Host) -> Result<Option<Placed>, PortError> {
        self.inner.placed_if_known(host)
    }

    fn seen_if_known(&self, host: &Host) -> Result<Option<Seen>, PortError> {
        self.inner.seen_if_known(host)
    }

    /// The mark is written before the drain call, and kept if the call fails (the frontend
    /// may have applied it). A node already out and unmarked is an operator's, and a node the
    /// frontend does not know is nobody's: no mark.
    fn drain(&self, host: &Host, deadline: Option<Duration>) -> Result<(), PortError> {
        // Marks of a cordon that has changed hands are dropped before they count.
        if !self.marks(host)?.is_empty() {
            self.placed(host)?;
        }
        if let Some(mark) = self.mark(host)
            && !Self::is_set(&mark)?
            && (!self.marks(host)?.is_empty()
                || self.inner.placed_if_known(host)? == Some(Placed::In))
        {
            self.set(&mark)?;
        }
        self.inner.drain(host, deadline)
    }

    fn leases(&self, host: &Host) -> Result<usize, PortError> {
        self.inner.leases(host)
    }

    /// This unit's mark is cleared after the node was returned to placement, and the node is
    /// returned only if no other unit's mark remains.
    fn undrain(&self, host: &Host) -> Result<(), PortError> {
        let Some(own) = self.mark(host) else {
            return self.inner.undrain(host);
        };
        if self.marks(host)?.iter().all(|mark| *mark == own) {
            self.inner.undrain(host)?;
        }
        self.clear(&own)
    }

    /// A marked node is read, so that a cordon given back by hand is noticed; an unmarked one
    /// is not asked about.
    fn observe(&self, host: &Host) -> Result<(), PortError> {
        if self.marks(host)?.is_empty() {
            return Ok(());
        }
        self.placed(host).map(drop)
    }
}
