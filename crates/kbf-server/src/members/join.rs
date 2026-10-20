//! Admission of a node whose instance the farm has not seen before (the join of
//! `docs/design/architecture-v2.md`, section 8.4).
//!
//! A node that registers for the first time has an empty store. A new storage node says it
//! is rebuilding (its meta log has not rebuilt the index), then says it is done within
//! seconds; one that said its index is whole would be `Active` at once. Either way, in a
//! farm that holds data it would become `Active`, so rank 1 (the head of the chain and the
//! `FindMissing` target) for its share of the digests that exist, and hold none of them.
//! The transaction that creates `instance/<id>` therefore also writes `unfilled/<id>` when
//! the farm holds data, and a registration is `Joining` while the mark is there, whatever
//! the node says.
//!
//! **Whether the farm holds data.** The store keeps no count of blobs, so the farm is taken
//! to hold data once it has been formed for [`BOOTSTRAP`]: `meta/members_formed` is there
//! and `meta/members_forming`, which the first `Active` member writes with that time to
//! live, is gone. Nodes that start together to make a farm are `Active`; a node that joins
//! a farm older than that is `Joining`, whether or not anything was written meanwhile.
//!
//! **The mark.** `unfilled/<id>` is kept for ever until the member is made `Active`
//! ([`Members::set_state`], the operator's `POST /v1/members/{id}`, which deletes it every
//! time it makes or keeps a member `Active`). While it is there the member's own
//! registration is `Joining` whatever it says, so a restart does not promote it, and a
//! heartbeat does not move a `Joining` member. The registration's write carries a check on
//! the mark's version, so a promotion between its read and its write is not reverted.
//! (`set_state(CatchingUp)` on a joiner, a library call the HTTP route does not make,
//! lets a caught-up beat promote it.) A member that
//! was `Active` and restarts has no mark and is `Active` again, and so is a returning
//! instance the farm has seen before.
//!
//! **What fills a joiner.** It takes writes at once, so new blobs reach it. Nothing copies
//! the blobs that existed before it joined: the promotion is the operator's word that the
//! joiner holds what it owns (the handoff copy is not built).

use std::time::Duration;

use kbf_map::{MemberId, MemberState};
use kbf_state::{Cond, Txn};

use super::{FORMED, Members, MembershipError};

/// How long after its first `Active` member a farm is taken to hold no data.
pub(super) const BOOTSTRAP: Duration = Duration::from_secs(60);

/// Written with the first `Active` member, with [`BOOTSTRAP`] as its time to live.
const FORMING: &str = "meta/members_forming";

/// The prefix of the marks of members that were admitted as joiners and not yet promoted.
const UNFILLED: &str = "unfilled/";

pub(super) fn unfilled_key(id: MemberId) -> String {
    format!("{UNFILLED}{}", id.0)
}

impl Members {
    /// The state a registration of `id` is written with, and the condition on the mark that
    /// the write must carry so that a promotion between this read and the write is not
    /// overwritten: `Joining` while the mark is there; else `CatchingUp` if the node says
    /// it is rebuilding, else `Active`. The mark is written with a new instance
    /// ([`Members::holds_data`]), so what a node says of its index does not decide.
    pub(super) fn admit(
        &self,
        id: MemberId,
        rebuilding: bool,
    ) -> Result<(MemberState, Cond), MembershipError> {
        if let Some(mark) = self.store.get(&unfilled_key(id))? {
            return Ok((MemberState::Joining, Cond::Version(mark.version)));
        }
        let state = if rebuilding {
            MemberState::CatchingUp
        } else {
            MemberState::Active
        };
        Ok((state, Cond::Absent))
    }

    /// Whether the farm was formed longer than [`BOOTSTRAP`] ago.
    pub(super) fn holds_data(&self) -> Result<bool, MembershipError> {
        Ok(self.store.get(FORMED)?.is_some() && self.store.get(FORMING)?.is_none())
    }

    /// `txn` that also says the farm has had an `Active` member: `meta/members_formed`,
    /// and the bootstrap window if this is the first.
    pub(super) fn formed(&self, txn: Txn) -> Result<Txn, MembershipError> {
        let txn = if self.store.get(FORMED)?.is_none() {
            txn.put_ttl(FORMING, Vec::new(), BOOTSTRAP, Cond::Any)
        } else {
            txn
        };
        Ok(txn.put(FORMED, Vec::new(), Cond::Any))
    }

    /// `txn` that also marks the new instance `id` unfilled, if the farm holds data: the
    /// same transaction that creates `instance/<id>`, so no later registration, retry or
    /// race can see the instance without the mark.
    pub(super) fn unfilled_if_data(&self, txn: Txn, id: MemberId) -> Result<Txn, MembershipError> {
        Ok(if self.holds_data()? {
            txn.put(unfilled_key(id), Vec::new(), Cond::Any)
        } else {
            txn
        })
    }
}
