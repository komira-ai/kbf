//! Admission of a node whose instance the farm has not seen before (the join of
//! `docs/design/architecture-v2.md`, section 8.4).
//!
//! A node that registers for the first time says its index is whole, since there is nothing
//! to rebuild, but its store is empty. In a farm that holds data it would be `Active`, so
//! rank 1 (the head of the chain and the `FindMissing` target) for its share of the digests
//! that exist, and hold none of them. [`Members::register`] admits it as `Joining` instead,
//! and writes `unfilled/<id>` in the same transaction.
//!
//! **Whether the farm holds data.** The store keeps no count of blobs, so the farm is taken
//! to hold data once it has been formed for [`BOOTSTRAP`]: `meta/members_formed` is there
//! and `meta/members_forming`, which the first `Active` member writes with that time to
//! live, is gone. Nodes that start together to make a farm are `Active`; a node that joins
//! a farm older than that is `Joining`, whether or not anything was written meanwhile.
//!
//! **The mark.** `unfilled/<id>` is kept for ever until the member is made `Active`
//! ([`Members::set_state`], the operator's `POST /v1/members/{id}`). While it is there the
//! member's own registration is `Joining` whatever it says, so a restart does not promote
//! it, and a heartbeat promotes nothing (it never moves a `Joining` member). A member that
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
    /// The state a registration of `id` is written with: `Joining` while its mark is
    /// there, or for a first-seen instance in a farm that holds data; else what the node
    /// says of its index.
    pub(super) fn admit(
        &self,
        id: MemberId,
        fresh: bool,
        rebuilding: bool,
    ) -> Result<MemberState, MembershipError> {
        if self.store.get(&unfilled_key(id))?.is_some() {
            return Ok(MemberState::Joining);
        }
        if rebuilding {
            return Ok(MemberState::CatchingUp);
        }
        if fresh && self.holds_data()? {
            return Ok(MemberState::Joining);
        }
        Ok(MemberState::Active)
    }

    /// Whether the farm was formed longer than [`BOOTSTRAP`] ago.
    fn holds_data(&self) -> Result<bool, MembershipError> {
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
}
