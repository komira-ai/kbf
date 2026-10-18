//! What the helper remembers between cycles, in `helper.json` beside `state.json`
//! (root-owned: the order spool is the service user's, so nothing the helper must trust is
//! kept there).

use crate::CommitId;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// What the last thing the helper did came to, as `status.json` reports it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Event {
    #[default]
    Idle,
    Confirmed,
    Reverted,
    Refused,
}

/// What a fetched release says about itself, for the frontend's comparison with its
/// preflight.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StagedFacts {
    pub commit: CommitId,
    pub protocol: u32,
    pub min_protocol: u32,
    pub state_schema: u32,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Memory {
    /// The newest order acted on (acted on includes refused).
    pub acked_order_id: u64,
    /// The epoch of that order: a frontend's `Welcome` epoch, which a restored store
    /// raises. An order is newer than the last when its epoch is higher, or the same and
    /// its id is higher; ids come from one counter in the store, not from this host, so
    /// no gap between them is a reason to refuse.
    pub acked_epoch: u64,
    /// The same pair for `frontend_update` records. Each kind has its own floor: a frontend
    /// order never advances or blocks the node's, and the reverse.
    pub frontend_order_id: u64,
    pub frontend_epoch: u64,
    pub event: Event,
    /// Why the last order was refused, or what was reverted.
    pub refusal: String,
    /// When root first saw each record now in the spool, in seconds. A record expires by
    /// this, never by the file's modification time, which its writer sets.
    pub first_seen: BTreeMap<String, u64>,
    /// The unit must be restarted: the links moved and the restart is not done. Written
    /// before the restart, so a crash between the two restarts on the next cycle.
    pub restart_due: bool,
    pub staged: Option<StagedFacts>,
    /// A fetch is in progress (only while a pass runs; never stored, so a crash cannot
    /// leave it set).
    #[serde(skip)]
    pub fetching: bool,
}
