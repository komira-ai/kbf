//! The seams of the update API. Nothing U8a depends on exists on the feature branch yet
//! (no proxy crate, no child admin calls, no `frontend_update` order writer), so each of
//! them is a trait with a fake in the tests; U7b and U8b supply the real ones.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use bytes::Bytes;
use hyper::{Request, Response};
use serde::Deserialize;

use super::model::{
    Failure, Locator, NewChild, OldChild, OrderVerb, Phase, Slot, StagedFacts, UpdateId,
};
use crate::CommitId;

/// Writes one record into `orders/` (FP section 2). The order ids (`order_id`, `epoch`,
/// `generation`) come from the writer, which owns their counter: where that counter
/// lives is open (Q1 names the record, not the counter), so the API never invents one.
pub trait OrderWriter: Send {
    fn write(
        &mut self,
        verb: OrderVerb,
        commit: &CommitId,
        window_s: u32,
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
    #[serde(default)]
    pub acked_order_id: u64,
    #[serde(default)]
    pub staged: Option<StagedFacts>,
}

impl InstallerStatus {
    /// The stable word of the refusal: what precedes the first colon.
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

/// The marker the package's postinst writes when the proxy itself has been upgraded and
/// not restarted (FP section 10). Its path is not named by the design: the proxy state
/// directory is assumed.
pub struct RestartMarker {
    path: PathBuf,
}

impl RestartMarker {
    pub fn new(path: impl Into<PathBuf>) -> RestartMarker {
        RestartMarker { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn is_set(&self) -> bool {
        self.path.exists()
    }
}

/// One update's facts as the supervisor (U7) reports them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunningUpdate {
    pub update_id: UpdateId,
    pub from: CommitId,
    pub to: CommitId,
    pub phase: Phase,
    pub since_s: u64,
    pub active_slot: Slot,
    pub old: Option<OldChild>,
    pub new: NewChild,
    /// How many nodes the new child has begun to update.
    pub nodes_updated: u32,
    pub failure: Option<Failure>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SupervisorStatus {
    /// The newest update the supervisor knows, open or ended.
    pub update: Option<RunningUpdate>,
    /// An old child still lives from an update that is otherwise over.
    pub old_child_draining: bool,
    /// The commit the active child runs, if it knows.
    pub running: Option<CommitId>,
}

/// What `start` is given.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UpdateSpec {
    pub update_id: UpdateId,
    pub from: Option<CommitId>,
    pub to: CommitId,
    pub nodes: bool,
    pub rollback: bool,
    pub facts: StagedFacts,
    pub order: Locator,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Retired {
    pub dropped_streams: u64,
}

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum SupervisorError {
    #[error("no update is open")]
    NotOpen,
    #[error("refused: {0}")]
    Refused(String),
    #[error("{0}")]
    Failed(String),
}

/// The U7 swap. Every call is made by the one actor, so an implementation sees them one
/// at a time. `start` is idempotent on `update_id`.
pub trait Supervisor: Send {
    fn start(&mut self, spec: &UpdateSpec) -> Result<(), SupervisorError>;
    fn abort(&mut self) -> Result<Phase, SupervisorError>;
    fn rollback(&mut self) -> Result<Phase, SupervisorError>;
    fn force_retire(&mut self, reason: &str) -> Result<Retired, SupervisorError>;
    fn status(&self) -> SupervisorStatus;
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
