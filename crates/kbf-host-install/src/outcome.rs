//! What an invocation did, as the one JSON line on stdout and the exit status.
//!
//! Exit 0: done (installed, already installed, or a query answered). Exit 2: refused, and
//! nothing ran. Exit 3: failed after a step started (or the audit log could not be
//! written first). A reason is a code from a closed set, never text taken from the
//! request or from a child process; the detail of a failure goes to stderr, cleaned.

use serde::Serialize;

/// Why a request was refused. Nothing ran.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// Not one of the two forms.
    BadCommand,
    /// A channel the allowlist does not name.
    ChannelNotAllowed,
    /// The allowlist is missing or does not parse.
    ConfigInvalid,
    /// A file read as root is not root's alone to write.
    ConfigUnsafe,
    /// The channel's source names no `signed-by` key.
    ChannelUnsigned,
    /// The key the source names is not a root-owned file.
    KeyUntrusted,
    /// The target is older than the installed version.
    OlderThanInstalled,
}

impl Refusal {
    /// The closed code.
    pub fn code(self) -> &'static str {
        todo!()
    }
}

/// Why a started request failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureCode {
    /// The audit log could not be opened, so nothing was run.
    AuditUnavailable,
    /// The installed version could not be read.
    QueryFailed,
    /// The installed version is not a Debian version.
    InstalledVersionInvalid,
    /// The package is in a state that is neither installed nor absent.
    PackageStateUnclean,
    /// `apt-get update` failed.
    UpdateFailed,
    /// `apt-get install` failed.
    InstallFailed,
    /// The install ended well but the installed version is not the target.
    VerifyFailed,
}

impl FailureCode {
    /// The closed code.
    pub fn code(self) -> &'static str {
        todo!()
    }
}

/// A failure: the code, and detail for stderr.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure {
    pub code: FailureCode,
    pub detail: String,
}

/// The answer of `query`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct QueryReport {
    pub installed_version: Option<String>,
    pub running_commit: Option<String>,
    pub in_progress: bool,
}

/// What an invocation did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The install ran and the target is installed.
    Installed { version: String },
    /// The target was already installed; nothing ran.
    AlreadyInstalled { version: String },
    /// `query`.
    Queried(QueryReport),
    /// Refused.
    Refused(Refusal),
    /// Failed.
    Failed(Failure),
}

impl Outcome {
    /// The exit status: 0, 2 or 3.
    pub fn exit_code(&self) -> u8 {
        todo!()
    }

    /// The one JSON line for stdout, without its line end.
    pub fn json_line(&self) -> String {
        todo!()
    }

    /// The outcome and reason of the audit line.
    pub fn audit_fields(&self) -> (&'static str, Option<&'static str>) {
        todo!()
    }

    /// The text for stderr: the detail of a failure, cleaned, else nothing.
    pub fn stderr_detail(&self) -> Option<String> {
        todo!()
    }
}

#[cfg(test)]
mod tests;
