//! What the swap's supervisor deals in: the update it is asked to start, the status it reports,
//! its refusals, and the question of whether the running proxy is still the installed program.
//!
//! These types were the update API's (`api::traits`, `api::model`), and the API still names them
//! through re-exports. They live here so that the swap and the binary watch name nothing of the
//! API.

use serde::{Deserialize, Serialize};

use super::model::{Failure, Locator, Phase, Slot, Source, StagedFacts};
use crate::CommitId;

/// An update's id, assigned by whoever starts it and stable across a retried request.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct UpdateId(pub String);

/// The old child's counters: streams are counted by the proxy; `open_executions` and
/// `queued_operations` are the wire's fields for what the child reports of itself (Q8).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OldChild {
    pub open_streams: u64,
    pub oldest_stream_age_s: u64,
    pub open_executions: u64,
    pub queued_operations: u64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NewChild {
    pub open_streams: u64,
}

/// Whether the running proxy is no longer the installed program (FP section 10): the
/// package manager replaced the binary and nothing restarted the proxy. There is no marker
/// file; [`crate::proxy::RunningBinary`] compares file identities.
pub trait RestartPending: Send {
    fn restart_pending(&self) -> bool;
}

/// One update's facts as the supervisor (U7) reports them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunningUpdate {
    pub update_id: UpdateId,
    pub from: CommitId,
    pub to: CommitId,
    pub phase: Phase,
    pub since_s: u64,
    /// The slot of the child that is SERVING requests now, not the candidate and not the
    /// child that was started last: the old child's slot until the switch, the new child's
    /// from `switched` on, and the old child's again after a rollback or a revert (the new
    /// child was stood down). `GET /v1/update/frontend` reports it as `active_slot`, and
    /// `kbf-installer deploy` reads it to follow the frontend across a handover
    /// (docs/design/frontend-proxy.md section 4).
    pub active_slot: Slot,
    pub old: Option<OldChild>,
    pub new: NewChild,
    /// How many nodes the new child has begun to update.
    pub nodes_updated: u32,
    pub failure: Option<Failure>,
    /// Where the update's release came from.
    pub source: Source,
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
    /// Where the release came from.
    pub source: Source,
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

/// The U7 swap. Every call is made by one caller at a time (the update API's actor, or the
/// binary watch), so an implementation sees them one at a time. `start` is idempotent on
/// `update_id`.
pub trait Supervisor: Send {
    /// Why `start` cannot be called at all, when that is known before an update is
    /// accepted. `POST` then answers 503 with this text, and saves no `request_id` and
    /// writes no order, so the installer does not fetch a release that nothing can take
    /// over. `None` is a supervisor that can start.
    fn start_blocked(&self) -> Option<String>;

    fn start(&mut self, spec: &UpdateSpec) -> Result<(), SupervisorError>;
    fn abort(&mut self) -> Result<Phase, SupervisorError>;
    fn rollback(&mut self) -> Result<Phase, SupervisorError>;
    fn force_retire(&mut self, reason: &str) -> Result<Retired, SupervisorError>;
    fn status(&self) -> SupervisorStatus;
}
