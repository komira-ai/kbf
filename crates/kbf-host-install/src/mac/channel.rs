//! The allowlist of a Mac, `/usr/local/etc/kbf/host-install.toml`: the channels an install
//! may take a release from.
//!
//! ```toml
//! [channels.stable]
//! dir = "/usr/local/kbf-host-install/stable"
//! sums = "/usr/local/etc/kbf/host-install/stable.SHA256SUMS"
//! source_ref = "refs/heads/main"          # optional; this is the default
//! ```
//!
//! - `dir` is where the release files are put: `kbf-<commit>-darwin-arm64.tar.gz` and its
//!   `.sigstore.json` bundle. Whoever puts them there is not trusted; they are copied
//!   once and checked.
//! - `sums` is the pinned `SHA256SUMS` of the releases the channel offers: root's alone to
//!   write, like the allowlist itself. A tarball is taken only when its sha256 is listed
//!   there.
//! - `source_ref` is the branch the attestation must have been made for.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::config::refusal;
use crate::outcome::Refusal;
use crate::request::Channel;
use crate::trust::Trust;

/// The branch an attestation is accepted for when the channel names none.
pub const DEFAULT_SOURCE_REF: &str = "refs/heads/main";

/// A channel as the allowlist names it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MacChannel {
    pub dir: PathBuf,
    pub sums: PathBuf,
    pub source_ref: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    channels: BTreeMap<String, Entry>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    dir: PathBuf,
    sums: PathBuf,
    source_ref: Option<String>,
}

/// The parsed allowlist.
#[derive(Debug)]
pub struct MacAllowlist {
    channels: BTreeMap<String, Entry>,
}

impl MacAllowlist {
    /// Read and parse the allowlist at `path`.
    ///
    /// # Errors
    /// [`Refusal::ConfigUnsafe`] when the file is not root's alone to write,
    /// [`Refusal::ConfigInvalid`] when it is missing or does not parse.
    pub fn load(trust: &Trust, path: &Path) -> Result<Self, Refusal> {
        let text = trust.read(path).map_err(refusal)?;
        let file: File = toml::from_str(&text).map_err(|_| Refusal::ConfigInvalid)?;
        let plain = |path: &Path| {
            path.is_absolute()
                && path
                    .to_str()
                    .is_some_and(|p| !p.chars().any(char::is_control))
        };
        let well_formed = file.channels.iter().all(|(name, entry)| {
            Channel::new(name).is_ok()
                && plain(&entry.dir)
                && plain(&entry.sums)
                && entry.source_ref.as_deref().is_none_or(is_branch_ref)
        });
        if !well_formed {
            return Err(Refusal::ConfigInvalid);
        }
        Ok(Self {
            channels: file.channels,
        })
    }

    /// The channel `name`, once its pinned sums are known to be root's.
    ///
    /// # Errors
    /// [`Refusal::ChannelNotAllowed`], [`Refusal::ConfigUnsafe`] or
    /// [`Refusal::ConfigInvalid`].
    pub fn channel(&self, trust: &Trust, name: &Channel) -> Result<MacChannel, Refusal> {
        let entry = self
            .channels
            .get(name.as_str())
            .ok_or(Refusal::ChannelNotAllowed)?;
        trust.check_file(&entry.sums).map_err(refusal)?;
        Ok(MacChannel {
            dir: entry.dir.clone(),
            sums: entry.sums.clone(),
            source_ref: entry
                .source_ref
                .clone()
                .unwrap_or_else(|| DEFAULT_SOURCE_REF.to_owned()),
        })
    }
}

/// `refs/heads/` and a name of letters, digits and `. _ - /`, as the installer accepts one:
/// it is an argument of `gh`, so what cannot be a branch name (a space, a leading `-`, `..`)
/// is refused.
pub fn is_branch_ref(text: &str) -> bool {
    let Some(branch) = text.strip_prefix("refs/heads/") else {
        return false;
    };
    !branch.is_empty()
        && branch
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-/".contains(&b))
        && !branch.starts_with('/')
        && !branch.ends_with('/')
        && !branch.contains("//")
        && !branch.contains("..")
}
