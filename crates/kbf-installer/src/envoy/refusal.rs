//! Why an `envoy` verb did not do what it was asked: one enum, a stable code for each case
//! and the exit code of docs/design/kbf-installer.md, section 2 it ends the verb with.

use std::io;
use std::path::PathBuf;

use super::fetch::FetchError;
use super::manifest::ManifestFault;
use crate::binary::BinaryKind;
use crate::layout::Platform;
use crate::refusal::Refusal;
use crate::state::StateError;

/// What `envoy --version` said that is not what the pin says.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum VersionFault {
    #[error("it exited with {}", code.map_or_else(|| "a signal".to_owned(), |c| c.to_string()))]
    Failed { code: Option<i32> },
    #[error("it printed no `version: <commit>/<version>/...` line")]
    Unreadable,
    #[error("it says {got}, the pin says {want}")]
    Mismatch { want: String, got: String },
}

#[derive(Debug, thiserror::Error)]
pub enum EnvoyError {
    /// Envoy is pinned on Linux only.
    #[error("Envoy is pinned on Linux hosts only, not {}", .0.name())]
    Platform(Platform),
    #[error("the manifest is refused: {0}")]
    Manifest(#[from] ManifestFault),
    /// The manifest file could not be read, or is not root's alone.
    /// The verb needs root.
    #[error("this verb needs root")]
    NotRoot,
    #[error("cannot use the manifest {}: {reason}", path.display())]
    ManifestFile { path: PathBuf, reason: String },
    #[error("cannot fetch Envoy: {0}")]
    Fetch(#[from] FetchError),
    /// The download is not a file the fetch user left, or is not a plain file within the cap.
    #[error("the download is refused: {0}")]
    Received(Box<Refusal>),
    /// The download is not a file the fetch user owns.
    #[error("the download is owned by uid {found}, not the fetch user (uid {want})")]
    DownloadOwner { found: u32, want: u32 },
    #[error("the download hashes to {got}, the manifest says {want}")]
    Hash { want: String, got: String },
    #[error("the binary is {found}, and this host runs {want}")]
    WrongBinary { found: BinaryKind, want: BinaryKind },
    #[error("`envoy --version` is refused: {0}")]
    Version(VersionFault),
    /// `envoy/<id>` is there with other bytes: it is never replaced.
    #[error("{} holds other bytes than the pin: a build is never replaced", path.display())]
    Differs { path: PathBuf },
    /// What is installed is not what `verify` accepts (missing, not root's, writable, the
    /// recorded sha256 unreadable or not the file's).
    #[error("{}: {reason}", path.display())]
    Installed { path: PathBuf, reason: String },
    /// `state.json` pins no Envoy.
    #[error("state.json pins no Envoy; run `envoy fetch` first")]
    Unpinned,
    #[error("{0}")]
    State(#[from] StateError),
    #[error("cannot run {what}: {source}")]
    Tool {
        what: &'static str,
        source: io::Error,
    },
    #[error("{what} ran out of time and was killed")]
    TimedOut { what: &'static str },
    #[error("cannot use {}: {source}", path.display())]
    Work { path: PathBuf, source: io::Error },
}

impl EnvoyError {
    /// The stable, kebab-case code of this case.
    pub fn code(&self) -> &'static str {
        match self {
            EnvoyError::Platform(_) => "envoy-platform",
            EnvoyError::Manifest(_) => "envoy-manifest",
            EnvoyError::ManifestFile { .. } => "envoy-manifest-file",
            EnvoyError::NotRoot => "not-root",
            EnvoyError::Fetch(_) => "envoy-fetch",
            EnvoyError::Received(r) => r.code(),
            EnvoyError::Hash { .. } => "envoy-hash",
            EnvoyError::DownloadOwner { .. } => "envoy-download-owner",
            EnvoyError::WrongBinary { .. } => "envoy-wrong-platform",
            EnvoyError::Version(_) => "envoy-version",
            EnvoyError::Differs { .. } => "envoy-differs",
            EnvoyError::Installed { .. } => "envoy-installed",
            EnvoyError::Unpinned => "envoy-unpinned",
            EnvoyError::State(_) => "envoy-state",
            EnvoyError::Tool { .. } => "tool-failed",
            EnvoyError::TimedOut { .. } => "timed-out",
            EnvoyError::Work { .. } => "work-dir",
        }
    }

    /// The exit code: 1 failed, 2 usage or a missing prerequisite, 3 refused (verification
    /// or policy), 5 needs the operator (no pin, an unreadable state).
    pub fn exit(&self) -> u8 {
        match self {
            EnvoyError::Fetch(_)
            | EnvoyError::Tool { .. }
            | EnvoyError::TimedOut { .. }
            | EnvoyError::Work { .. } => 1,
            EnvoyError::Platform(_) | EnvoyError::ManifestFile { .. } | EnvoyError::NotRoot => 2,
            EnvoyError::Unpinned | EnvoyError::State(_) => 5,
            EnvoyError::Manifest(_)
            | EnvoyError::Received(_)
            | EnvoyError::Hash { .. }
            | EnvoyError::DownloadOwner { .. }
            | EnvoyError::WrongBinary { .. }
            | EnvoyError::Version(_)
            | EnvoyError::Differs { .. }
            | EnvoyError::Installed { .. } => 3,
        }
    }
}
