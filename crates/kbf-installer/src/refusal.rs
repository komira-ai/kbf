//! Why a stage was refused: one enum, a stable machine-readable [`Refusal::code`] for
//! each case and a one-line human message (its `Display`).
//!
//! The code is what a caller reports upward and a test matches; the message is for an
//! operator. A code never changes meaning and a case never shares one. Nothing here is a
//! message in a string: a case is a variant, and the strings some carry (a file name, a
//! tool's first line of complaint) are detail.

use std::io;
use std::path::PathBuf;

use crate::binary::BinaryKind;
use crate::release::ReleaseError;

/// A stage was refused or could not be done. Nothing it refused was installed.
#[derive(Debug, thiserror::Error)]
pub enum Refusal {
    /// `gh`, the trusted root or the settings file, or a directory above one, is not
    /// what the host's bootstrap leaves.
    #[error("{} is not trusted: {fault}", path.display())]
    Untrusted { path: PathBuf, fault: TrustFault },
    #[error("settings {}: {fault}", path.display())]
    Settings { path: PathBuf, fault: SettingsFault },
    #[error("the offer is refused: {0}")]
    Offer(OfferFault),
    #[error("inbox file {name:?} is refused: {fault}")]
    Inbox { name: String, fault: InboxFault },
    /// The copy's sha256 is not the one the offer names for the file.
    #[error("{name:?} does not hash to the sha256 the offer names for it")]
    HashMismatch { name: String },
    /// `gh attestation verify` did not accept the file for the commit and a trusted ref.
    #[error("{subject:?} is not attested by the pinned workflow for this commit: {detail}")]
    NotAttested { subject: String, detail: String },
    #[error("{component}: its archive is refused: {fault}")]
    Archive {
        component: String,
        fault: ArchiveFault,
    },
    /// An attested file of another kind of binary than this host runs.
    #[error("{component} is {found}, and this host runs {want}")]
    WrongPlatform {
        component: String,
        found: BinaryKind,
        want: BinaryKind,
    },
    #[error("inbox file {name:?} carries the quarantine attribute")]
    Quarantined { name: String },
    #[error("{component}: its code signature is refused: {fault}")]
    BadSignature {
        component: String,
        fault: SignatureFault,
    },
    #[error("{component}: its version check is refused: {fault}")]
    Version {
        component: String,
        fault: VersionFault,
    },
    /// The commit's release directory is there, with other bytes: it is never replaced.
    #[error("{path} differs from the release being staged: a release is never replaced")]
    Differs { path: String },
    /// Copying the release into its directory, or recording it, failed.
    #[error("cannot install the release: {0}")]
    Install(#[source] ReleaseError),
    /// A tool could not be started.
    #[error("cannot run {what}: {source}")]
    Tool {
        what: &'static str,
        source: io::Error,
    },
    #[error("{what} ran out of time and was killed")]
    TimedOut { what: &'static str },
    /// The scratch directory, or a file in it, could not be made or written.
    #[error("cannot use {}: {source}", path.display())]
    Work { path: PathBuf, source: io::Error },
}

impl Refusal {
    /// The stable, kebab-case code of this case.
    pub fn code(&self) -> &'static str {
        match self {
            Refusal::Untrusted { fault, .. } => fault.code(),
            Refusal::Settings { fault, .. } => fault.code(),
            Refusal::Offer(fault) => fault.code(),
            Refusal::Inbox { fault, .. } => fault.code(),
            Refusal::HashMismatch { .. } => "hash-mismatch",
            Refusal::NotAttested { .. } => "not-attested",
            Refusal::Archive { fault, .. } => fault.code(),
            Refusal::WrongPlatform { .. } => "wrong-platform",
            Refusal::Quarantined { .. } => "quarantined",
            Refusal::BadSignature { fault, .. } => fault.code(),
            Refusal::Version { fault, .. } => fault.code(),
            Refusal::Differs { .. } => "release-differs",
            Refusal::Install(_) => "install-failed",
            Refusal::Tool { .. } => "tool-failed",
            Refusal::TimedOut { .. } => "timed-out",
            Refusal::Work { .. } => "work-dir",
        }
    }
}

impl From<ReleaseError> for Refusal {
    fn from(e: ReleaseError) -> Refusal {
        match e {
            ReleaseError::Differs { path } => Refusal::Differs {
                path: path.to_string(),
            },
            other => Refusal::Install(other),
        }
    }
}

/// Why a path on the trust list (`gh`, the trusted root, the settings, the directories
/// above them) is not trusted.
#[derive(Debug, thiserror::Error)]
pub enum TrustFault {
    #[error("it is missing")]
    Missing,
    #[error("it is a symbolic link, or not a plain file or directory")]
    Link,
    #[error("it is a {found}, not a {want}")]
    Kind {
        found: &'static str,
        want: &'static str,
    },
    #[error("it is owned by uid {uid}, not root")]
    NotRoot { uid: u32 },
    #[error("it is writable by group or others (mode {mode:04o})")]
    Writable { mode: u32 },
    #[error("it cannot be read: {0}")]
    Unreadable(io::Error),
}

impl TrustFault {
    pub fn code(&self) -> &'static str {
        match self {
            TrustFault::Missing => "untrusted-missing",
            TrustFault::Link => "untrusted-link",
            TrustFault::Kind { .. } => "untrusted-kind",
            TrustFault::NotRoot { .. } => "untrusted-owner",
            TrustFault::Writable { .. } => "untrusted-writable",
            TrustFault::Unreadable(_) => "untrusted-unreadable",
        }
    }
}

/// Why the host's settings file is refused.
#[derive(Debug, thiserror::Error)]
pub enum SettingsFault {
    #[error("line {line} is not KEY=VALUE")]
    Syntax { line: usize },
    #[error("line {line}: {key:?} is not a setting")]
    UnknownKey { line: usize, key: String },
    #[error("line {line}: {key} is named twice")]
    Repeated { line: usize, key: &'static str },
    #[error("line {line}: {key} is not a number")]
    NotANumber { line: usize, key: &'static str },
    #[error("line {line}: {value:?} is not a branch ref (refs/heads/...)")]
    BadRef { line: usize, value: String },
    #[error("SOURCE_REFS names no ref")]
    NoRefs,
    #[error("the unprivileged user or group is root (0)")]
    RunAsRoot,
    #[error("it is not UTF-8")]
    NotUtf8,
}

impl SettingsFault {
    pub fn code(&self) -> &'static str {
        match self {
            SettingsFault::Syntax { .. } => "settings-syntax",
            SettingsFault::UnknownKey { .. } => "settings-unknown-key",
            SettingsFault::Repeated { .. } => "settings-repeated",
            SettingsFault::NotANumber { .. } => "settings-number",
            SettingsFault::BadRef { .. } => "settings-ref",
            SettingsFault::NoRefs => "settings-no-refs",
            SettingsFault::RunAsRoot => "settings-run-as-root",
            SettingsFault::NotUtf8 => "settings-utf8",
        }
    }
}

/// Why an offer is refused.
#[derive(Debug, thiserror::Error)]
pub enum OfferFault {
    #[error("it is not the JSON an offer is: {0}")]
    NotAnOffer(String),
    #[error("its schema is {found}; this build reads only schema {want}")]
    Schema { found: u64, want: u64 },
    #[error("it names no component")]
    NoComponents,
    #[error("{0:?} is not a component of a release")]
    UnknownComponent(String),
    #[error("it names the component {0} twice")]
    RepeatedComponent(String),
    #[error("{0:?} is not a plain file name")]
    BadFileName(String),
    #[error("it names the file {0:?} twice")]
    RepeatedFile(String),
    #[error("{0:?} is not a sha256 (64 lowercase hex digits)")]
    BadSha256(String),
    #[error("{0:?} is not a commit (40 lowercase hex digits)")]
    BadCommit(String),
}

impl OfferFault {
    pub fn code(&self) -> &'static str {
        match self {
            OfferFault::NotAnOffer(_) => "offer-malformed",
            OfferFault::Schema { .. } => "offer-schema",
            OfferFault::NoComponents => "offer-no-components",
            OfferFault::UnknownComponent(_) => "offer-component",
            OfferFault::RepeatedComponent(_) => "offer-repeated-component",
            OfferFault::BadFileName(_) => "offer-file-name",
            OfferFault::RepeatedFile(_) => "offer-repeated-file",
            OfferFault::BadSha256(_) => "offer-sha256",
            OfferFault::BadCommit(_) => "offer-commit",
        }
    }
}

/// Why a file in the inbox entry is refused.
#[derive(Debug, thiserror::Error)]
pub enum InboxFault {
    #[error("it is missing")]
    Missing,
    #[error("it is a symbolic link")]
    Symlink,
    #[error("it is not a regular file")]
    NotRegular,
    #[error("it is not a directory")]
    NotADirectory,
    #[error("it is not a plain file name (one path component)")]
    BadName,
    #[error("it has more than one hard link")]
    HardLinked,
    #[error("it is larger than {cap} bytes")]
    Oversize { cap: u64 },
    #[error("it is empty")]
    Empty,
    #[error("the offer does not name it")]
    Unexpected,
    #[error("the entry holds more than {max} files")]
    TooMany { max: usize },
    #[error("it cannot be read: {0}")]
    Unreadable(io::Error),
}

impl InboxFault {
    pub fn code(&self) -> &'static str {
        match self {
            InboxFault::Missing => "inbox-missing",
            InboxFault::Symlink => "inbox-symlink",
            InboxFault::NotRegular => "inbox-not-regular",
            InboxFault::NotADirectory => "inbox-not-directory",
            InboxFault::BadName => "inbox-bad-name",
            InboxFault::HardLinked => "inbox-hard-linked",
            InboxFault::Oversize { .. } => "inbox-oversize",
            InboxFault::Empty => "inbox-empty",
            InboxFault::Unexpected => "inbox-unexpected",
            InboxFault::TooMany { .. } => "inbox-too-many",
            InboxFault::Unreadable(_) => "inbox-unreadable",
        }
    }
}

/// Why a component's tarball is refused.
#[derive(Debug, thiserror::Error)]
pub enum ArchiveFault {
    #[error("it is not a gzip-compressed tar: {0}")]
    NotTarGz(String),
    #[error("it holds {0} entries, not exactly one")]
    Entries(usize),
    #[error("its entry is not named {0:?}, alone and with no path")]
    WrongName(String),
    #[error("its entry is not a regular file")]
    NotRegular,
    #[error("its entry is larger than {cap} bytes")]
    Oversize { cap: u64 },
}

impl ArchiveFault {
    pub fn code(&self) -> &'static str {
        match self {
            ArchiveFault::NotTarGz(_) => "archive-malformed",
            ArchiveFault::Entries(_) => "archive-entries",
            ArchiveFault::WrongName(_) => "archive-name",
            ArchiveFault::NotRegular => "archive-not-regular",
            ArchiveFault::Oversize { .. } => "archive-oversize",
        }
    }
}

/// Why a macOS binary's code signature is refused.
#[derive(Debug, thiserror::Error)]
pub enum SignatureFault {
    #[error("`codesign --verify --strict` does not accept it")]
    NotValid,
    #[error("its code directory flags are {found:?}, not \"adhoc,runtime\"")]
    Flags { found: String },
}

impl SignatureFault {
    pub fn code(&self) -> &'static str {
        match self {
            SignatureFault::NotValid => "signature-invalid",
            SignatureFault::Flags { .. } => "signature-flags",
        }
    }
}

/// Why a staged binary's `--version --json` is refused.
#[derive(Debug, thiserror::Error)]
pub enum VersionFault {
    #[error("it did not exit 0 (status {0:?})")]
    Failed(Option<i32>),
    #[error("it printed no JSON object of the fields a build reports: {0}")]
    Unreadable(String),
    #[error("it says commit {got}, and the release is {want}")]
    Commit { want: String, got: String },
    #[error("it says profile {0:?}, not \"release\"")]
    Profile(String),
}

impl VersionFault {
    pub fn code(&self) -> &'static str {
        match self {
            VersionFault::Failed(_) => "version-failed",
            VersionFault::Unreadable(_) => "version-unreadable",
            VersionFault::Commit { .. } => "version-commit",
            VersionFault::Profile(_) => "version-profile",
        }
    }
}

#[cfg(test)]
#[path = "refusal_tests.rs"]
mod tests;
