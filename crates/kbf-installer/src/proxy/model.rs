//! The values the swap orchestrator reads and writes: slots, phases, what a child reports
//! about itself, the plan of one update and the intent record kept in `update.json`.

use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::CommitId;

/// One of the two child positions of a frontend host. The proxy keeps its children at
/// fixed loopback offsets per slot; a slot holds at most one process.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Slot {
    A,
    B,
}

impl Slot {
    pub fn other(self) -> Slot {
        match self {
            Slot::A => Slot::B,
            Slot::B => Slot::A,
        }
    }
}

/// Where an update is (FP section 4). The order of the variants is the order of a
/// successful update, so `phase <= Phase::Baking` reads "not past bake". The phases before
/// `Staged` (accepted, fetching, verifying) belong to the API and the root half (U8).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
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
    pub fn is_terminal(self) -> bool {
        matches!(self, Phase::Done | Phase::Failed | Phase::RolledBack)
    }
}

/// The step a failed update names (`failure.step` in `GET /v1/update/frontend`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Step {
    Stage,
    Migrate,
    Start,
    Ready,
    Hold,
    Switch,
    Abort,
    Rollback,
    Restart,
    Confirm,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Failure {
    pub step: Step,
    pub reason: String,
}

/// How the host swaps: a frontend runs two children side by side (FP section 5); an
/// all-in-one host stops the old process and starts the new one (FP section 5a).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    Frontend,
    AllInOne,
}

/// A process, named so that a recycled pid cannot be mistaken for it: the pid, the start
/// time the kernel reports for it and the executable it runs. Adoption compares all three.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcIdent {
    pub pid: u32,
    pub start_time: u64,
    pub exe: PathBuf,
}

/// Why a recorded process is not the live one. Either way it is not ours to signal.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum AdoptError {
    #[error("process {0} is gone")]
    Gone(u32),
    #[error("pid {pid} is a different process now (start time or executable differs)")]
    Recycled { pid: u32 },
}

/// The one rule of adoption: the live process is the recorded one only if pid, start
/// time and executable all agree. The real `Child` and the fake both call this.
pub fn verify_ident(recorded: &ProcIdent, live: &ProcIdent) -> Result<(), AdoptError> {
    if recorded.pid == live.pid
        && recorded.start_time == live.start_time
        && recorded.exe == live.exe
    {
        Ok(())
    } else {
        Err(AdoptError::Recycled { pid: recorded.pid })
    }
}

/// What a child says about itself on `GET role`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RoleState {
    /// Started with `--await-takeover`: serves nothing, holds nothing.
    Candidate,
    Active,
    Quiescing,
    Quiesced,
    StoodDown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RoleInfo {
    /// This child holds the store's generation. The store wins every disagreement.
    pub holder: bool,
    pub state: RoleState,
}

/// Readiness of a candidate: `/readyz`, a `grpc.health.v1` check and one synthetic
/// `GetCapabilities` through it (FP section 5 step 5). All three must hold.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Readiness {
    pub readyz: bool,
    pub health: bool,
    pub capabilities: bool,
}

impl Readiness {
    pub fn all(self) -> bool {
        self.readyz && self.health && self.capabilities
    }
}

/// What the child itself counts (Q8). Streams are counted by the proxy's permits, not here.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OpenCounts {
    pub open_executions: u64,
    pub queued_operations: u64,
}

impl OpenCounts {
    pub fn idle(self) -> bool {
        self.open_executions == 0 && self.queued_operations == 0
    }
}

/// The facts the root half publishes under `staged{}` in `status.json` after it fetched,
/// verified and probed a `frontend_update` commit without flipping.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StagedFacts {
    pub commit: CommitId,
    pub protocol: u32,
    pub min_protocol: u32,
    pub state_schema: u32,
    pub profile: String,
}

/// The `frontend_update` order (Q1): a locator only. Nothing in it is a path or a URL.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Locator {
    pub order_id: u64,
    pub epoch: u64,
    pub generation: u64,
    pub commit: CommitId,
    pub window_s: u32,
}

/// An operator request that arrives while an update runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Request {
    Abort,
    Rollback,
}

/// Every limit the swap uses. The defaults are the numbers of FP sections 5 and 5a.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Timing {
    /// An exit of the candidate inside this window earns one respawn.
    pub early_exit: Duration,
    /// A candidate not ready this long after its first spawn is killed.
    pub ready_within: Duration,
    pub hold_max: Duration,
    pub quiesce: Duration,
    pub takeover: Duration,
    pub bake: Duration,
    /// How often a long wait (bake, drain, a candidate coming up) looks again.
    pub poll: Duration,
    /// How often a short wait (quiesce, takeover resolving) looks again.
    pub tick: Duration,
}

impl Timing {
    pub fn frontend() -> Timing {
        Timing {
            early_exit: Duration::from_secs(30),
            ready_within: Duration::from_secs(120),
            hold_max: Duration::from_secs(15),
            quiesce: Duration::from_secs(3),
            takeover: Duration::from_secs(10),
            bake: Duration::from_secs(300),
            poll: Duration::from_secs(1),
            tick: Duration::from_millis(100),
        }
    }

    pub fn all_in_one() -> Timing {
        Timing {
            hold_max: Duration::from_secs(300),
            ..Timing::frontend()
        }
    }
}

/// One update, as the API accepted it.
#[derive(Clone, Debug)]
pub struct Plan {
    pub update_id: String,
    pub request_id: String,
    pub mode: Mode,
    pub from: CommitId,
    pub to: CommitId,
    /// Update the nodes after the bake (FP section 5 step 9).
    pub nodes: bool,
    /// The downgrade flag of the API; recorded, not acted on here.
    pub rollback: bool,
    /// The slot that serves now.
    pub old_slot: Slot,
    pub timing: Timing,
}

impl Plan {
    /// The plan of an update found in `update.json` after a restart; the limits come
    /// from configuration again.
    pub fn resumed(intent: &Intent, timing: Timing) -> Plan {
        Plan {
            update_id: intent.update_id.clone(),
            request_id: intent.request_id.clone(),
            mode: intent.mode,
            from: intent.from.clone(),
            to: intent.to.clone(),
            nodes: intent.nodes,
            rollback: intent.rollback,
            old_slot: intent.old_slot,
            timing,
        }
    }

    pub fn new_slot(&self) -> Slot {
        self.old_slot.other()
    }
}

/// The intent record, `update.json` in the proxy state directory (Q5). It holds what the
/// proxy decided, never a fact the store or the root half owns: after a restart the
/// generation holder and `status.json` win any disagreement with it. It is written before
/// each action, so a crash resumes at the phase it names.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Intent {
    pub update_id: String,
    pub request_id: String,
    pub mode: Mode,
    pub from: CommitId,
    pub to: CommitId,
    pub nodes: bool,
    pub rollback: bool,
    pub phase: Phase,
    pub phase_started: u64,
    pub bake_started: Option<u64>,
    pub nodes_posted: bool,
    pub old_slot: Slot,
    pub new_slot: Slot,
    /// The old child's process, adopted after a restart.
    pub old_proc: Option<ProcIdent>,
    /// The new child's process; set when it is spawned.
    pub new_proc: Option<ProcIdent>,
    /// The candidate was respawned once already.
    pub respawned: bool,
    /// The slot being retired (the old one, or the new one after a rollback).
    pub retire: Option<Slot>,
    /// The update ended in a rollback; `Draining` then ends in `RolledBack`, not `Done`.
    pub rolling_back: bool,
    pub failure: Option<Failure>,
}

impl Intent {
    pub fn begin(plan: &Plan, old_proc: Option<ProcIdent>, now: u64) -> Intent {
        Intent {
            update_id: plan.update_id.clone(),
            request_id: plan.request_id.clone(),
            mode: plan.mode,
            from: plan.from.clone(),
            to: plan.to.clone(),
            nodes: plan.nodes,
            rollback: plan.rollback,
            phase: Phase::Staged,
            phase_started: now,
            bake_started: None,
            nodes_posted: false,
            old_slot: plan.old_slot,
            new_slot: plan.new_slot(),
            old_proc,
            new_proc: None,
            respawned: false,
            retire: None,
            rolling_back: false,
            failure: None,
        }
    }
}

/// Why the orchestrator stopped without finishing: the proxy is going away (or, in a test,
/// died at a crash point). Nothing more is done; the next start resumes from `update.json`.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Halt {
    #[error("the proxy crashed at {0}")]
    Crashed(String),
    #[error("the proxy is shutting down")]
    Interrupted,
    #[error("a port the swap cannot do without failed: {0}")]
    Port(String),
}

/// The three crash points the U7 proof names (FP section 10), where a test makes the
/// proxy die. The other FP section 6 rows are phase boundaries: the recorder crashes
/// after it persisted a phase.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CrashPoint {
    /// The takeover call was sent; its answer is not read yet. It may or may not have landed.
    BeforeTakeoverReturns,
    /// Takeover succeeded; the active slot is not swapped and `switched` is not recorded.
    AfterTakeoverBeforeSwap,
    /// The old child was told to stand down and the retire wait starts.
    WhileDraining,
}

/// A port call that failed, with the reason for the failure record.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct PortError(pub String);
