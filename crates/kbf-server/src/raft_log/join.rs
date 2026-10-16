//! Joining the log's cell: the leader side of `Join` (docs/design/cluster.md, section
//! 5.3, step 2).
//!
//! A new node asks the leader to add it. The leader checks the cluster id the node
//! expects (the log id), then commits a membership change adding the node as a learner,
//! one change at a time, and answers with the cluster id, the leader, the membership
//! and the index of the entry that set it. A node that is already a member gets the
//! same answer without a new change, so a join retried after a lost answer is harmless.

use kbf_log::FarmLogId;
use kbf_raft::{LogIndex, Membership, ServerId};

/// A node's request to join.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct JoinRequest {
    /// The cluster the node expects: its stored id, or `--cluster-id` on a first join.
    pub cluster: FarmLogId,
    /// The node's Raft id.
    pub node: ServerId,
}

/// The leader's answer to a join.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JoinAnswer {
    /// The cluster id (the log id).
    pub cluster: FarmLogId,
    /// The leader that answered.
    pub leader: ServerId,
    /// The membership, the node included.
    pub membership: Membership,
    /// The index of the entry that set `membership` (0: the membership the log was
    /// created with). Of two answers, the one with the higher index is the newer.
    pub config_index: LogIndex,
}

/// Why a join was refused. Nothing was committed.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum JoinError {
    /// The node expects another cluster: it must not join this one.
    #[error("this is cluster {actual}, but the node expects {expected}")]
    WrongCluster {
        /// The id the node asked for.
        expected: FarmLogId,
        /// This cluster's id.
        actual: FarmLogId,
    },
    /// The leader refused the membership change, or the log could not commit it.
    #[error("the join was refused: {0}")]
    Refused(String),
}

/// A drain or undrain the leader refused. Nothing was committed.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("the membership change was refused: {0}")]
pub struct ChangeRefused(pub String);
