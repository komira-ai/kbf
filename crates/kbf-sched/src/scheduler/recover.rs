//! Taking up again the operations a state store holds: a scheduler rebuilt after its
//! process ended, or taken over by another.

use kbf_types::{LeaseId, OperationId, WaiterId, WorkerId};

use super::Scheduler;
use crate::input::Request;

/// An unfinished operation as the state store holds it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Recovered {
    /// The operation's id, as it was granted.
    pub operation: OperationId,
    /// What it runs.
    pub request: Request,
    /// The one caller the scheduler holds it for: the callers it had are gone with the
    /// process that held them, and a client that asks for the action again joins it.
    pub waiter: WaiterId,
    /// The committed lease it holds, if one was granted and not given up.
    pub lease: Option<RecoveredLease>,
}

/// A committed lease of a [`Recovered`] operation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecoveredLease {
    /// The lease.
    pub lease: LeaseId,
    /// The worker it was granted to.
    pub worker: WorkerId,
}

impl Scheduler {
    /// Takes `operations` up in an empty scheduler, which grants leases under `term` from
    /// `next_seq` and operation ids from `next_operation` on.
    pub fn recover(
        &mut self,
        term: u64,
        next_operation: OperationId,
        next_seq: u64,
        operations: Vec<Recovered>,
    ) {
        // Not implemented yet: the tests of this change fail against this.
        let _ = (term, next_operation, next_seq, operations);
    }
}
