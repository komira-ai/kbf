//! What the server keeps of each host's `UpdateStatus` (`docs/design/pull-updates.md`
//! sections 6.3 and 6.4): the fields the update controller reads, in an [`UpdateReport`].
//! No operator view lists it.
//!
//! - **Text from a host is data.** A host sends the strings, so each is kept cut to a
//!   byte bound on a character boundary with control characters replaced
//!   ([`COMMIT_MAX_BYTES`]).
//! - **The newest report is kept across streams**, as `NodeStatus` is; the controller
//!   reads it only while the node's newest stream has sent it.

use kbf_proto::update::UpdateStatus;
use kbf_proto::update::update_status::ReleaseSource as WireSource;
use kbf_proto::update::update_status::State;

/// The most bytes of each commit a host reports that are kept (a full commit is 40).
pub const COMMIT_MAX_BYTES: usize = 64;

/// Where a host's installer fetches releases from, as the host reported it. A host that
/// fetches nothing ([`ReleaseSource::Bundled`]) cannot carry out an order that fetches.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReleaseSource {
    /// The host does not say: its installer predates the field, or the host sent a number
    /// this build does not know. Never read as one of the others.
    Unreported,
    /// An https file server.
    Url,
    /// The frontend's release store.
    Frontend,
    /// Nothing: the release is the one the host's package carries.
    Bundled,
}

/// A release the host's package carries, as the host reported it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AvailableRelease {
    /// The commit as the host said it, at most [`COMMIT_MAX_BYTES`] bytes with control
    /// characters replaced; not checked to be a commit.
    pub commit: String,
    /// The number of commits behind it: puts two builds of one branch in order.
    pub seq: u64,
}

/// A host's newest `UpdateStatus`, cut down to what the update controller reads.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UpdateReport {
    /// The commit the process was built from, as the host said (12 hex digits, or
    /// `unknown`).
    pub running_commit: String,
    /// The commit installed as current; empty if the host does not know.
    pub installed_commit: String,
    /// The newest order id the host acted on: the answer to the controller's orders.
    pub acked_order_id: u64,
    /// The state as the wire gave it.
    pub kind: State,
    /// The release the host's package carries, when its installer has found it whole; `None`
    /// if it reported none, or its installer predates the field.
    pub available: Option<AvailableRelease>,
    /// Where the host's installer fetches releases from; [`ReleaseSource::Unreported`] if it
    /// did not say or sent a number this build does not know.
    pub release_source: ReleaseSource,
}

impl UpdateReport {
    /// `status`, with every string made safe to keep.
    #[must_use]
    pub fn new(status: UpdateStatus) -> Self {
        let release_source = match WireSource::try_from(status.release_source) {
            Ok(WireSource::Url) => ReleaseSource::Url,
            Ok(WireSource::Frontend) => ReleaseSource::Frontend,
            Ok(WireSource::Bundled) => ReleaseSource::Bundled,
            Ok(WireSource::Unspecified) | Err(_) => ReleaseSource::Unreported,
        };
        Self {
            running_commit: as_data(&status.running_commit, COMMIT_MAX_BYTES),
            installed_commit: as_data(&status.installed_commit, COMMIT_MAX_BYTES),
            acked_order_id: status.acked_order_id,
            kind: State::try_from(status.state).unwrap_or(State::Unspecified),
            available: status.available.map(|a| AvailableRelease {
                commit: as_data(&a.commit, COMMIT_MAX_BYTES),
                seq: a.seq,
            }),
            release_source,
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

#[cfg(test)]
mod tests;
