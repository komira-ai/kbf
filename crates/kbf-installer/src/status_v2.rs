//! `status.json`, schema 2 (docs/design/kbf-installer.md section 4): the installer's word to
//! `kbf`, root-owned and world-readable, replaced whole. This is the *shape*, fixed by the
//! golden fixture `testdata/golden/status.json`; the writer comes with `P4`.
//!
//! `state` words map 1:1 to `UpdateStatus.State` on the wire. `refusal` words are the stable
//! set of section 5; lock contention is exit 4, not a refusal.

use serde::{Deserialize, Serialize};

use crate::CommitId;

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

/// Why an order was refused (exit 3).
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

/// The last refusal, with the reason a person reads.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RefusalNote {
    pub word: Refusal,
    pub reason: String,
}

/// What a staged release declares (`kbf --version --json`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StagedFacts {
    pub protocol: u32,
    pub min_protocol: u32,
    pub state_schema: u32,
}

/// What the probe of the newest installed release reported.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProbeFacts {
    pub commit: CommitId,
    pub platform: String,
    pub protocol: u32,
    pub min_protocol: u32,
    pub state_schema: u32,
    pub profile: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StatusV2 {
    pub schema: u64,
    pub installed_commit: Option<CommitId>,
    pub previous_commit: Option<CommitId>,
    pub staged_commit: Option<CommitId>,
    pub state: Phase,
    pub refusal: Option<RefusalNote>,
    pub acked_order_id: u64,
    pub staged: Option<StagedFacts>,
    pub probe: Option<ProbeFacts>,
    /// Days until the trust root expires; negative once it has.
    pub trust_days_left: Option<i64>,
    /// Releases a link names that carry no `VERIFIED` marker: a rollback to them is refused.
    pub unverified: Vec<CommitId>,
}

impl StatusV2 {
    pub fn parse(bytes: &[u8]) -> Result<StatusV2, String> {
        let status: StatusV2 = serde_json::from_slice(bytes).map_err(|e| e.to_string())?;
        if status.schema != SCHEMA {
            return Err(format!("status.json is schema {}, not 2", status.schema));
        }
        Ok(status)
    }

    /// The bytes as written: indented, a newline last.
    pub fn render(&self) -> Result<Vec<u8>, serde_json::Error> {
        let mut bytes = serde_json::to_vec_pretty(self)?;
        bytes.push(b'\n');
        Ok(bytes)
    }
}

#[cfg(test)]
#[path = "status_v2_tests.rs"]
mod tests;
