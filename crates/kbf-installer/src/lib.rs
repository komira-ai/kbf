//! The host side of kbf's releases: where a host keeps them, the state it records about
//! them, the verbs that install, stage, roll back and remove them, and the offline check
//! that decides whether a release handed over by the unprivileged `kbf` may be staged.
//!
//! The host's records:
//!
//! - [`layout`]: the paths ([`layout::Layout`], a directory tree and its
//!   [`layout::Platform`]): each release in its own directory under
//!   `/usr/local/kbf/releases/<commit>/`, the `current` and `previous` links beside them,
//!   the state under `/var/lib/kbf-deploy/` (Linux) or `/var/db/kbf-deploy/` (macOS) and
//!   its settings under `/etc/kbf-deploy/`.
//! - [`state`]: `state.json` (the current, previous and staged commits, the one waiting
//!   to be confirmed, and the order floor), read and written under an exclusive lock and
//!   replaced whole by a rename.
//! - [`release`]: the `install`, `stage`, `rollback` and `gc` verbs.
//! - [`activate`]: the swap on trial: `activate`, `confirm`, `revert`, the start check
//!   and the expiry, and the repair of the links after an interrupted verb.
//! - [`CommitId`]: a commit as 40 lowercase hex characters, the only form a release is
//!   named by.
//!
//! The root helper of the pull-based updates is [`helper`], run by the `kbf-installer`
//! binary (`run`, `boot-check`). The binary says what it is in [`version`]: `--version`
//! and `--version --json` name the commit and the profile it was built with.
//!
//! The offline check ([`stage::stage_from_inbox`] runs them in the order that matters,
//! documented there):
//!
//! - [`inbox`] and [`offer`]: an entry the unprivileged process wrote, opened without
//!   following a link, and the offer that names its files and their sha256.
//! - [`attest`]: `gh attestation verify`, pinned, offline, as an unprivileged user.
//! - [`trust`] and [`settings`]: what the check believes without looking at its content
//!   (`gh`, the trusted root, the host's settings) and the root-ownership check on it.
//! - [`archive`], [`binary`] and [`darwin`]: the tarball, the kind of executable and its
//!   `--version --json`, and on macOS the quarantine attribute and the code signature.
//! - [`envoy`]: the Envoy of a frontend host, rendered from settings and checked (a library
//!   for the frontend-update path, with no command line).
//! - [`run`]: the one place a process is started, behind a trait a test plays.
//! - [`refusal`]: every way a stage is refused, one enum with a stable code per case.
//!
//! `kbf-deploy host ...` (the manual path) calls these; so does any other program that
//! changes a host's releases, so that a host has one state file and one layout. The
//! trust rests on GitHub's build attestation of the artifacts workflow on the host's
//! source ref (docs/deploy/host-releases.md, "What is trusted").

pub mod activate;
pub mod app;
pub mod archive;
pub mod attest;
pub mod binary;
pub mod cli;
pub mod darwin;
pub mod envoy;
pub mod exit;
#[cfg(test)]
mod fixtures;
#[cfg(any(test, feature = "testkit"))]
pub mod golden;
pub mod helper;
pub mod inbox;
pub mod installer_toml;
pub mod layout;
pub mod offer;
pub mod record;
pub mod refusal;
pub mod release;
pub mod run;
pub mod settings;
pub mod stage;
pub mod state;
pub mod state_v2;
pub mod status_v2;
#[cfg(any(test, feature = "testkit"))]
pub mod testkit;
pub mod trust;
pub mod version;

use std::fmt;

use serde::{Deserialize, Serialize};

/// A commit, as Git names it in full: exactly 40 lowercase hex characters. A release
/// directory, a link target and the state name commits only in this form, so a short or
/// upper-case id can never name a second copy of one release.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct CommitId(String);

/// Why a string is not a [`CommitId`].
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{0:?} is not a commit id (40 lowercase hex characters)")]
pub struct NotACommit(pub String);

impl CommitId {
    pub fn new(id: &str) -> Result<CommitId, NotACommit> {
        let hex = |b: u8| matches!(b, b'0'..=b'9' | b'a'..=b'f');
        if id.len() == 40 && id.bytes().all(hex) {
            Ok(CommitId(id.to_owned()))
        } else {
            Err(NotACommit(id.to_owned()))
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for CommitId {
    type Error = NotACommit;

    fn try_from(id: String) -> Result<CommitId, NotACommit> {
        CommitId::new(&id)
    }
}

impl From<CommitId> for String {
    fn from(id: CommitId) -> String {
        id.0
    }
}

impl std::str::FromStr for CommitId {
    type Err = NotACommit;

    fn from_str(id: &str) -> Result<CommitId, NotACommit> {
        CommitId::new(id)
    }
}

impl fmt::Display for CommitId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests;
