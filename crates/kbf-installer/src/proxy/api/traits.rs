//! The seams of the update API. Nothing U8a depends on exists on the feature branch yet
//! (no proxy crate, no child admin calls, no `frontend_update` order writer), so each of
//! them is a trait with a fake in the tests; U7b and U8b supply the real ones.

use std::path::PathBuf;

use async_trait::async_trait;
use bytes::Bytes;
use hyper::{Request, Response};
use kbf_update::Source;
use serde::Deserialize;

use crate::CommitId;
use crate::proxy::model::{Locator, StagedFacts, Verb};

// The swap's supervisor and what it deals in are the swap's (`proxy::kinds`), named here as
// they always were.
pub use crate::proxy::kinds::{
    RestartPending, Retired, RunningUpdate, Supervisor, SupervisorError, SupervisorStatus,
    UpdateSpec,
};

/// Writes one record into `orders/` (FP section 2). The order ids (`order_id`, `epoch`,
/// `generation`) come from the writer, which owns their counter: where that counter
/// lives is open (Q1 names the record, not the counter), so the API never invents one.
pub trait OrderWriter: Send {
    fn write(
        &mut self,
        verb: Verb,
        commit: &CommitId,
        window_s: u32,
    ) -> Result<Locator, OrderError>;

    /// A `frontend_update` for `commit` that takes its release from `source`: the record names
    /// it unless it is `fetch`, which a record without the key means.
    fn write_update(
        &mut self,
        commit: &CommitId,
        window_s: u32,
        source: Source,
    ) -> Result<Locator, OrderError>;
}

#[derive(Debug, thiserror::Error)]
#[error("the order could not be written: {0}")]
pub struct OrderError(pub String);

/// The installer's `status.json`, the part the API reads (KI section 4). Unknown fields
/// are ignored: the installer is the writer and may add fields.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
pub struct InstallerStatus {
    #[serde(default)]
    pub installed_commit: Option<CommitId>,
    #[serde(default)]
    pub staged_commit: Option<CommitId>,
    #[serde(default)]
    pub state: InstallerState,
    /// `<word>: <message>`, or empty (`helper/cycle.rs` writes the word first).
    #[serde(default)]
    pub refusal: String,
    /// The id of the last `activate`, `confirm` or `revert` order the installer acted on.
    /// A `frontend_update` order has a floor of its own and a refused one moves none, so
    /// this cannot say whose refusal `refusal` is: see `refused_order_id`.
    #[serde(default)]
    pub acked_order_id: u64,
    /// The id of the order that `refusal` answers, when it answers one. A refused state is an
    /// update's own only when this names that update's order; without it (a refusal that is no
    /// order's, or a root half from before the field) no refusal is attributed to any update,
    /// and an order the installer refuses ends at the staging deadline instead.
    #[serde(default)]
    pub refused_order_id: Option<u64>,
    #[serde(default)]
    pub staged: Option<StagedWire>,
    /// The seed the host's package carries, once the root half has found it whole and above
    /// its floor of `seq`; absent from a root half that predates it.
    #[serde(default)]
    pub available: Option<AvailableWire>,
    /// The word `installer.toml` holds for `release_source` (`url`, `frontend` or `bundled`);
    /// absent from a root half that predates it, or one that could not read the file.
    #[serde(default)]
    pub release_source: Option<String>,
}

/// `available{}` as `status.json` carries it.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct AvailableWire {
    pub commit: CommitId,
    pub seq: u64,
}

/// `staged{}` as `status.json` carries it: the facts, not the commit (that is
/// `staged_commit`, beside it). [`InstallerStatus::staged_facts`] joins the two. The type is
/// the one the swap reads its own status file with.
pub use crate::proxy::releases::StagedWire;

impl InstallerStatus {
    /// The stable word of the refusal: what precedes the first colon.
    pub fn staged_facts(&self) -> Option<StagedFacts> {
        let (commit, wire) = (self.staged_commit.as_ref()?, self.staged.as_ref()?);
        Some(StagedFacts {
            commit: commit.clone(),
            protocol: wire.protocol,
            min_protocol: wire.min_protocol,
            state_schema: wire.state_schema,
            profile: wire.profile.clone(),
        })
    }

    pub fn refusal_word(&self) -> &str {
        self.refusal.split(':').next().unwrap_or("").trim()
    }
}

/// The `state` words of KI section 4.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InstallerState {
    #[default]
    Idle,
    Fetching,
    Staged,
    ActivatedUnconfirmed,
    Confirmed,
    Reverted,
    Refused,
    /// A word this build does not know: treated as busy, never as idle.
    #[serde(other)]
    Unknown,
}

#[derive(Debug, thiserror::Error)]
#[error("status.json is unreadable: {0}")]
pub struct StatusError(pub String);

pub trait StatusReader: Send {
    fn read(&self) -> Result<InstallerStatus, StatusError>;
}

/// `status.json` on disk (root-owned, world-readable, replaced whole by a rename, so a
/// read sees one complete version).
pub struct JsonFileStatus {
    path: PathBuf,
}

impl JsonFileStatus {
    pub fn new(path: impl Into<PathBuf>) -> JsonFileStatus {
        JsonFileStatus { path: path.into() }
    }
}

impl StatusReader for JsonFileStatus {
    fn read(&self) -> Result<InstallerStatus, StatusError> {
        let bytes = std::fs::read(&self.path).map_err(|e| StatusError(e.to_string()))?;
        serde_json::from_slice(&bytes).map_err(|e| StatusError(e.to_string()))
    }
}

/// The verdict of rules R1 and R4 (FP section 9).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Compat {
    Compatible,
    Incompatible {
        rule: String,
    },
    /// The release is not complete for this platform.
    Incomplete {
        reason: String,
    },
}

/// R1/R4. With `facts` of `None` the gate answers from what is knowable before the
/// release is fetched; with `Some` it judges the staged release (U9 probes
/// `releases/<commit>/kbf --version --json`). Stubbed until U9.
pub trait CompatGate: Send {
    fn check(&self, commit: &CommitId, facts: Option<&StagedFacts>) -> Compat;
}

/// The gate until U9 lands: everything is compatible.
pub struct StubGate;

impl CompatGate for StubGate {
    fn check(&self, _commit: &CommitId, _facts: Option<&StagedFacts>) -> Compat {
        Compat::Compatible
    }
}

#[derive(Debug, thiserror::Error)]
#[error("the operator port did not answer: {0}")]
pub struct ForwardError(pub String);

/// Sends a request on to the operator port of the active child.
#[async_trait]
pub trait Forwarder: Send + Sync {
    async fn forward(&self, req: Request<Bytes>) -> Result<Response<Bytes>, ForwardError>;
}

/// Time as an input. `mono_ms` never goes backwards; `wall_s` is Unix seconds.
pub trait Clock: Send + Sync {
    fn mono_ms(&self) -> u64;
    fn wall_s(&self) -> u64;
}

/// The real clock.
pub struct SystemClock {
    start: std::time::Instant,
}

impl SystemClock {
    pub fn new() -> SystemClock {
        SystemClock {
            start: std::time::Instant::now(),
        }
    }
}

impl Default for SystemClock {
    fn default() -> SystemClock {
        SystemClock::new()
    }
}

impl Clock for SystemClock {
    fn mono_ms(&self) -> u64 {
        u64::try_from(self.start.elapsed().as_millis()).unwrap_or(u64::MAX)
    }

    fn wall_s(&self) -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs())
    }
}
