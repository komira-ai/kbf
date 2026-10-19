//! The seams of the update API. Nothing U8a depends on exists on the feature branch yet
//! (no proxy crate, no child admin calls, no `frontend_update` order writer), so each of
//! them is a trait with a fake in the tests; U7b and U8b supply the real ones.

use std::path::PathBuf;

use async_trait::async_trait;
use bytes::Bytes;
use hyper::{Request, Response};
use serde::Deserialize;

use super::fleet::FleetSnapshot;
use super::model::{NewChild, OldChild, UpdateId};
use crate::CommitId;
use crate::proxy::model::{Failure, Locator, Phase, Slot, StagedFacts, Verb};

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
    pub staged: Option<StagedWire>,
}

/// `staged{}` as `status.json` carries it: the facts, not the commit (that is
/// `staged_commit`, beside it). [`InstallerStatus::staged_facts`] joins the two.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct StagedWire {
    pub protocol: u32,
    pub min_protocol: u32,
    pub state_schema: u32,
    #[serde(default)]
    pub profile: Option<String>,
}

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
    /// The caller's idempotency key; the swap records it in `update.json`.
    pub request_id: String,
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

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
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

/// The verdict of the compatibility gate (FP section 9).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Compat {
    Compatible,
    /// A rule is broken: `rule` is its name (`R1`) and `detail` says what broke it, for the
    /// operator.
    Incompatible {
        rule: String,
        detail: String,
    },
    /// The release is not complete for this platform.
    Incomplete {
        reason: String,
    },
}

/// A staged release and the fleet it would meet: what a gate needs to judge the release
/// itself and not only its name.
#[derive(Clone, Copy, Debug)]
pub struct Staged<'a> {
    pub facts: &'a StagedFacts,
    pub fleet: &'a FleetSnapshot,
}

/// The gate: rule R1 ([`R1Gate`](super::compat::R1Gate)). R4 is the child's, not the
/// proxy's (FP section 9).
///
/// With `staged` of `None` the gate answers from what is knowable before the release is
/// fetched, which is its commit; with `Some` it judges the staged release against the
/// fleet read for it. The caller reads the fleet
/// ([`FleetReader`](super::fleet::FleetReader)) only for the second call, so the first
/// costs no request.
pub trait CompatGate: Send {
    fn check(&self, commit: &CommitId, staged: Option<Staged<'_>>) -> Compat;
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
