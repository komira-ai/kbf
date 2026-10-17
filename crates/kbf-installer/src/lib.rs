//! The host side of kbf's releases: where a host keeps them, the state it records about
//! them, and the verbs that install, roll back and remove them.
//!
//! - [`layout`]: the paths: each release in its own directory under
//!   `/usr/local/kbf/releases/<commit>/`, the `current` and `previous` links beside them,
//!   kbf-deploy's state under `/var/lib/kbf-deploy/` and its settings under
//!   `/etc/kbf-deploy/`.
//! - [`state`]: `state.json` (the current, previous and staged commits, the one waiting
//!   to be confirmed, and the order floor), read and written under an exclusive lock on
//!   `/run/kbf-deploy.lock` and replaced whole by a rename.
//! - [`release`]: the `install`, `rollback` and `gc` verbs.
//! - [`CommitId`]: a commit as 40 lowercase hex characters, the only form a release is
//!   named by.
//!
//! `kbf-deploy host ...` (the manual path) calls these; so does any other program that
//! changes a host's releases, so that a host has one state file and one layout.

pub mod layout;
pub mod release;
pub mod state;
#[cfg(any(test, feature = "testkit"))]
pub mod testkit;

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
