//! `status.json`, schema 2 (docs/design/kbf-installer.md section 4), as a type: the installer's
//! word to `kbf`, root-owned and world-readable, replaced whole.
//!
//! The writer is `helper/status.rs`. This is the strict reader of what it writes (a key it does
//! not name is refused), pinned to the golden fixture `testdata/golden/status.json`, which is
//! the writer's bytes: a key added to the writer moves the writer's golden test, this type and
//! the fixture in the same change. The readers that matter ignore keys they do not know, so a
//! key added here is additive to them: the node's (`kbf-update`, `helper.rs`) and the frontend
//! proxy's ([`crate::proxy::api::traits::InstallerStatus`]); each is tested against the
//! writer's bytes too.
//!
//! `state` words map 1:1 to `UpdateStatus.State` on the wire. `refusal` is the last refusal as
//! text, `<word>: <message>`, the word one of section 5's; lock contention is exit 4, not a
//! refusal.

use serde::{Deserialize, Serialize};

use crate::CommitId;
use crate::seed::Available;

pub const SCHEMA: u64 = 2;

/// What the host is doing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Idle,
    Fetching,
    Staged,
    ActivatedUnconfirmed,
    Confirmed,
    Reverted,
    Refused,
}

/// Why an order was refused (exit 3): the words that lead `refusal`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Refusal {
    TrustChain,
    Hash,
    Attestation,
    CommitOrRef,
    Facts,
    InstallerTooOld,
    Platform,
    Size,
    Disk,
    Fetch,
    Replay,
    Generation,
    Floor,
    Pending,
    Lease,
    Denied,
    /// Frontend update only: the installed Envoy is outside the tested range.
    EnvoyRange,
    /// Frontend update only: a second `frontend_update` order while one is in flight.
    FrontendBusy,
}

impl Refusal {
    pub const ALL: [Refusal; 18] = [
        Refusal::TrustChain,
        Refusal::Hash,
        Refusal::Attestation,
        Refusal::CommitOrRef,
        Refusal::Facts,
        Refusal::InstallerTooOld,
        Refusal::Platform,
        Refusal::Size,
        Refusal::Disk,
        Refusal::Fetch,
        Refusal::Replay,
        Refusal::Generation,
        Refusal::Floor,
        Refusal::Pending,
        Refusal::Lease,
        Refusal::Denied,
        Refusal::EnvoyRange,
        Refusal::FrontendBusy,
    ];
}

/// What a staged release declares (`kbf --version --json`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StagedFacts {
    pub protocol: u32,
    pub min_protocol: u32,
    pub state_schema: u32,
    /// The build profile the release declared, when the installer has it.
    pub profile: Option<String>,
}

/// The pinned Envoy, once the installer manages one; today neither is set.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Envoy {
    pub current: Option<String>,
    pub previous: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StatusV2 {
    pub schema: u64,
    pub installed_commit: Option<CommitId>,
    pub previous_commit: Option<CommitId>,
    pub staged_commit: Option<CommitId>,
    pub state: Phase,
    /// `<word>: <message>` of the last refusal or revert, or empty.
    pub refusal: String,
    pub acked_order_id: u64,
    pub staged: Option<StagedFacts>,
    pub envoy: Envoy,
    /// The word `installer.toml` holds for `release_source`: `url`, `frontend` or `bundled`. Left
    /// out when the file could not be read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub release_source: Option<String>,
    /// The seed the host's package carries, once it is found whole and above the floor of `seq`.
    /// Left out when there is none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub available: Option<Available>,
    /// Releases a link names that carry no `VERIFIED` marker: a rollback to them is refused.
    /// Left out when there are none.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unverified: Vec<CommitId>,
}

impl StatusV2 {
    /// The file strictly: schema 2, and no key but these.
    pub fn parse(bytes: &[u8]) -> Result<StatusV2, String> {
        let status: StatusV2 = serde_json::from_slice(bytes).map_err(|e| e.to_string())?;
        if status.schema != SCHEMA {
            return Err(format!("status.json is schema {}, not 2", status.schema));
        }
        Ok(status)
    }

    /// The bytes as the writer writes them: one line, a newline last.
    pub fn render(&self) -> Result<Vec<u8>, serde_json::Error> {
        let mut bytes = serde_json::to_vec(self)?;
        bytes.push(b'\n');
        Ok(bytes)
    }
}

#[cfg(test)]
#[path = "status_v2_tests.rs"]
mod tests;
