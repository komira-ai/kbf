//! `installer.toml` as the release fetcher reads it: through the trust files' rule.
//!
//! [`InstallerToml::parse`] reads text and checks its shape. The file also decides where a
//! release comes from and which versions the host refuses, so whoever reads it to fetch must
//! first know that nobody but root can have written it. That check ([`trusted_chain`]) is
//! here, with the fetcher that needs it, and not in `crate::installer_toml`, which `setup`,
//! `check` and the proxy verb read without it.

use kbf_host::exec::{Entry, Exec, Kind};
use kbf_host::path::HostPath;

use crate::installer_toml::{ConfigError, InstallerToml};
use crate::refusal::{Refusal, TrustFault};
use crate::trust::trusted_chain;

/// Why the file at a path could not be taken as the installer's settings.
#[derive(Debug, thiserror::Error)]
pub enum ReadError {
    /// It is missing, not a plain file, owned by someone but root, writable by group or
    /// others, or in a directory that is ([`trusted_chain`]).
    #[error("{0}")]
    Untrusted(Refusal),
    #[error("{0}")]
    Invalid(ConfigError),
}

/// The settings in `file`, read through `trust` (the host's own executor, or a test's).
///
/// The file decides where a release comes from and which versions the host refuses, so it is
/// held to the rule of the trust files ([`trusted_chain`]): a plain file of root's that group
/// and others cannot write, in directories of root's that they cannot write. `setup` leaves it
/// so (root:root 0644 in a root-owned 0755 directory).
///
/// # Errors
/// [`ReadError::Untrusted`] when the rule is not met (or the file cannot be read),
/// [`ReadError::Invalid`] when its content is not an `installer.toml`.
pub fn read_trusted(trust: &dyn Exec, file: &HostPath) -> Result<InstallerToml, ReadError> {
    let untrusted = |fault| {
        ReadError::Untrusted(Refusal::Untrusted {
            path: file.as_str().into(),
            fault,
        })
    };
    let read = trust
        .inspect(file)
        .map_err(|e| untrusted(TrustFault::Unreadable(e)))?;
    let Some(entry) = read else {
        return Err(untrusted(TrustFault::Missing));
    };
    // A link, a directory or a device where the file should be.
    let Entry::File { bytes, .. } = entry else {
        return Err(untrusted(TrustFault::Link));
    };
    trusted_chain(trust, file, Kind::File).map_err(ReadError::Untrusted)?;
    let text = String::from_utf8(bytes)
        .map_err(|_| ReadError::Invalid(ConfigError::Syntax("it is not UTF-8".to_owned())))?;
    InstallerToml::parse(&text).map_err(ReadError::Invalid)
}

#[cfg(test)]
#[path = "trusted_toml_tests.rs"]
mod tests;
