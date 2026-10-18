//! Pulling one component of a release onto the host (`docs/design/pull-updates.md`, section
//! 7.1 to 7.3): the root installer downloads in an unprivileged child, re-verifies everything
//! the child hands it, and only then puts the release where a service can run it.
//!
//! The caller (the order watcher) names the component with a [`ReleaseLocator`] and gets an
//! [`InstalledRelease`] or a [`FetchError`] from a [`Fetcher`]; [`RootFetcher`] is the real
//! one. What the watcher passes in is **a locator and claims, not trust**: every field that
//! matters is checked here against the bytes.
//!
//! - [`config`]: the `[update]` table of `kbf.toml`, the one place a source is named.
//! - [`child`]: the `fetch-child` verb, the only code of this module that touches a network.
//! - [`pipe`]: the parent's end: start the child confined, read its bytes with `poll`,
//!   count and hash them as they arrive, end it at a cap.
//! - [`root`]: the order of things: free space, download, digest, attestation, shape, platform,
//!   the binary's own `--version --json` against the order's expectations, a `VERIFIED`
//!   marker, then one atomic rename into `releases/<commit>/`.
//! - [`sweep`]: what a crash leaves (`work/`, directories nobody verified).
//!
//! Where the release source is `frontend`, this build has no transport yet (the pinned CA, the
//! node's identity and the endpoint come from enrollment's configuration, which the installer
//! does not read today): a fetch from it is refused with [`FetchError::FrontendUnsupported`],
//! never silently turned into another source.

pub mod child;
pub mod config;
pub mod pipe;
pub mod root;
pub mod sweep;

use std::io;
use std::path::PathBuf;

use crate::CommitId;
use crate::refusal::Refusal;
use pipe::PipeFault;

/// The root-owned tree the installer keeps: `releases/`, `work/`, its state and its status.
pub const INSTALL_ROOT: &str = "/var/lib/kbf-installer";
/// The file that marks a release directory the installer verified and installed.
pub const MARKER: &str = "VERIFIED";
/// The most an attestation bundle may be.
pub const MAX_ATTESTATION_BYTES: usize = 256 << 10;

/// One component of a release to pull, as an activation order carries it. Claims, not trust.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReleaseLocator {
    pub commit: CommitId,
    /// `kbf` or `kbf-daemon`: the file the tarball holds.
    pub name: String,
    /// The tarball's sha256 (64 lowercase hex digits), also its name at the source.
    pub sha256: String,
    /// The tarball's length in bytes.
    pub size: u64,
    /// The build-provenance bundle for the tarball (JSON lines, at most 256 KiB).
    pub attestation: Vec<u8>,
    /// The expected facts about the release, as the frontend's preflight recorded them: the
    /// binary must say the same before anything is installed.
    pub platform: String,
    pub protocol: u64,
    pub min_protocol: u64,
    pub state_schema: u64,
    pub profile: String,
}

/// What the binary says about itself, as the installer read it (for `status.json`).
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ReleaseFacts {
    pub platform: String,
    pub protocol: u64,
    pub min_protocol: u64,
    pub state_schema: u64,
    pub profile: String,
    pub version: String,
}

/// A release that is verified and in place under `releases/<commit>/`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InstalledRelease {
    pub commit: CommitId,
    pub name: String,
    /// `releases/<commit>` on this machine.
    pub dir: PathBuf,
    pub facts: ReleaseFacts,
    /// Whether this call downloaded it. `false`: it was installed and verified before, and no
    /// byte was fetched.
    pub fetched: bool,
}

/// Something that makes a verified release available. The order watcher takes one, so that it
/// can be run with a fake.
pub trait Fetcher {
    /// The release `rec` names, verified and installed; nothing is changed that a service
    /// reads before it is.
    fn fetch(&self, rec: &ReleaseLocator) -> Result<InstalledRelease, FetchError>;
}

/// Why a fetch did not end in an installed release. Nothing but scratch has changed.
#[derive(Debug, thiserror::Error)]
pub enum FetchError {
    #[error("fetch: the order's component is refused: {0}")]
    Locator(String),
    #[error("fetch: the frontend as a release source is not available in this build")]
    FrontendUnsupported,
    #[error("fetch: no user {0:?} on this host to run the download as")]
    NoFetchUser(String),
    #[error("fetch: {need} bytes are needed under {path} and {free} are free")]
    NoRoom { need: u64, free: u64, path: PathBuf },
    #[error("fetch: {0}")]
    Pipe(#[from] PipeFault),
    #[error("fetch: the download has sha256 {actual}; the order says {want}")]
    Digest { actual: String, want: String },
    #[error("{0}")]
    Refused(#[from] Refusal),
    #[error("the binary says {what} is {got}; the order expects {want}")]
    Expected {
        what: &'static str,
        want: String,
        got: String,
    },
    #[error("protocol {got} is below the operator floor {floor}")]
    Floor { got: u64, floor: u64 },
    #[error("{path} holds another binary than the one verified: a release is never replaced")]
    Differs { path: PathBuf },
    #[error("cannot use {}: {source}", path.display())]
    Io { path: PathBuf, source: io::Error },
}

impl FetchError {
    /// A stable code for the status file and for a test to match.
    pub fn code(&self) -> &'static str {
        match self {
            FetchError::Locator(_) => "fetch-locator",
            FetchError::FrontendUnsupported => "fetch-frontend-unsupported",
            FetchError::NoFetchUser(_) => "fetch-no-user",
            FetchError::NoRoom { .. } => "fetch-no-room",
            FetchError::Pipe(PipeFault::Spawn(_)) => "fetch-spawn",
            FetchError::Pipe(PipeFault::Oversize { .. }) => "fetch-oversize",
            FetchError::Pipe(PipeFault::Short { .. }) => "fetch-short-read",
            FetchError::Pipe(PipeFault::Stalled { .. }) => "fetch-stalled",
            FetchError::Pipe(PipeFault::TimedOut { .. }) => "fetch-timeout",
            FetchError::Pipe(_) => "fetch-io",
            FetchError::Digest { .. } => "fetch-digest-mismatch",
            FetchError::Refused(r) => r.code(),
            FetchError::Expected { .. } => "release-unexpected",
            FetchError::Floor { .. } => "release-below-floor",
            FetchError::Differs { .. } => "release-differs",
            FetchError::Io { .. } => "fetch-io",
        }
    }
}
