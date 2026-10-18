//! The files the migration writes in the new tree: `state.json` (schema 2) and
//! `status.json`, whose shapes are the golden fixtures of contract P1, and its own
//! journal.
//!
//! [`NewState`] and [`NewStatus`] write exactly the keys of the golden fixtures and fill
//! only the fields a migrated host has; the fields a migration never sets (a swap on
//! trial, an unacknowledged order, the serving instance, the refusal) are opaque JSON that
//! is always `null`. They stand in for `state_v2` and `status_v2` of P1/P2 and are
//! replaced by them (one `use` and the constructors below) when those land.

use serde::{Deserialize, Serialize};

use super::units::Unit;
use crate::CommitId;

/// The schema number of `state.json` and `status.json`.
pub const SCHEMA: u64 = 2;

/// The highest order acted on, as a pair for the (epoch, order id) rule and a generation
/// for the (epoch, generation) rule.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Floor {
    pub epoch: u64,
    pub order_id: u64,
    pub generation: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewState {
    pub schema: u64,
    pub current: Option<CommitId>,
    pub previous: Option<CommitId>,
    pub staged: Option<CommitId>,
    pub pending_confirm: Option<serde_json::Value>,
    pub floor: Floor,
    pub serving_instance: Option<serde_json::Value>,
    pub last_acked_order_id: u64,
    pub unacked: Option<serde_json::Value>,
}

impl NewState {
    pub fn migrated(
        current: Option<CommitId>,
        previous: Option<CommitId>,
        floor: Floor,
    ) -> NewState {
        NewState {
            schema: SCHEMA,
            current,
            previous,
            staged: None,
            pending_confirm: None,
            floor,
            serving_instance: None,
            last_acked_order_id: floor.order_id,
            unacked: None,
        }
    }

    pub fn parse(bytes: &[u8]) -> Result<NewState, String> {
        let state: NewState = serde_json::from_slice(bytes).map_err(|e| e.to_string())?;
        match state.schema {
            SCHEMA => Ok(state),
            other => Err(format!("schema {other}, not {SCHEMA}")),
        }
    }

    pub fn render(&self) -> Vec<u8> {
        render(self)
    }
}

/// What a release says about itself, as the golden `status.json` has it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProbeFacts {
    pub commit: CommitId,
    pub platform: String,
    pub protocol: u64,
    pub min_protocol: u64,
    pub state_schema: u64,
    pub profile: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewStatus {
    pub schema: u64,
    pub installed_commit: Option<CommitId>,
    pub previous_commit: Option<CommitId>,
    pub staged_commit: Option<CommitId>,
    pub state: String,
    pub refusal: Option<serde_json::Value>,
    pub acked_order_id: u64,
    pub staged: Option<serde_json::Value>,
    pub probe: Option<ProbeFacts>,
    pub trust_days_left: Option<i64>,
    pub unverified: Vec<CommitId>,
}

impl NewStatus {
    /// The status of a migrated host: idle, with the releases it adopted without
    /// verification named so that nothing reading it takes them for verified.
    pub fn migrated(
        state: &NewState,
        probe: Option<ProbeFacts>,
        unverified: Vec<CommitId>,
    ) -> NewStatus {
        NewStatus {
            schema: SCHEMA,
            installed_commit: state.current.clone(),
            previous_commit: state.previous.clone(),
            staged_commit: None,
            state: "idle".to_owned(),
            refusal: None,
            acked_order_id: state.last_acked_order_id,
            staged: None,
            probe,
            trust_days_left: None,
            unverified,
        }
    }

    pub fn render(&self) -> Vec<u8> {
        render(self)
    }
}

fn render<T: Serialize>(value: &T) -> Vec<u8> {
    let mut bytes = serde_json::to_vec_pretty(value).expect("plain fields serialize");
    bytes.push(b'\n');
    bytes
}

/// How far the migration got.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    /// Begun; every step may be repeated.
    Started,
    /// Finished: the host is the installer's.
    Done,
    /// Undone by `rollback`: the host is the old layout's again, and `setup` refuses
    /// until it is migrated.
    RolledBack,
}

/// A release the migration brought over.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Adopted {
    /// `current` or `previous`.
    pub role: String,
    pub commit: CommitId,
    /// Why it carries no `VERIFIED` marker; `None` for a release the verify pipeline
    /// installed.
    pub unverified: Option<String>,
}

/// `migration.json`, in the new state directory: what was decided and what is owed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Journal {
    pub schema: u64,
    pub phase: Phase,
    /// The units repointed at the new tree.
    pub units: Vec<Unit>,
    /// The old helper's unit, stopped and disabled so only one tool manages the host.
    pub helper_unit: Option<Unit>,
    /// The units the migration stopped, which it starts again at its end (and a
    /// rollback starts again at its own).
    pub stopped: Vec<Unit>,
    pub adopted: Vec<Adopted>,
}

impl Journal {
    pub fn new(units: Vec<Unit>, helper_unit: Option<Unit>) -> Journal {
        Journal {
            schema: 1,
            phase: Phase::Started,
            units,
            helper_unit,
            stopped: Vec::new(),
            adopted: Vec::new(),
        }
    }

    pub fn parse(bytes: &[u8]) -> Result<Journal, String> {
        serde_json::from_slice(bytes).map_err(|e| e.to_string())
    }

    pub fn render(&self) -> Vec<u8> {
        render(self)
    }
}
