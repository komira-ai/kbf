//! What the server keeps of each host's `UpdateStatus` (`docs/design/pull-updates.md`
//! sections 6.3 and 6.4, `docs/design/update-ui.md` section 5), and the two views of it:
//! the open per-node summary on `GET /v1/nodes` ([`UpdateSummary`]) and the per-host
//! detail of `GET /v1/update/hosts` ([`HostUpdate`], served by [`crate::update_hosts`]).
//!
//! - **Text from a host is data.** A host sends the strings, so each is kept cut to a
//!   byte bound on a character boundary with control characters replaced
//!   ([`REFUSAL_MAX_BYTES`], [`COMMIT_MAX_BYTES`]); a reader escapes it as it would any
//!   string from outside.
//! - **The newest report is kept across streams**, as `NodeStatus` is, and marked live
//!   only while the node's newest stream has sent it ([`Availability`]): a host that
//!   restarted onto another release is not judged by what its previous process said.
//! - **A silent host is never idle.** `availability` is `unsupported` for a host that
//!   never sent an `UpdateStatus` (a daemon without the update client), `unavailable`
//!   for one whose report is not live, and `ok` otherwise; `state` is `null` unless it
//!   is `ok`.
//! - **`up_to_date` is three-valued**: `true` or `false` against the desired commit, and
//!   `null` whenever the server cannot judge (no desired release, a host that is not
//!   `ok`, a running commit that is not a hex prefix of a commit).

use kbf_proto::update::UpdateStatus;
use kbf_proto::update::update_status::State;
use serde::Serialize;

/// The most bytes of a host's `refusal` that are kept.
pub const REFUSAL_MAX_BYTES: usize = 512;

/// The most bytes of each commit a host reports that are kept (a full commit is 40).
pub const COMMIT_MAX_BYTES: usize = 64;

/// The shortest running commit judged against the desired one (the daemon reports 12
/// hex digits).
const MIN_RUNNING_DIGITS: usize = 12;

/// A host's newest `UpdateStatus`, and when the server received it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UpdateReport {
    /// The host's own step state: `idle`, `fetching`, `staged`, `activated_unconfirmed`,
    /// `confirmed`, `reverted`, `refused`, `manual`, or `unknown` (a state this server
    /// does not know).
    pub state: &'static str,
    /// The commit the process was built from, as the host said (12 hex digits, or
    /// `unknown`).
    pub running_commit: String,
    /// The commit installed as current; empty if the host does not know.
    pub installed_commit: String,
    /// The commit staged and verified, not yet activated; empty if none.
    pub staged_commit: String,
    /// Why the host refused, at most [`REFUSAL_MAX_BYTES`] bytes.
    pub refusal: String,
    /// The newest offer generation the host has seen.
    pub offered_generation: u64,
    /// Whether the privileged helper is present on the host.
    pub helper_present: bool,
    /// When the server received it: milliseconds since the Unix epoch, server clock.
    pub received_at_unix_ms: u64,
}

impl UpdateReport {
    /// `status`, received at `received_at_unix_ms`, with every string made safe to keep.
    #[must_use]
    pub fn new(status: UpdateStatus, received_at_unix_ms: u64) -> Self {
        let state = match State::try_from(status.state) {
            Ok(State::Idle) => "idle",
            Ok(State::Fetching) => "fetching",
            Ok(State::Staged) => "staged",
            Ok(State::ActivatedUnconfirmed) => "activated_unconfirmed",
            Ok(State::Confirmed) => "confirmed",
            Ok(State::Reverted) => "reverted",
            Ok(State::Refused) => "refused",
            Ok(State::Manual) => "manual",
            Ok(State::Unspecified) | Err(_) => "unknown",
        };
        Self {
            state,
            running_commit: as_data(&status.running_commit, COMMIT_MAX_BYTES),
            installed_commit: as_data(&status.installed_commit, COMMIT_MAX_BYTES),
            staged_commit: as_data(&status.staged_commit, COMMIT_MAX_BYTES),
            refusal: as_data(&status.refusal, REFUSAL_MAX_BYTES),
            offered_generation: status.offered_generation,
            helper_present: status.helper_present,
            received_at_unix_ms,
        }
    }
}

/// `text` kept as data: control characters replaced by U+FFFD, then cut to at most `max`
/// bytes without splitting a character.
fn as_data(text: &str, max: usize) -> String {
    let mut kept = String::new();
    for c in text.chars() {
        let c = if c.is_control() {
            char::REPLACEMENT_CHARACTER
        } else {
            c
        };
        if kept.len() + c.len_utf8() > max {
            break;
        }
        kept.push(c);
    }
    kept
}

/// Whether a host's update report can be trusted to describe it now.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Availability {
    /// Its newest stream is open and has sent an `UpdateStatus`.
    Ok,
    /// It has sent one, but not on its newest stream, or that stream has closed: what
    /// was kept is what it last said.
    Unavailable,
    /// It has never sent one: it takes no update by itself, or runs a daemon that
    /// predates the update client.
    Unsupported,
}

impl Availability {
    /// `has_report`: the node ever sent an `UpdateStatus`; `connected`: its newest
    /// stream is open; `live`: the report came on that stream.
    #[must_use]
    pub const fn of(has_report: bool, connected: bool, live: bool) -> Self {
        match (has_report, connected && live) {
            (false, _) => Self::Unsupported,
            (true, true) => Self::Ok,
            (true, false) => Self::Unavailable,
        }
    }
}

/// Whether a host running `running` runs the `desired` commit: `None` if `running` is
/// not a lowercase hex prefix of a commit ([`MIN_RUNNING_DIGITS`] digits or more, such as
/// `unknown`), which cannot be judged.
#[must_use]
pub fn up_to_date(running: &str, desired: &str) -> Option<bool> {
    let hex = running
        .bytes()
        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
    (hex && running.len() >= MIN_RUNNING_DIGITS).then(|| desired.starts_with(running))
}

/// A node's update state as `GET /v1/nodes` lists it: open, so no commit and no free
/// text (`update-ui.md` section 3, rule 5).
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct UpdateSummary {
    /// The host's step state; `null` unless `availability` is `ok`.
    pub state: Option<&'static str>,
    /// Whether it runs the desired commit; `null` when that cannot be judged.
    pub up_to_date: Option<bool>,
    /// Whether the report describes the host now.
    pub availability: Availability,
    /// What it reported running, to judge against a desired commit.
    #[serde(skip)]
    running: Option<String>,
}

impl UpdateSummary {
    /// The summary of `report` for a node that is (not) `connected`, `live` as for
    /// [`Availability::of`]; `up_to_date` is `null` until [`Self::judge`].
    #[must_use]
    pub fn new(report: Option<&UpdateReport>, connected: bool, live: bool) -> Self {
        let availability = Availability::of(report.is_some(), connected, live);
        let ok = availability == Availability::Ok;
        Self {
            state: report.filter(|_| ok).map(|r| r.state),
            up_to_date: None,
            availability,
            running: report.filter(|_| ok).map(|r| r.running_commit.clone()),
        }
    }

    /// Judges the host against the `desired` commit (`None`: there is none).
    pub fn judge(&mut self, desired: Option<&str>) {
        self.up_to_date = self
            .running
            .as_deref()
            .zip(desired)
            .and_then(|(running, desired)| up_to_date(running, desired));
    }
}

/// One host as `GET /v1/update/hosts` lists it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct HostUpdate {
    /// The node id its daemon registered with.
    pub node_id: String,
    /// Whether its newest stream is still open.
    pub connected: bool,
    /// Whether the report describes the host now.
    pub availability: Availability,
    /// The host's step state; `null` unless `availability` is `ok`.
    pub state: Option<&'static str>,
    /// Whether it runs the desired commit; `null` when that cannot be judged.
    pub up_to_date: Option<bool>,
    /// The commit the farm wants (`null`: none, or the update state is not served).
    pub desired_commit: Option<String>,
    /// As the host last reported; `null` if it never did. The fields below are too.
    pub running_commit: Option<String>,
    /// The commit installed as current (`""`: the host does not know).
    pub installed_commit: Option<String>,
    /// The commit staged, not yet activated (`""`: none).
    pub staged_commit: Option<String>,
    /// Why the host refused, a string from the host: at most [`REFUSAL_MAX_BYTES`]
    /// bytes with control characters replaced.
    pub refusal: Option<String>,
    /// The newest offer generation the host has seen.
    pub offered_generation: Option<u64>,
    /// Whether the privileged helper is present.
    pub helper_present: Option<bool>,
    /// When the server received the report: milliseconds since the Unix epoch.
    pub reported_at_unix_ms: Option<u64>,
    #[serde(skip)]
    summary: UpdateSummary,
}

impl HostUpdate {
    /// The detail of `node_id`, judged against no desired commit until [`Self::judge`].
    #[must_use]
    pub fn new(node_id: String, connected: bool, live: bool, report: Option<UpdateReport>) -> Self {
        let summary = UpdateSummary::new(report.as_ref(), connected, live);
        Self {
            node_id,
            connected,
            availability: summary.availability,
            state: summary.state,
            up_to_date: None,
            desired_commit: None,
            running_commit: report.as_ref().map(|r| r.running_commit.clone()),
            installed_commit: report.as_ref().map(|r| r.installed_commit.clone()),
            staged_commit: report.as_ref().map(|r| r.staged_commit.clone()),
            refusal: report.as_ref().map(|r| r.refusal.clone()),
            offered_generation: report.as_ref().map(|r| r.offered_generation),
            helper_present: report.as_ref().map(|r| r.helper_present),
            reported_at_unix_ms: report.as_ref().map(|r| r.received_at_unix_ms),
            summary,
        }
    }

    /// Fills `desired_commit` and `up_to_date` from the `desired` commit.
    pub fn judge(&mut self, desired: Option<&str>) {
        self.summary.judge(desired);
        self.up_to_date = self.summary.up_to_date;
        self.desired_commit = desired.map(str::to_owned);
    }
}

#[cfg(test)]
mod tests;
