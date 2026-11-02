//! What the local socket answers: one JSON line, every line with a `result`.
//!
//! | request | line |
//! |---|---|
//! | install | `{"result":"staged","version":V,"commit":C,"current":bool}` |
//! | switch | `{"result":"switched","version":V,"commit":C,"deadline_unix":N}` |
//! | confirm | `{"result":"confirmed","version":V}` |
//! | revert | `{"result":"reverted","version":V\|null}` |
//! | activate | `{"result":"activated","commit":C,"version":V\|null,"from":C\|null,"pinned":bool,"lock_taken":bool}` |
//! | query | `{"result":"query","commit":..,"version":..,"node_id":..,"pending":{..}\|null,"last":{..}\|null,"busy":bool}` |
//! | no | `{"result":"refused","reason":CODE}` (nothing ran) or `{"result":"failed","reason":CODE}` |
//! | no, for `activate` | `{"result":"refused","reason":"not_kept"\|"ambiguous_release","releases":[..]}`: the releases the host keeps (`not_kept` also for one whose binary cannot run) |
//!
//! A reason is a code from a closed set ([`Code`]), never text taken from the request or from
//! a child process; the detail goes to the log, cleaned.

use kbf_retain::{CommitId, Kept};
use serde::Serialize;

use crate::outcome::MAX_DETAIL_LEN;

/// Why a request was refused or failed. The closed set of the socket's reasons.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Code {
    /// Not one of the request forms.
    BadCommand,
    /// A channel the allowlist does not name.
    ChannelNotAllowed,
    /// The allowlist is missing or does not parse.
    ConfigInvalid,
    /// A file read as root is not root's alone to write.
    ConfigUnsafe,
    /// No release asset starts with the version's 12 hex digits.
    UnknownRelease,
    /// The assets of two commits start with them.
    AmbiguousRelease,
    /// The release could not be fetched.
    FetchFailed,
    /// The release is not what the farm's build workflow made for that commit, or is not the
    /// executable this host runs.
    VerifyFailed,
    /// A release is on trial already (another one, or the same one for an install).
    Pending,
    /// The version named is not the one laid out by an install.
    NotStaged,
    /// Nothing is on trial.
    NothingPending,
    /// The host has no release that runs now to go back to.
    NoCurrent,
    /// Another install, switch or restart holds the install lock.
    Busy,
    /// The release could not be laid out.
    StageFailed,
    /// The release could not be put in place; the host is back on the release that ran.
    SwitchFailed,
    /// The host could not be put back on the release that ran.
    RevertFailed,
    /// The host's records cannot be read or written.
    StateUnavailable,
    /// The audit log cannot be opened, so nothing was run.
    AuditUnavailable,
    /// Another frontend this host can see is not ready, so this one is not taken down.
    PeerNotReady,
    /// The container runtime could not be asked, or did not do what it was asked.
    RuntimeFailed,
    /// The release an `activate` names is not one the host keeps.
    NotKept,
    /// The links moved to the release an `activate` named and a unit did not come up on it;
    /// the host stays on that release.
    RestartFailed,
}

impl Code {
    /// Every code.
    pub const ALL: [Self; 22] = [
        Self::BadCommand,
        Self::ChannelNotAllowed,
        Self::ConfigInvalid,
        Self::ConfigUnsafe,
        Self::UnknownRelease,
        Self::AmbiguousRelease,
        Self::FetchFailed,
        Self::VerifyFailed,
        Self::Pending,
        Self::NotStaged,
        Self::NothingPending,
        Self::NoCurrent,
        Self::Busy,
        Self::StageFailed,
        Self::SwitchFailed,
        Self::RevertFailed,
        Self::StateUnavailable,
        Self::AuditUnavailable,
        Self::PeerNotReady,
        Self::RuntimeFailed,
        Self::NotKept,
        Self::RestartFailed,
    ];

    /// The code whose closed spelling this is.
    pub fn from_code(code: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|c| c.code() == code)
    }

    /// The closed spelling.
    pub fn code(self) -> &'static str {
        match self {
            Self::BadCommand => "bad_command",
            Self::ChannelNotAllowed => "channel_not_allowed",
            Self::ConfigInvalid => "config_invalid",
            Self::ConfigUnsafe => "config_unsafe",
            Self::UnknownRelease => "unknown_release",
            Self::AmbiguousRelease => "ambiguous_release",
            Self::FetchFailed => "fetch_failed",
            Self::VerifyFailed => "verify_failed",
            Self::Pending => "pending",
            Self::NotStaged => "not_staged",
            Self::NothingPending => "nothing_pending",
            Self::NoCurrent => "no_current",
            Self::Busy => "busy",
            Self::StageFailed => "stage_failed",
            Self::SwitchFailed => "switch_failed",
            Self::RevertFailed => "revert_failed",
            Self::StateUnavailable => "state_unavailable",
            Self::AuditUnavailable => "audit_unavailable",
            Self::PeerNotReady => "peer_not_ready",
            Self::RuntimeFailed => "runtime_failed",
            Self::NotKept => "not_kept",
            Self::RestartFailed => "restart_failed",
        }
    }
}

/// A release on trial, as `query` and `switch` tell it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PendingView {
    /// The version text the release was staged as, when this host knows it.
    pub version: Option<String>,
    /// The full commit.
    pub commit: String,
    /// When the host goes back by itself if nothing confirmed the release (Unix seconds);
    /// none when the swap on trial is not one this program arms.
    pub deadline_unix: Option<u64>,
}

/// How the last update this program ran ended.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LastView {
    /// The version it was told to run.
    pub target: String,
    /// `succeeded` (confirmed) or `failed` (the host is back on the release that ran).
    pub outcome: &'static str,
    /// Why it failed, in a word: `reverted`, `timeout`, `switch_failed`.
    pub reason: Option<&'static str>,
}

/// What `query` found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueryView {
    /// The full commit `current` names.
    pub commit: Option<String>,
    /// The version text that release was installed as, when this host knows it.
    pub version: Option<String>,
    pub node_id: Option<String>,
    pub pending: Option<PendingView>,
    pub last: Option<LastView>,
    /// An install, a switch or a restart holds the install lock.
    pub busy: bool,
    /// The releases the host holds, newest first, each with its commit and version: at most
    /// [`super::core::MAX_LISTED`], the ones that must stay first.
    pub releases: Vec<Kept>,
    /// How many releases the host holds past the ones listed; 0 when it lists them all.
    pub releases_omitted: usize,
    /// The releases could not be listed (the file that names what must stay, or `releases/`,
    /// cannot be read): `releases` is empty for that, not because the host holds none.
    pub releases_unavailable: bool,
}

/// What a request did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Answer {
    Staged {
        version: String,
        commit: String,
        /// The host already runs this release: nothing was fetched.
        current: bool,
    },
    Switched {
        version: String,
        commit: String,
        deadline_unix: u64,
    },
    Confirmed {
        version: String,
    },
    Reverted {
        /// The version of the release the host is back on, when this host knows it.
        version: Option<String>,
    },
    Queried(QueryView),
    /// The release an `activate` named runs now, for good.
    Activated {
        commit: CommitId,
        /// Its version text, when this host knows it.
        version: Option<String>,
        /// The release that ran before, now `previous` (none on a host that ran none, and the
        /// same commit when it was already the one that runs).
        from: Option<CommitId>,
        /// The host is held on it: the hold file is written and the release is pinned.
        pinned: bool,
        /// The install lock was taken from another holder (`--force-lock`).
        lock_taken: bool,
    },
    /// An `unpin` ended: the pin of `commit` is undone, or there was none (`None`: no hold
    /// file, or one that named no commit root wrote).
    Unpinned {
        commit: Option<CommitId>,
    },
    /// An `activate` named no release the host keeps (`not_kept`), or more than one
    /// (`ambiguous_release`): refused, nothing ran, and the releases the host keeps are told.
    Unresolved {
        code: Code,
        /// What was asked for and why it names no release to run, for the log.
        detail: String,
        releases: Vec<Kept>,
        releases_omitted: usize,
    },
    /// Refused, and nothing ran.
    Refused(Code, String),
    /// Failed after something started.
    Failed(Code, String),
}

#[derive(Serialize)]
struct Staged<'a> {
    result: &'static str,
    version: &'a str,
    commit: &'a str,
    current: bool,
}

#[derive(Serialize)]
struct Switched<'a> {
    result: &'static str,
    version: &'a str,
    commit: &'a str,
    deadline_unix: u64,
}

#[derive(Serialize)]
struct Confirmed<'a> {
    result: &'static str,
    version: &'a str,
}

#[derive(Serialize)]
struct Reverted<'a> {
    result: &'static str,
    version: Option<&'a str>,
}

#[derive(Serialize)]
struct Queried<'a> {
    result: &'static str,
    commit: Option<&'a str>,
    version: Option<&'a str>,
    node_id: Option<&'a str>,
    pending: Option<&'a PendingView>,
    last: Option<&'a LastView>,
    busy: bool,
    #[serde(skip_serializing_if = "<[Kept]>::is_empty")]
    releases: &'a [Kept],
    #[serde(skip_serializing_if = "Option::is_none")]
    releases_omitted: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    releases_unavailable: Option<bool>,
}

#[derive(Serialize)]
struct Activated<'a> {
    result: &'static str,
    commit: &'a CommitId,
    version: Option<&'a str>,
    from: Option<&'a CommitId>,
    pinned: bool,
    lock_taken: bool,
}

#[derive(Serialize)]
struct Unpinned<'a> {
    result: &'static str,
    commit: Option<&'a CommitId>,
}

#[derive(Serialize)]
struct Unresolved<'a> {
    result: &'static str,
    reason: &'static str,
    releases: &'a [Kept],
    #[serde(skip_serializing_if = "Option::is_none")]
    releases_omitted: Option<usize>,
}

#[derive(Serialize)]
struct Reason {
    result: &'static str,
    reason: &'static str,
}

impl Answer {
    /// The one JSON line, without its end.
    pub fn json_line(&self) -> String {
        match self {
            Self::Staged {
                version,
                commit,
                current,
            } => to_json(&Staged {
                result: "staged",
                version,
                commit,
                current: *current,
            }),
            Self::Switched {
                version,
                commit,
                deadline_unix,
            } => to_json(&Switched {
                result: "switched",
                version,
                commit,
                deadline_unix: *deadline_unix,
            }),
            Self::Confirmed { version } => to_json(&Confirmed {
                result: "confirmed",
                version,
            }),
            Self::Reverted { version } => to_json(&Reverted {
                result: "reverted",
                version: version.as_deref(),
            }),
            Self::Queried(view) => to_json(&Queried {
                result: "query",
                commit: view.commit.as_deref(),
                version: view.version.as_deref(),
                node_id: view.node_id.as_deref(),
                pending: view.pending.as_ref(),
                last: view.last.as_ref(),
                busy: view.busy,
                releases: &view.releases,
                releases_omitted: (view.releases_omitted > 0).then_some(view.releases_omitted),
                releases_unavailable: view.releases_unavailable.then_some(true),
            }),
            Self::Activated {
                commit,
                version,
                from,
                pinned,
                lock_taken,
            } => to_json(&Activated {
                result: "activated",
                commit,
                version: version.as_deref(),
                from: from.as_ref(),
                pinned: *pinned,
                lock_taken: *lock_taken,
            }),
            Self::Unpinned { commit } => to_json(&Unpinned {
                result: "unpinned",
                commit: commit.as_ref(),
            }),
            Self::Unresolved {
                code,
                releases,
                releases_omitted,
                ..
            } => to_json(&Unresolved {
                result: "refused",
                reason: code.code(),
                releases,
                releases_omitted: (*releases_omitted > 0).then_some(*releases_omitted),
            }),
            Self::Refused(code, _) => to_json(&Reason {
                result: "refused",
                reason: code.code(),
            }),
            Self::Failed(code, _) => to_json(&Reason {
                result: "failed",
                reason: code.code(),
            }),
        }
    }

    /// The code and word of the audit line: the outcome and the reason.
    pub fn audit_fields(&self) -> (&'static str, Option<&'static str>) {
        match self {
            Self::Staged { .. } => ("staged", None),
            Self::Switched { .. } => ("switched", None),
            Self::Confirmed { .. } => ("confirmed", None),
            Self::Reverted { .. } => ("reverted", None),
            Self::Queried(_) => ("query", None),
            Self::Activated { .. } => ("activated", None),
            Self::Unpinned { .. } => ("unpinned", None),
            Self::Refused(code, _) | Self::Unresolved { code, .. } => {
                ("refused", Some(code.code()))
            }
            Self::Failed(code, _) => ("failed", Some(code.code())),
        }
    }

    /// The detail of a refusal or a failure, cleaned and cut, for the log; none for an
    /// answer that went well.
    pub fn log_detail(&self) -> Option<String> {
        let detail = match self {
            Self::Refused(_, detail)
            | Self::Failed(_, detail)
            | Self::Unresolved { detail, .. } => detail,
            _ => return None,
        };
        let mut text = String::new();
        for c in detail.chars() {
            let c = if c.is_control() { '?' } else { c };
            if text.len() + c.len_utf8() > MAX_DETAIL_LEN {
                break;
            }
            text.push(c);
        }
        Some(text)
    }
}

/// The JSON text of `value`. Serializing strings and booleans does not fail.
fn to_json<T: Serialize>(value: &T) -> String {
    serde_json::to_string(value).unwrap_or_default()
}
