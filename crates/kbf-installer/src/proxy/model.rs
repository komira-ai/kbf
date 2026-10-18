//! The vocabulary of the update API: phases, ids, the order locator, and the JSON bodies
//! of `docs/design/frontend-proxy.md` section 4.

use serde::{Deserialize, Serialize};

use crate::CommitId;

/// The phases of one update, in the order they happen (FP section 4), then the two ways
/// it can end without finishing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Accepted,
    Fetching,
    Verifying,
    Staged,
    Preflight,
    Starting,
    Ready,
    Holding,
    Quiescing,
    Switching,
    Switched,
    Baking,
    Nodes,
    Draining,
    Done,
    Failed,
    RolledBack,
}

impl Phase {
    /// The update still occupies the one slot: a second `POST` gets 409.
    pub fn is_open(self) -> bool {
        !matches!(self, Phase::Done | Phase::Failed | Phase::RolledBack)
    }

    /// `abort` is allowed before the flip (FP: "before switching").
    pub fn abortable(self) -> bool {
        self < Phase::Switching
    }

    /// The phases in which the new child is the active one and the update is not over.
    fn switched(self) -> bool {
        (Phase::Switched..=Phase::Draining).contains(&self)
    }
}

/// An update's id, assigned by the API and stable across a retried request.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct UpdateId(pub String);

/// The verbs of the orders the proxy writes into `orders/` (KI section 4).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OrderVerb {
    FrontendUpdate,
    Confirm,
    Revert,
}

/// The record the proxy writes: a locator and nothing else (FP section 2). The ids come
/// from the [`crate::proxy::traits::OrderWriter`], which owns their counter.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Locator {
    pub order_id: u64,
    pub epoch: u64,
    pub generation: u64,
    pub verb: OrderVerb,
    pub commit: CommitId,
    pub window_s: u32,
}

/// What a staged release says about itself, as `status.json` reports it under `staged{}`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StagedFacts {
    pub protocol: u32,
    pub min_protocol: u32,
    pub state_schema: u32,
    #[serde(default)]
    pub profile: Option<String>,
    #[serde(default)]
    pub platform: Option<String>,
}

/// Which of the two child slots is active.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Slot {
    A,
    B,
}

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

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Failure {
    pub step: String,
    pub reason: String,
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

/// Whether rollback is allowed, from the facts the supervisor reports. The API applies it
/// itself, so a supervisor that forgot a rule cannot roll back past a node update.
pub fn rollback_gate(
    phase: Phase,
    old_child_alive: bool,
    nodes_updated: u32,
) -> Result<(), RollbackBlocked> {
    if !phase.is_open() {
        Err(RollbackBlocked::NoUpdate)
    } else if !phase.switched() {
        Err(RollbackBlocked::NotSwitched)
    } else if !old_child_alive {
        Err(RollbackBlocked::OldChildGone)
    } else if nodes_updated > 0 {
        Err(RollbackBlocked::NodesUpdated)
    } else {
        Ok(())
    }
}
