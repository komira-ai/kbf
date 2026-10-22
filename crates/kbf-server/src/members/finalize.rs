//! The farm's finalize target (`kbf cluster finalize`): the metadata-log machine version the
//! operator asked every storage node to raise its log to, and the report of where each
//! member stands against it.
//!
//! **The key.** `meta/finalize` holds the target `T`, four bytes, big endian; absent means
//! nothing was asked. It only rises: [`Members::raise_finalize`] writes `T` with a
//! compare-and-set on the version it read, and the same or a lower `T` writes nothing, so a
//! second operator, a retry, or a frontend of an older build asking for less can never take
//! the target back. A raise of a node's log is one-way, and so is this.
//!
//! **Who reads it.** Every answer to a registration and a heartbeat carries the key as
//! `finalize_to` (`members_rpc`), so a node learns the target from its own beat, and a node
//! restarted later learns it when it registers. What a node does with it is its own
//! (`crate::finalize`).
//!
//! **The report.** [`Members::finalize_report`] judges every member of the map and every live
//! row against a target `T` ([`FinalizeState`]): `done` when its log is at `T` or past it,
//! `pending` when its build runs `T` and its log is below, `blocked` when its build runs
//! less than `T` or it has no live row (it is `Down`, or its row lapsed), and `unknown` when
//! the row says nothing of the build or the log (an older build, or a node with no log).
//! `blocked` and `unknown` keep `POST /v1/finalize` from writing the key. The report reads
//! the map and the rows one after the other, not as one snapshot.

use std::collections::BTreeSet;

use kbf_map::{Map, MemberId};
use kbf_state::{Cond, Txn};
use serde::{Deserialize, Serialize};

use super::facts::NodeFacts;
use super::{ATTEMPTS, Members, MembershipError};

/// The key of the target: four bytes, big endian; absent is "nothing asked".
pub(super) const FINALIZE: &str = "meta/finalize";

/// Where one member stands against a target: see the module docs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FinalizeState {
    /// Its log is at the target or past it.
    Done,
    /// Its build runs the target and its log is below it.
    Pending,
    /// Its build runs less than the target, or it has no live row.
    Blocked,
    /// The row says nothing of the build or the log.
    Unknown,
}

impl FinalizeState {
    /// The state as the API spells it: `done`, `pending`, `blocked` or `unknown`.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Done => "done",
            Self::Pending => "pending",
            Self::Blocked => "blocked",
            Self::Unknown => "unknown",
        }
    }

    /// Whether a member in this state keeps the target from being written.
    #[must_use]
    pub const fn blocks(self) -> bool {
        matches!(self, Self::Blocked | Self::Unknown)
    }
}

/// One member's line of the report: what its row says and the verdict on it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FinalizeNode {
    /// The member's id.
    pub member: u64,
    /// The commit its binary was built from; `None` if its row names none or has lapsed.
    pub build_commit: Option<String>,
    /// The newest log version its build can raise the log to; `None` as for `build_commit`.
    pub runs_log: Option<u32>,
    /// The log version it reported last; `None` as for `build_commit`.
    pub log_version: Option<u32>,
    /// The verdict.
    pub state: FinalizeState,
    /// Why, when it is `blocked` or `unknown`; empty otherwise.
    pub reason: String,
}

/// The verdict on a member whose live row says `facts` (`None`: it has no live row), for a
/// finalize to `target`, and the reason when it blocks.
fn judge(facts: Option<&NodeFacts>, target: u32) -> (FinalizeState, String) {
    let Some(facts) = facts else {
        let why = "it has no live row: it is down or its row lapsed; restart it";
        return (FinalizeState::Blocked, why.to_owned());
    };
    let (Some(runs), Some(at)) = (facts.runs_log, facts.log_version) else {
        let why = "it reports no runs_log or log_version: an older build, or no log";
        return (FinalizeState::Unknown, why.to_owned());
    };
    if at >= target {
        (FinalizeState::Done, String::new())
    } else if runs < target {
        let why = format!("its build runs log versions up to {runs}, below {target}");
        (FinalizeState::Blocked, why)
    } else {
        (FinalizeState::Pending, String::new())
    }
}

impl Members {
    /// The target the operator asked the farm's logs to be raised to; `None` before anyone did.
    ///
    /// # Errors
    /// The store failing; a stored target that is not four bytes.
    pub fn finalize_target(&self) -> Result<Option<u32>, MembershipError> {
        Ok(self.finalize_cond()?.0)
    }

    /// The target and the condition a write of a higher one must carry.
    fn finalize_cond(&self) -> Result<(Option<u32>, Cond), MembershipError> {
        match self.store.get(FINALIZE)? {
            None => Ok((None, Cond::Absent)),
            Some(stored) => {
                let bytes = <[u8; 4]>::try_from(stored.value.as_slice())
                    .map_err(|_| MembershipError::Corrupt("finalize is not a u32".to_owned()))?;
                let held = u32::from_be_bytes(bytes);
                Ok((Some(held), Cond::Version(stored.version)))
            }
        }
    }

    /// Raises the farm's finalize target to `target`; whether it wrote. A target at or below
    /// the one held is left as it is (`false`), and so is the same one: the key only rises.
    ///
    /// # Errors
    /// The store failing; a stored target that is not four bytes; [`MembershipError::Contended`]
    /// if other writers keep winning the compare-and-set.
    pub fn raise_finalize(&self, target: u32) -> Result<bool, MembershipError> {
        for _ in 0..ATTEMPTS {
            let (held, cond) = self.finalize_cond()?;
            if held.is_some_and(|held| held >= target) {
                return Ok(false);
            }
            let value = target.to_be_bytes().to_vec();
            let txn = Txn::new().put(FINALIZE, value, cond);
            if self.store.txn(txn)?.is_ok() {
                return Ok(true);
            }
        }
        Err(MembershipError::Contended)
    }

    /// Where every member stands against a finalize to `target`, in member order: each member
    /// of the map, and each live row the map does not hold yet.
    ///
    /// # Errors
    /// The store failing; a stored row or map not decoding.
    pub fn finalize_report(&self, target: u32) -> Result<Vec<FinalizeNode>, MembershipError> {
        let map = self.current_map()?;
        let rows = self.read_rows()?;
        let in_map = map.as_ref().map_or(&[][..], Map::members).iter();
        let ids: BTreeSet<MemberId> = rows.keys().copied().chain(in_map.map(|m| m.id)).collect();
        Ok(ids
            .into_iter()
            .map(|id| {
                let facts = rows.get(&id).map(|live| &live.facts);
                let (state, reason) = judge(facts, target);
                let said = facts.cloned().unwrap_or_default();
                FinalizeNode {
                    member: id.0,
                    build_commit: said.build_commit,
                    runs_log: said.runs_log,
                    log_version: said.log_version,
                    state,
                    reason,
                }
            })
            .collect())
    }
}
