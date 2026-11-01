//! What the platform does for an update: the one trait [`crate::local::core::Ab`] stands on.
//!
//! A Linux host keeps its releases in the layout of `kbf-installer`
//! (`/var/lib/kbf-installer/releases/<commit>`, the links `current` and `previous`, one
//! `state.json`); a Mac keeps them in the kit tree. The trait is the same: verify and lay out
//! a release beside the running one, move to it on trial, keep it, or go back. What is on
//! trial is persisted by the platform (in `state.json`, or in the Mac's `pending.json`) in the
//! same step as the move, so a restart of this program or of the host finds it.

use std::path::PathBuf;

use kbf_installer::retain::Kept;

use super::answer::{Answer, Code};
use crate::request::{Channel, Version};

/// A release on trial, as the platform's marker holds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pending {
    /// The release on trial (all 40 digits).
    pub commit: String,
    /// The release that ran before it, which a revert goes back to.
    pub from: String,
    /// When the host goes back by itself (Unix seconds); none when the swap was armed by
    /// something else, which this program does not time out.
    pub deadline_unix: Option<u64>,
    /// What the platform needs to name the swap again when it confirms it.
    pub activation: String,
}

/// A request the platform could not do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fault {
    pub code: Code,
    /// Nothing changed on the host: a refusal, not a failure.
    pub refused: bool,
    /// For the log, never for the answer line.
    pub detail: String,
}

impl Fault {
    /// A refusal: nothing ran.
    pub fn refused(code: Code, detail: impl Into<String>) -> Self {
        Self {
            code,
            refused: true,
            detail: detail.into(),
        }
    }

    /// A failure after something started.
    pub fn failed(code: Code, detail: impl Into<String>) -> Self {
        Self {
            code,
            refused: false,
            detail: detail.into(),
        }
    }

    /// The answer that tells it.
    pub fn answer(self) -> Answer {
        if self.refused {
            Answer::Refused(self.code, self.detail)
        } else {
            Answer::Failed(self.code, self.detail)
        }
    }
}

/// The platform side of an update.
pub trait Backend {
    /// The lock file an install, a switch and a restart hold, shared with the ssh forced
    /// command so that the two never overlap.
    fn lock_file(&self) -> PathBuf;

    /// The node's id, if the host's configuration gives one.
    fn node_id(&self) -> Option<String>;

    /// The release `current` names (all 40 digits).
    ///
    /// # Errors
    /// The links or the platform's records cannot be read.
    fn current(&self) -> Result<Option<String>, Fault>;

    /// The release on trial, if one is.
    ///
    /// # Errors
    /// The platform's marker cannot be read.
    fn pending(&self) -> Result<Option<Pending>, Fault>;

    /// Whether the release `commit` is laid out on this host.
    fn has(&self, commit: &str) -> bool;

    /// Fetch the release `version` names in `channel`, verify it end to end, and lay it out
    /// beside the running one. No link moves and nothing restarts. The commit.
    ///
    /// # Errors
    /// A refusal for anything the root-owned configuration or the release does not allow, a
    /// failure for the rest; nothing but scratch has changed either way.
    fn stage(&self, version: &Version, channel: &Channel) -> Result<String, Fault>;

    /// Put the laid-out release `commit` in place on trial: the marker is written with a
    /// deadline `window_secs` after `now` in the same step as the move (the marker first),
    /// the links move and the service restarts on it. When the restart does not work the host
    /// goes back by itself and the fault says so. The deadline.
    ///
    /// # Errors
    /// The release did not come up and the host is on the release that ran again (or, for a
    /// fault with [`Code::RevertFailed`], is not).
    fn switch(&self, commit: &str, window_secs: u64, now: u64) -> Result<u64, Fault>;

    /// Keep the release on trial.
    ///
    /// # Errors
    /// The marker could not be cleared; the release stays on trial.
    fn confirm(&self, pending: &Pending) -> Result<(), Fault>;

    /// Go back to the release that ran before the one on trial: the links, the restart, and
    /// the marker cleared. Needs nothing of the release on trial to run.
    ///
    /// # Errors
    /// The host could not be put back; the marker stays, so the next try finds it.
    fn revert(&self, pending: &Pending) -> Result<(), Fault>;

    /// The releases the host holds, newest first, each with what the host knows of its
    /// version and why it must stay. None for a host that keeps no release directories (a
    /// package or a container image is the release there).
    fn kept(&self) -> Result<Vec<Kept>, Fault> {
        Ok(Vec::new())
    }

    /// The file that names the releases kept whatever their age (`retain.json`): the pinned
    /// ones and the coordinator's `last_good`. None for a host that keeps no release
    /// directories.
    fn retain_file(&self) -> Option<PathBuf> {
        None
    }

    /// Remove every release directory but the ones that must stay (current, previous, the
    /// pinned ones, `last_good`, what runs) and the `keep` newest others; what it did, in a
    /// line. Only after a confirm, and never while a release is on trial.
    fn prune(&self, _keep: usize) -> Result<String, Fault> {
        Ok(String::new())
    }
}
