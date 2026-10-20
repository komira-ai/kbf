//! `migration.json`: what the migration decided and what it still owes. It is the
//! migration's own file; `state.json` and `status.json` are the installer's
//! ([`crate::state`], [`crate::helper::status`]) and are written through their own types.

use serde::{Deserialize, Serialize};

use super::units::Unit;
use crate::CommitId;

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

fn render<T: Serialize>(value: &T) -> Vec<u8> {
    let mut bytes = serde_json::to_vec_pretty(value).expect("plain fields serialize");
    bytes.push(b'\n');
    bytes
}
