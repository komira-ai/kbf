//! The allowlist, `/etc/kbf/host-install.toml`, and the apt source it points at.
//!
//! ```toml
//! [channels.candidate]
//! suite = "candidate"
//! source_list = "/etc/kbf/host-install/candidate.sources"
//! ```
//!
//! The file, and each source file, must be root's alone to write ([`crate::trust`]). A
//! channel is usable only when its source names a `signed-by` key on every entry, as an
//! absolute path to a file that is itself root's alone to write: an unsigned source, a key
//! given as a fingerprint or inline, or a key a user could replace is refused. The source
//! is either a deb822 file (`Types:`, `Signed-By:`) or a one-line apt list
//! (`deb [signed-by=/path] ...`).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::outcome::Refusal;
use crate::request::Channel;
use crate::trust::Trust;

/// A channel after its checks: what the plan needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelSource {
    /// The suite apt installs from (`-t`).
    pub suite: String,
    /// The only apt source file the install reads.
    pub source_list: PathBuf,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    channels: BTreeMap<String, Entry>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    suite: String,
    source_list: PathBuf,
}

/// The parsed allowlist.
#[derive(Debug)]
pub struct Allowlist {
    channels: BTreeMap<String, Entry>,
}

impl Allowlist {
    /// Read and parse the allowlist at `path`.
    ///
    /// # Errors
    /// [`Refusal::ConfigUnsafe`] when the file is not root's alone to write,
    /// [`Refusal::ConfigInvalid`] when it is missing or does not parse.
    pub fn load(trust: &Trust, path: &Path) -> Result<Self, Refusal> {
        let _ = (trust, path);
        todo!()
    }

    /// The channel `name`, once its source passes the signing checks.
    ///
    /// # Errors
    /// [`Refusal::ChannelNotAllowed`], [`Refusal::ConfigUnsafe`],
    /// [`Refusal::ConfigInvalid`], [`Refusal::ChannelUnsigned`] or
    /// [`Refusal::KeyUntrusted`].
    pub fn channel(&self, trust: &Trust, name: &Channel) -> Result<ChannelSource, Refusal> {
        let _ = (trust, name, &self.channels);
        todo!()
    }
}

#[cfg(test)]
mod tests;
