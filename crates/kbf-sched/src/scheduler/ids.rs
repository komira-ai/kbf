//! The lease sequences a scheduler may hand out, and the undoing of a placement.
//!
//! Several schedulers over one state store must not number leases from counters of their
//! own: each is given a block of sequences that no other has (reserved in the store), uses
//! them in order, and places no lease past the block's end until it is given another.

use std::cmp::Reverse;
use std::ops::Range;

use kbf_types::{LeaseId, OperationId};

use super::{OpState, Scheduler};

impl Scheduler {
    /// The sequence the next lease gets.
    #[must_use]
    pub const fn next_seq(&self) -> u64 {
        self.next_seq
    }

    /// How many leases may still be placed before the end of the block of sequences. A
    /// scheduler never given a block has no end.
    #[must_use]
    pub const fn seqs_left(&self) -> u64 {
        self.seq_limit.saturating_sub(self.next_seq)
    }

    /// Hands out lease sequences from the start of `block` on, and none at or past its
    /// end. The sequences of the block the scheduler used before and did not use up are
    /// given up: a sequence is never handed out twice. The caller reserved `block` in the
    /// state store, so no other scheduler of the store uses a sequence in it.
    pub fn use_seq_block(&mut self, block: Range<u64>) {
        self.next_seq = self.next_seq.max(block.start);
        self.seq_limit = block.end;
    }

    /// Undoes the placement of `lease`, which the scheduler granted and whose run has not
    /// started: the operation is queued again, its worker's booking is released, and the
    /// lease is forgotten. Nothing is recorded: no [`crate::Requeue`], no loss against the
    /// operation, no farm fault. The sequence of the lease is not handed out again.
    ///
    /// This is for a scheduler that has lost the claim on the operation to another one,
    /// which runs it. A `Start` already sent for the lease is not recalled: the worker
    /// lists the lease in its next heartbeat, and [`Scheduler::not_held`] names it for the
    /// caller to cancel. A result still to come for the lease is not accepted.
    ///
    /// Returns the operation put back in the queue, or `None` if the scheduler holds no
    /// such lease (never granted here, given up, or finished) or its run has started.
    ///
    /// A method of the scheduler, not an [`crate::Event`], as [`Scheduler::use_seq_block`]
    /// is: the caller learns that the claim is lost from the state store, which no replay
    /// of events shows, and calls it under the lock that serialises its inputs. The farm
    /// calls it when the grant it wrote loses to another frontend's.
    pub fn unplace(&mut self, lease: LeaseId) -> Option<OperationId> {
        let id = self.held.get(&lease)?.operation;
        if !matches!(self.ops[&id].state, OpState::Leased { .. }) {
            return None;
        }
        self.release(id);
        let op = self
            .ops
            .get_mut(&id)
            .expect("held leases name live operations");
        op.committed_lease = op.committed_lease.filter(|committed| *committed != lease);
        op.state = OpState::Queued;
        op.queued_at = self.now;
        self.queue.insert((Reverse(op.request.qos.clone()), id));
        Some(id)
    }
}
