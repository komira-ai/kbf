//! The JSON bodies of the update API (`docs/design/frontend-proxy.md` section 4) and the
//! rules it applies itself. Phases, steps, slots, failures, order locators and staged facts
//! are the swap's: [`crate::proxy::model`].

use serde::{Deserialize, Serialize};

use crate::CommitId;
use crate::proxy::model::{Failure, Phase, Slot};

/// An update's id, assigned by the API and stable across a retried request.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct UpdateId(pub String);

/// The old child's counters: streams are counted by the proxy, executions and queued
/// operations are reported by the child (Q8; `open_sessions` is dropped).
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

/// The body of `POST /v1/update/frontend`. Unknown fields are refused.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateRequest {
    pub commit: CommitId,
    pub request_id: String,
    #[serde(default = "yes")]
    pub nodes: bool,
    #[serde(default)]
    pub rollback: bool,
}

fn yes() -> bool {
    true
}

/// The body of `POST .../force-retire`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ForceRetireRequest {
    #[serde(default)]
    pub reason: String,
}

/// `GET /v1/update/frontend`. With no update, everything but `restart_pending` and
/// `rollback_allowed` is null.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct StatusBody {
    pub update_id: Option<UpdateId>,
    pub from: Option<CommitId>,
    pub to: Option<CommitId>,
    pub phase: Option<Phase>,
    /// Unix seconds at which the update entered `phase`.
    pub since: Option<u64>,
    pub active_slot: Option<Slot>,
    pub restart_pending: bool,
    pub rollback_allowed: bool,
    pub rollback_blocked_reason: Option<RollbackBlocked>,
    pub old: Option<OldChild>,
    pub new: Option<NewChild>,
    pub failure: Option<Failure>,
}

/// Why `rollback` is refused (FP section 4: "after switched while the old child lives
/// and no node updated").
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RollbackBlocked {
    NoUpdate,
    NotSwitched,
    OldChildGone,
    NodesUpdated,
}

impl RollbackBlocked {
    pub fn describe(self) -> &'static str {
        match self {
            RollbackBlocked::NoUpdate => "no update is open",
            RollbackBlocked::NotSwitched => "the update has not switched yet (use abort)",
            RollbackBlocked::OldChildGone => "the old child is gone",
            RollbackBlocked::NodesUpdated => "a node has already been updated",
        }
    }
}

/// `abort` is allowed before the flip (FP: "before switching").
pub fn abortable(phase: Phase) -> bool {
    phase < Phase::Switching
}

/// The phases in which the new child is the active one and the update is not over.
fn switched(phase: Phase) -> bool {
    (Phase::Switched..=Phase::Draining).contains(&phase)
}

/// Whether rollback is allowed, from the facts the supervisor reports. The API applies it
/// itself, so a supervisor that forgot a rule cannot roll back past a node update.
pub fn rollback_gate(
    phase: Phase,
    old_child_alive: bool,
    nodes_updated: u32,
) -> Result<(), RollbackBlocked> {
    if phase.is_terminal() {
        Err(RollbackBlocked::NoUpdate)
    } else if !switched(phase) {
        Err(RollbackBlocked::NotSwitched)
    } else if !old_child_alive {
        Err(RollbackBlocked::OldChildGone)
    } else if nodes_updated > 0 {
        Err(RollbackBlocked::NodesUpdated)
    } else {
        Ok(())
    }
}
