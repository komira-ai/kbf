//! Membership changes, one server at a time (single-server changes, Ongaro's
//! dissertation, section 4.1).
//!
//! A change is a [`Payload::Membership`](crate::Payload::Membership) entry naming the
//! whole new membership. Every server uses the latest membership in its log as soon as
//! it holds the entry, committed or not, and goes back to the one before if the entry is
//! truncated away. Any two memberships one server apart have overlapping majorities, so
//! the old and the new configuration cannot elect two leaders in one term, provided a
//! leader starts the next change only after the last one is committed, and only once it
//! has committed an entry of its own term (the dissertation's correction to the
//! single-server algorithm). [`Raft::change_membership`](crate::Raft::change_membership)
//! refuses anything else; there is no joint consensus.

use thiserror::Error;

use crate::{LogIndex, Membership, NotLeader, ServerId};

/// One server's change of role.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Change {
    /// Adds a server that is not a member, as a learner: it receives and applies the log
    /// but never votes or campaigns, so it can catch up before it counts.
    AddLearner(ServerId),
    /// Makes a learner a voter.
    Promote(ServerId),
    /// Removes a voter or a learner. A leader that removes itself keeps leading until
    /// the change commits, without counting its own copy, then steps down.
    Remove(ServerId),
}

/// A membership change the leader refused. Nothing was appended.
#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
pub enum ChangeError {
    /// Only the leader changes the membership.
    #[error(transparent)]
    NotLeader(NotLeader),
    /// The previous change, at `pending`, is not committed yet: one change at a time.
    #[error("the membership change at {pending:?} is not committed yet; one change at a time")]
    InProgress {
        /// The index of the uncommitted membership entry.
        pending: LogIndex,
    },
    /// The leader has not yet committed an entry of its own term, so a change of an
    /// earlier leader may still be uncommitted somewhere.
    #[error("the leader has not committed an entry of its term yet")]
    NotReady,
    /// The server to add is already a voter or a learner.
    #[error("{0} is already a member")]
    AlreadyMember(ServerId),
    /// Only a learner can be promoted.
    #[error("{0} is not a learner")]
    NotALearner(ServerId),
    /// The server to remove is not a member.
    #[error("{0} is not a member")]
    NotAMember(ServerId),
    /// A group needs a voter: the last one cannot be removed.
    #[error("{0} is the last voter")]
    LastVoter(ServerId),
}

impl Membership {
    /// The membership after `change`, one server apart from this one.
    ///
    /// # Errors
    ///
    /// [`ChangeError::AlreadyMember`], [`ChangeError::NotALearner`],
    /// [`ChangeError::NotAMember`] or [`ChangeError::LastVoter`] for a change that does
    /// not apply to this membership.
    pub fn changed(&self, change: Change) -> Result<Self, ChangeError> {
        let mut next = self.clone();
        match change {
            Change::AddLearner(id) => {
                if self.contains(id) {
                    return Err(ChangeError::AlreadyMember(id));
                }
                next.learners.insert(id);
            }
            Change::Promote(id) => {
                if !next.learners.remove(&id) {
                    return Err(ChangeError::NotALearner(id));
                }
                next.voters.insert(id);
            }
            Change::Remove(id) => {
                if !self.contains(id) {
                    return Err(ChangeError::NotAMember(id));
                }
                if self.voters.len() == 1 && self.is_voter(id) {
                    return Err(ChangeError::LastVoter(id));
                }
                next.voters.remove(&id);
                next.learners.remove(&id);
            }
        }
        Ok(next)
    }
}
