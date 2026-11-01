//! How the farm answers a worker's `Result`: what its lease stands for now, and the
//! reason a `ResultAck` gives. The lease id is the attempt: a re-grant of an operation
//! gets a new lease id, so a `Result` of an earlier attempt names a lease the farm no
//! longer holds.

use kbf_proto::worker::{self, ResultAck, ResultAckReason as Reason};

use super::{Digest, LeaseId, OpState, OperationId, Stamp, State, WorkerId};

/// Why a `Result` is not taken now.
#[derive(Clone, Copy, Debug)]
pub(super) enum Verdict {
    /// The scheduler finished the operation with this lease's result, and the finish
    /// waits for the state store: the `Result` is acknowledged when it is written.
    Held,
    /// The `Result` is refused, for this reason.
    Refused(Reason),
}

/// The lease whose result finished an operation in `state`, if it is finished.
const fn finished_lease(state: &OpState) -> Option<LeaseId> {
    match state {
        OpState::Completed { lease, .. } | OpState::Failed { lease, .. } => Some(*lease),
        _ => None,
    }
}

impl State {
    /// What `lease` stands for as `worker`'s `Result` names it: the operation, the action
    /// its `Start` named and the server's times for the run, if `worker` holds the
    /// operation's current lease; else why the `Result` is not taken. Only this process's
    /// leases are in the machine's started table, so a lease of an earlier process (another
    /// term) is unknown; one whose operation was answered since is remembered
    /// ([`State::answered`]) for as long as the scheduler keeps the operation.
    ///
    /// # Errors
    /// The [`Verdict`] on the `Result`.
    pub(super) fn standing(
        &self,
        lease: LeaseId,
        worker: &WorkerId,
    ) -> Result<(OperationId, Digest, Stamp), Verdict> {
        let Some(sent) = self.side.machine.sent(lease) else {
            return Err(Verdict::Refused(
                if self.engine.answered.contains_key(&lease) {
                    Reason::Duplicate
                } else {
                    Reason::UnknownLease
                },
            ));
        };
        let operation = sent.operation;
        let state = self.engine.sched.state(operation);
        // One body for both states: a guard on the alternatives would be two copies of it.
        let current = match state {
            Some(
                OpState::Leased {
                    lease: held,
                    worker: holder,
                    committed: true,
                }
                | OpState::Running {
                    lease: held,
                    worker: holder,
                },
            ) => *held == lease && holder == worker,
            _ => false,
        };
        if current {
            return Ok((operation, sent.action, sent.stamp));
        }
        // Given up, granted again (to this node or another), held by another node, or
        // finished with another lease's result: not the current attempt.
        if state.and_then(finished_lease) != Some(lease) {
            return Err(Verdict::Refused(Reason::Stale));
        }
        Err(if self.engine.finish_held(operation) {
            Verdict::Held
        } else {
            Verdict::Refused(Reason::Duplicate)
        })
    }
}

/// Logs why a `Result` of `lease` from `worker` is refused, one line for each reason.
pub(super) fn log_refusal(worker: &WorkerId, lease: LeaseId, reason: Reason) {
    match reason {
        Reason::Duplicate => {
            tracing::info!(%worker, %lease, "result is a copy: its operation is answered");
        }
        Reason::Stale => {
            tracing::warn!(%worker, %lease, "result refused: stale attempt, the lease was given up or granted again, or another node holds it");
        }
        Reason::UnknownLease => {
            tracing::warn!(%worker, %lease, "result refused: unknown lease, or one of another epoch");
        }
        _ => tracing::warn!(%worker, %lease, "result refused: it names another action"),
    }
}

/// The acknowledgement of a refused `Result` of `lease`: not accepted, for `reason`.
pub(super) fn refusal(lease: worker::LeaseId, reason: Reason) -> ResultAck {
    ResultAck {
        lease_id: Some(lease),
        accepted: false,
        reason: reason as i32,
    }
}
