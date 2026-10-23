//! The allowlist of a Mac, `/usr/local/etc/kbf/host-install.toml`: the channels an install
//! may take a release from.
//!
//! ```toml
//! [channels.stable]
//! dir = "/usr/local/kbf-host-install/stable"
//! sums = "/usr/local/etc/kbf/host-install/stable.SHA256SUMS"   # optional
//! source_ref = "refs/heads/main"          # optional; this is the default
//!
//! [channels.online]
//! source = "github"
//! token_file = "/usr/local/etc/kbf/host-install/github.token"   # optional
//! ```
//!
//! A channel takes its releases from exactly one place: a `dir`, or `source = "github"`.
//!
//! - `dir` is where the release files are put: `kbf-<commit>-darwin-arm64.tar.gz` and its
//!   `.sigstore.json` bundle. Whoever puts them there is not trusted; they are copied
//!   once and checked.
//! - `source = "github"` fetches the release from the repository's GitHub releases
//!   ([`super::fetch`]), as `nobody`; what it fetches is copied once and checked like the
//!   files of a `dir`. `token_file` (root's alone to write) holds a token for the public
//!   API, which `gh api` needs to run at all.
//! - `sums` is optional. The trust anchor is the build attestation, which every release
//!   must pass whether or not the channel has sums. When the channel sets `sums`, it is an
//!   extra freeze list: the pinned `SHA256SUMS` of the releases the channel offers, root's
//!   alone to write like the allowlist itself, and a release is taken only when its
//!   tarball's sha256 is listed there. When it does not, the release is the one tarball in
//!   `dir` whose commit starts with the version's 12 hex digits.
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

/// Where a channel's releases come from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// Files put in a directory.
    Dir(PathBuf),
    /// The GitHub releases of the repository, with the file of the token for the API when
    /// the channel names one.
    Github { token_file: Option<PathBuf> },
}

/// A channel as the allowlist names it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MacChannel {
    pub source: Source,
    pub sums: Option<PathBuf>,
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
    dir: Option<PathBuf>,
    source: Option<String>,
    token_file: Option<PathBuf>,
    sums: Option<PathBuf>,
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
                && entry.dir.as_deref().is_none_or(plain)
                && entry.token_file.as_deref().is_none_or(plain)
                && source_of(entry).is_some()
                && entry.sums.as_deref().is_none_or(plain)
                && entry.source_ref.as_deref().is_none_or(is_branch_ref)
        });
        if !well_formed {
            return Err(Refusal::ConfigInvalid);
        }
        Ok(Self {
            channels: file.channels,
        })
    }

    /// The channel `name`, once its pinned sums (if it has any) are known to be root's.
    ///
    /// # Errors
    /// [`Refusal::ChannelNotAllowed`], [`Refusal::ConfigUnsafe`] or
    /// [`Refusal::ConfigInvalid`].
    pub fn channel(&self, trust: &Trust, name: &Channel) -> Result<MacChannel, Refusal> {
        let entry = self
            .channels
            .get(name.as_str())
            .ok_or(Refusal::ChannelNotAllowed)?;
        if let Some(sums) = &entry.sums {
            trust.check_file(sums).map_err(refusal)?;
        }
        let source = source_of(entry).ok_or(Refusal::ConfigInvalid)?;
        if let Source::Github {
            token_file: Some(token),
        } = &source
        {
            trust.check_file(token).map_err(refusal)?;
        }
        Ok(MacChannel {
            source,
            sums: entry.sums.clone(),
            source_ref: entry
                .source_ref
                .clone()
                .unwrap_or_else(|| DEFAULT_SOURCE_REF.to_owned()),
        })
    }
}

/// The one place an entry takes releases from; none when it names two, none, or a source
/// that is not `github`, or a token file without the `github` source.
fn source_of(entry: &Entry) -> Option<Source> {
    match (&entry.dir, entry.source.as_deref()) {
        (Some(dir), None) if entry.token_file.is_none() => Some(Source::Dir(dir.clone())),
        (None, Some("github")) => Some(Source::Github {
            token_file: entry.token_file.clone(),
        }),
        _ => None,
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
