//! What an invocation did, as the one JSON line on stdout and the exit status.
//!
//! Every line has a `result` (`installed`, `already_installed`, `query`, `refused` or
//! `failed`).
//!
//! Exit 0: done (installed, already installed, or a query answered). Exit 2: refused, and
//! nothing ran. Exit 3: failed after a step started (or the audit log could not be
//! written first). A reason is a code from a closed set, never text taken from the
//! request or from a child process; the detail of a failure goes to stderr, cleaned.

use serde::{Deserialize, Serialize};

/// The most of a failure's detail that is written to stderr, in bytes.
pub const MAX_DETAIL_LEN: usize = 2048;

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
    /// The source switches signature checking off or weakens it (`trusted`,
    /// `allow-insecure`, `allow-weak`, `allow-downgrade-to-insecure`), even if it names a key.
    ChannelInsecure,
}

impl Refusal {
    /// Every refusal.
    pub const ALL: [Self; 7] = [
        Self::BadCommand,
        Self::ChannelNotAllowed,
        Self::ConfigInvalid,
        Self::ConfigUnsafe,
        Self::ChannelUnsigned,
        Self::KeyUntrusted,
        Self::ChannelInsecure,
    ];

    /// The refusal whose closed code this is.
    pub fn from_code(code: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|refusal| refusal.code() == code)
    }

    /// The closed code.
    pub fn code(self) -> &'static str {
        match self {
            Self::BadCommand => "bad_command",
            Self::ChannelNotAllowed => "channel_not_allowed",
            Self::ConfigInvalid => "config_invalid",
            Self::ConfigUnsafe => "config_unsafe",
            Self::ChannelUnsigned => "channel_unsigned",
            Self::KeyUntrusted => "key_untrusted",
            Self::ChannelInsecure => "channel_insecure",
        }
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
    /// Another install holds the install lock.
    Busy,
    /// The install lock file could not be opened.
    LockUnavailable,
}

impl FailureCode {
    /// Every failure.
    pub const ALL: [Self; 9] = [
        Self::AuditUnavailable,
        Self::QueryFailed,
        Self::InstalledVersionInvalid,
        Self::PackageStateUnclean,
        Self::UpdateFailed,
        Self::InstallFailed,
        Self::VerifyFailed,
        Self::Busy,
        Self::LockUnavailable,
    ];

    /// The failure whose closed code this is.
    pub fn from_code(code: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|failure| failure.code() == code)
    }

    /// The closed code.
    pub fn code(self) -> &'static str {
        match self {
            Self::AuditUnavailable => "audit_unavailable",
            Self::QueryFailed => "query_failed",
            Self::InstalledVersionInvalid => "installed_version_invalid",
            Self::PackageStateUnclean => "package_state_unclean",
            Self::UpdateFailed => "update_failed",
            Self::InstallFailed => "install_failed",
            Self::VerifyFailed => "verify_failed",
            Self::Busy => "busy",
            Self::LockUnavailable => "lock_unavailable",
        }
    }
}

/// A failure: the code, and detail for stderr.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure {
    pub code: FailureCode,
    pub detail: String,
}

/// What `query` found. Its line is `result: "query"` and then these four fields.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueryReport {
    pub installed_version: Option<String>,
    pub running_commit: Option<String>,
    /// The node's id from its configuration; none when the host has none to report.
    pub node_id: Option<String>,
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
        match self {
            Self::Installed { .. } | Self::AlreadyInstalled { .. } | Self::Queried(_) => 0,
            Self::Refused(_) => 2,
            Self::Failed(_) => 3,
        }
    }

    /// The one JSON line for stdout, without its line end.
    pub fn json_line(&self) -> String {
        match self {
            Self::Installed { version } => to_json(&Done {
                result: "installed",
                installed_version: version,
                unit_state: "finished",
            }),
            Self::AlreadyInstalled { version } => to_json(&Done {
                result: "already_installed",
                installed_version: version,
                unit_state: "not_run",
            }),
            Self::Queried(report) => to_json(&Queried {
                result: "query",
                installed_version: report.installed_version.as_deref(),
                running_commit: report.running_commit.as_deref(),
                node_id: report.node_id.as_deref(),
                in_progress: report.in_progress,
            }),
            Self::Refused(refusal) => to_json(&Reason {
                result: "refused",
                reason: refusal.code(),
            }),
            Self::Failed(failure) => to_json(&Reason {
                result: "failed",
                reason: failure.code.code(),
            }),
        }
    }

    /// The outcome an install unit printed, read back by the process that started it: none
    /// for anything but the closed lines of an install (`query` is never a unit's answer).
    /// A failure's detail is not on the line and is empty here.
    pub fn from_unit_line(line: &str) -> Option<Self> {
        let line: UnitLine = serde_json::from_str(line).ok()?;
        let version = line.installed_version;
        match (
            line.result.as_str(),
            line.reason.as_deref(),
            line.unit_state.as_deref(),
        ) {
            ("installed", None, Some("finished")) => Some(Self::Installed { version: version? }),
            ("already_installed", None, Some("not_run")) => {
                Some(Self::AlreadyInstalled { version: version? })
            }
            ("refused", Some(code), None) if version.is_none() => {
                Refusal::from_code(code).map(Self::Refused)
            }
            ("failed", Some(code), None) if version.is_none() => {
                FailureCode::from_code(code).map(|code| {
                    Self::Failed(Failure {
                        code,
                        detail: String::new(),
                    })
                })
            }
            _ => None,
        }
    }

    /// The outcome and reason of the audit line.
    pub fn audit_fields(&self) -> (&'static str, Option<&'static str>) {
        match self {
            Self::Installed { .. } => ("installed", None),
            Self::AlreadyInstalled { .. } => ("already_installed", None),
            Self::Queried(_) => ("query", None),
            Self::Refused(refusal) => ("refused", Some(refusal.code())),
            Self::Failed(failure) => ("failed", Some(failure.code.code())),
        }
    }

    /// The text for stderr: the detail of a failure, cleaned, else nothing.
    pub fn stderr_detail(&self) -> Option<String> {
        let Self::Failed(failure) = self else {
            return None;
        };
        let mut text = String::new();
        for c in failure.detail.chars() {
            let c = if c.is_control() && c != '\n' && c != '\t' {
                '?'
            } else {
                c
            };
            if text.len() + c.len_utf8() > MAX_DETAIL_LEN {
                break;
            }
            text.push(c);
        }
        Some(text)
    }
}

#[derive(Serialize)]
struct Queried<'a> {
    result: &'static str,
    installed_version: Option<&'a str>,
    running_commit: Option<&'a str>,
    node_id: Option<&'a str>,
    in_progress: bool,
}

/// A line an install unit printed, as `Outcome::from_unit_line` reads it.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UnitLine {
    result: String,
    reason: Option<String>,
    installed_version: Option<String>,
    unit_state: Option<String>,
}

#[derive(Serialize)]
struct Done<'a> {
    result: &'static str,
    installed_version: &'a str,
    unit_state: &'static str,
}

#[derive(Serialize)]
struct Reason {
    result: &'static str,
    reason: &'static str,
}

/// The JSON text of `value`. Serializing strings and booleans does not fail.
fn to_json<T: Serialize>(value: &T) -> String {
    serde_json::to_string(value).unwrap_or_default()
}

#[cfg(test)]
mod tests;
