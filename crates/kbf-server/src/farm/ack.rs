//! How the farm answers a worker's `Result`: what its lease stands for now, and the
//! reason a `ResultAck` gives. The lease id is the attempt: a re-grant of an operation
//! gets a new lease id, so a `Result` of an earlier attempt names a lease the farm no
//! longer holds.

use kbf_proto::worker::{self, ResultAck, ResultAckReason as Reason};

use super::{Digest, LeaseId, OpState, OperationId, Stamp, State, WorkerId};

/// What a `Result`'s lease stands for.
#[derive(Clone, Copy, Debug)]
pub(super) enum Standing {
    /// The worker holds the operation's current lease: the operation, the action its
    /// `Start` named, and the server's times for the run.
    Holds(OperationId, Digest, Stamp),
    /// The scheduler finished the operation with this lease's result, and the finish
    /// waits for the state store: the `Result` is acknowledged when it is written.
    Held,
    /// The `Result` is not taken, for this reason.
    Refused(Reason),
}

impl State {
    /// What `lease` stands for as `worker`'s `Result` names it. Only this process's leases
    /// are in the machine's started table, so a lease of an earlier process (another
    /// term) is unknown; one whose operation was answered since is remembered
    /// ([`State::answered`]) for as long as the scheduler keeps the operation.
    pub(super) fn standing(&self, lease: LeaseId, worker: &WorkerId) -> Standing {
        let Some(sent) = self.machine.sent(lease) else {
            return Standing::Refused(if self.answered.contains_key(&lease) {
                Reason::Duplicate
            } else {
                Reason::UnknownLease
            });
        };
        let operation = sent.operation;
        match self.sched.state(operation) {
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
            ) if *held == lease && holder == worker => {
                Standing::Holds(operation, sent.action, sent.stamp)
            }
            Some(OpState::Completed { lease: l, .. } | OpState::Failed { lease: l, .. })
                if *l == lease =>
            {
                if self.finish_held(operation) {
                    Standing::Held
                } else {
                    Standing::Refused(Reason::Duplicate)
                }
            }
            // Given up, granted again (to this node or another), held by another node, or
            // finished with another lease's result: not the current attempt.
            _ => Standing::Refused(Reason::Stale),
        }
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
        Reason::Mismatch => {
            tracing::warn!(%worker, %lease, "result refused: it names another action");
        }
        Reason::Unspecified | Reason::Accepted => {}
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
