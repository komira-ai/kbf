//! The first configuration of a kbf host: the library behind `kbf-installer setup`.
//!
//! - [`setup`]: the verb itself: it validates its flags, then makes the host what they say
//!   (accounts, directories, `installer.toml`, `kbf.toml`, the units, the state database's
//!   schema), as a plan of `kbf-host` steps that is printed without `--apply`.
//! - [`cli`]: the flags the verb takes ([`cli::SetupArgs`]) and the `kbf` modes a host can be
//!   set up for ([`cli::Mode`]).
//! - [`layout`]: the paths ([`layout::Layout`], a directory tree and its [`layout::Platform`]).
//! - [`installer_toml`]: the installer's settings file, read closed and written whole.
//! - [`database`]: the state database the installer owns: its URL, and its schema created or
//!   brought forward.
//! - [`exit`]: the exit codes of the `kbf-installer` binary.
//! - [`sha256_hex`]: a SHA-256 digest written as text.
//! - [`CommitId`]: a commit as 40 lowercase hex characters, the only form a release is named
//!   by.
//!
//! `kbf-installer` re-exports these modules under the paths it has always had
//! (`kbf_installer::layout`, `kbf_installer::setup`, ...); this crate does not depend on it.

pub mod cli;
pub mod database;
pub mod exit;
#[cfg(test)]
mod golden;
pub mod installer_toml;
pub mod layout;
pub mod setup;
pub mod sha256_hex;

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

impl AsRef<str> for CommitId {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl fmt::Display for CommitId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}
