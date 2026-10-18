//! The pinned Envoy: the proxy the frontend's `kbf-envoy@` units run, fetched once from
//! its upstream URL, checked against the sha256 the attested release manifest names, and
//! kept under the installer tree where nothing but root can change it
//! (docs/design/kbf-installer.md, sections 3, 5 step 8, 7 and 10 S18).
//!
//! ```text
//! /var/lib/kbf-installer/envoy/<version>-<sha256[:16]>/envoy         the binary, 0755
//! /var/lib/kbf-installer/envoy/<version>-<sha256[:16]>/envoy.sha256  its full sha256
//! state.json  "envoy": {"current": "<id>", "previous": "<id>"}
//! ```
//!
//! - [`manifest`]: the manifest's `envoy` section per platform, `{version, url, sha256}`,
//!   and the rules it is held to.
//! - [`fetch`]: how the bytes arrive: a [`fetch::Fetcher`], so the fetch child of the
//!   installer slots in behind the same trait ([`fetch::CurlFetcher`] is the minimal one).
//! - [`install`]: `fetch` (download, hash, ELF machine, `--version`, install, pin),
//!   `verify` (the same checks on what is installed, for the unit's `ExecStartPre`),
//!   `path` and `list`.
//! - [`cli`]: the four verbs as `kbf-installer envoy ...` parses them: `--json`, a dry run
//!   without `--apply`, and the exit codes of section 2.
//!
//! What a download is checked for, in order, and nothing is run before the third:
//!
//! 1. The file is opened beneath the child's download directory without following a link,
//!    owned by the fetch user, a regular file with one link and within the size cap, and
//!    copied by its descriptor into a directory root owns ([`crate::inbox`]).
//! 2. The copy's sha256 is the one the manifest names.
//! 3. Its header is an ELF for this host's CPU ([`crate::binary`]).
//! 4. `envoy --version` is run as the unprivileged verify user, in an empty directory with
//!    an empty environment, and names the manifest's version.
//! 5. The copy is written into `envoy/.stage-<id>.<pid>/`, hashed again as it is written,
//!    and renamed to `envoy/<id>`. A directory already there with other bytes is refused,
//!    never replaced; one with the same bytes is left alone (so a repeat is a no-op).
//! 6. `state.json` records it: `current` is the new build, `previous` the one it replaced.
//!
//! `verify` repeats 2 to 4 on the build `state.json` pins, and that the files are root's
//! and closed to group and other writes, so a build swapped on disk is refused at the next
//! start, not run.

pub mod cli;
pub mod fetch;
pub mod install;
pub mod manifest;
mod refusal;

#[cfg(test)]
mod cli_tests;
#[cfg(test)]
mod install_tests;
#[cfg(test)]
mod manifest_tests;
#[cfg(test)]
mod testing;

use std::fmt;

use serde::{Deserialize, Serialize};

pub use refusal::{EnvoyError, VersionFault};

/// How many hex digits of the sha256 are in a build's name.
pub const SHA_PREFIX: usize = 16;

/// A pinned Envoy build: `<version>-<first 16 hex digits of its sha256>`, the name of its
/// directory. Only this form is ever a path component, so neither the manifest nor
/// `state.json` can name a path outside `envoy/`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct EnvoyId(String);

/// Why a string is not an [`EnvoyId`] (or a part of one).
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{0:?} is not an Envoy build id (<major>.<minor>.<patch>-<16 lowercase hex digits>)")]
pub struct NotAnEnvoyId(pub String);

impl EnvoyId {
    /// The id of the build `version` with the (full) `sha256`.
    pub fn new(version: &str, sha256: &str) -> Result<EnvoyId, NotAnEnvoyId> {
        let id = format!("{version}-{}", sha256.get(..SHA_PREFIX).unwrap_or(sha256));
        EnvoyId::parse(&id)
    }

    /// `id` as the directory name spells it.
    pub fn parse(id: &str) -> Result<EnvoyId, NotAnEnvoyId> {
        match id.rsplit_once('-') {
            Some((version, sha)) if valid_version(version) && valid_prefix(sha) => {
                Ok(EnvoyId(id.to_owned()))
            }
            _ => Err(NotAnEnvoyId(id.to_owned())),
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The version part.
    pub fn version(&self) -> &str {
        self.split().0
    }

    /// The first 16 hex digits of the sha256.
    pub fn sha_prefix(&self) -> &str {
        self.split().1
    }

    fn split(&self) -> (&str, &str) {
        // `parse` guarantees one `-` with the version before it.
        self.0.rsplit_once('-').unwrap_or((&self.0, ""))
    }
}

/// `^\d+\.\d+\.\d+$`.
pub fn valid_version(version: &str) -> bool {
    let mut parts = 0;
    for part in version.split('.') {
        parts += 1;
        if part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()) {
            return false;
        }
    }
    parts == 3
}

fn valid_prefix(sha: &str) -> bool {
    sha.len() == SHA_PREFIX && sha.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// A full lowercase hex sha256.
pub fn valid_sha256(sha: &str) -> bool {
    sha.len() == 64 && sha.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

impl TryFrom<String> for EnvoyId {
    type Error = NotAnEnvoyId;

    fn try_from(id: String) -> Result<EnvoyId, NotAnEnvoyId> {
        EnvoyId::parse(&id)
    }
}

impl From<EnvoyId> for String {
    fn from(id: EnvoyId) -> String {
        id.0
    }
}

impl fmt::Display for EnvoyId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}
