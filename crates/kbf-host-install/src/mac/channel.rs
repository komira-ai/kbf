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
//!
//! # A channel of another product (the allow-list must name its workflow):
//! trusted_workflows = ["owner/other/.github/workflows/release.yml"]  # top level, before the first [channels.*]
//! [channels.other]
//! source = "github"
//! repository = "owner/other"
//! signer_workflow = "owner/other/.github/workflows/release.yml"
//! artifact_prefix = "other"               # optional; `kbf` is the default
//! ```
//!
//! A channel takes its releases from exactly one place: a `dir`, or `source = "github"`.
//!
//! - `dir` is where the release files are put: `kbf-<commit>-darwin-arm64.tar.gz` and its
//!   `.sigstore.json` bundle. Whoever puts them there is not trusted; they are copied
//!   once and checked.
//! - `source = "github"` fetches the release from the repository's GitHub releases
//!   ([`super::fetch`]), as `nobody`; what it fetches is copied once and checked like the
//!   files of a `dir`. `token_file` is optional: root's alone to write, it holds a token
//!   for the public API that raises the rate limit of the listing; without one the listing
//!   is unauthenticated.
//! - `sums` is optional. The trust anchor is the build attestation, which every release
//!   must pass whether or not the channel has sums. When the channel sets `sums`, it is an
//!   extra freeze list: the pinned `SHA256SUMS` of the releases the channel offers, root's
//!   alone to write like the allowlist itself, and a release is taken only when its
//!   tarball's sha256 is listed there. When it does not, the release is the one tarball in
//!   `dir` whose commit starts with the version's 12 hex digits.
//! - `source_ref` is the branch the attestation must have been made for.
//! - `repository` and `signer_workflow` (both or neither) name the repository the releases
//!   are listed from and the workflow whose attestation is believed; `artifact_prefix` is
//!   the start of the release files' names (a `source = "github"` channel without `sums`
//!   only: the names of a directory and of sums are kbf's). Absent, they are kbf's, so a
//!   file without them means what it always meant. Any signer workflow but kbf's own
//!   (`komira-ai/kbf`'s `artifacts.yml`) is parsed and refused when the channel is used,
//!   unless the top-level `trusted_workflows` of the same root-owned file names that exact
//!   workflow ([`ChannelError::UntrustedSigner`]); it is empty by default. That check is of
//!   the channel's own pair. The attestation itself is also accepted, with no entry, from
//!   the built-in pairs: kbf's own and its former name's (`artifacts.yml` of
//!   `komira-ai/komira-build-farm`) and, for the `tierstore` prefix of a channel that names
//!   kbf's, its former name's or `komira-ai/tierstore`'s repository, `komira-ai/tierstore`'s
//!   `artifacts.yml`. Any other signer needs a `trusted_workflows` entry.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use super::release::{ARTIFACT_PREFIX, Origin, REPO, WORKFLOW, is_signer_workflow};
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
    /// The repository, signer workflow and file names of the channel's releases.
    pub origin: Origin,
}

/// Why a channel cannot be used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChannelError {
    /// A refusal of the allowlist or of the channel's files.
    Refused(Refusal),
    /// The channel names a signer workflow other than kbf's own that `trusted_workflows`
    /// does not list.
    UntrustedSigner { channel: String, workflow: String },
}

impl ChannelError {
    /// The closed refusal this comes to.
    pub fn refusal(&self) -> Refusal {
        match self {
            Self::Refused(refusal) => *refusal,
            Self::UntrustedSigner { .. } => Refusal::ChannelNotAllowed,
        }
    }
}

impl From<Refusal> for ChannelError {
    fn from(refusal: Refusal) -> Self {
        Self::Refused(refusal)
    }
}

impl fmt::Display for ChannelError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Refused(refusal) => write!(f, "the allowlist: {}", refusal.code()),
            Self::UntrustedSigner { channel, workflow } => write!(
                f,
                "channel {channel} names signer workflow {workflow}, which trusted_workflows in the allowlist does not list"
            ),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    #[serde(default)]
    trusted_workflows: Vec<String>,
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
    repository: Option<String>,
    signer_workflow: Option<String>,
    artifact_prefix: Option<String>,
}

/// The parsed allowlist.
#[derive(Debug)]
pub struct MacAllowlist {
    trusted_workflows: Vec<String>,
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
                && origin_of(entry).is_some()
        });
        if !well_formed || !file.trusted_workflows.iter().all(|w| is_signer_workflow(w)) {
            return Err(Refusal::ConfigInvalid);
        }
        Ok(Self {
            trusted_workflows: file.trusted_workflows,
            channels: file.channels,
        })
    }

    /// The channel `name`, once its pinned sums (if it has any) are known to be root's.
    ///
    /// # Errors
    /// [`ChannelError::Refused`] with [`Refusal::ChannelNotAllowed`], [`Refusal::ConfigUnsafe`]
    /// or [`Refusal::ConfigInvalid`]; [`ChannelError::UntrustedSigner`] for a signer workflow
    /// other than kbf's own that `trusted_workflows` does not name.
    pub fn channel(&self, trust: &Trust, name: &Channel) -> Result<MacChannel, ChannelError> {
        let entry = self
            .channels
            .get(name.as_str())
            .ok_or(Refusal::ChannelNotAllowed)?;
        let origin = origin_of(entry).ok_or(Refusal::ConfigInvalid)?;
        if !origin.is_kbf_signer()
            && !self
                .trusted_workflows
                .iter()
                .any(|w| w == origin.workflow())
        {
            return Err(ChannelError::UntrustedSigner {
                channel: name.as_str().to_owned(),
                workflow: origin.workflow().to_owned(),
            });
        }
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
            origin,
        })
    }
}

/// The origin an entry names: kbf's when it names none. The repository and the workflow
/// come together; a prefix goes with a GitHub channel that has no sums.
fn origin_of(entry: &Entry) -> Option<Origin> {
    let prefix = entry.artifact_prefix.as_deref();
    let named_files = prefix.is_some() && (entry.dir.is_some() || entry.sums.is_some());
    let prefix = prefix.unwrap_or(ARTIFACT_PREFIX);
    match (&entry.repository, &entry.signer_workflow) {
        _ if named_files => None,
        (None, None) => Origin::new(REPO, WORKFLOW, prefix).ok(),
        (Some(repository), Some(workflow)) => Origin::new(repository, workflow, prefix).ok(),
        _ => None,
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
