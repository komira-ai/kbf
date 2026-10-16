//! Taking up again the operations a state store holds: a scheduler rebuilt after its
//! process ended, or taken over by another.

use std::cmp::Reverse;

use kbf_types::{LeaseId, LeaseKind, OperationId, WaiterId, WorkerId};

use super::{Held, OpState, Operation, Scheduler, StartSent, Worker};
use crate::fence::LEASE_GRACE;
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
    /// Takes `operations` up in an empty scheduler, which grants leases under `term`
    /// from `next_seq` and operation ids from `next_operation` on (both past every id
    /// the store ever named, finished operations' included, so no id names two).
    ///
    /// An operation with no lease is queued. One with a lease holds it on its worker,
    /// which has not registered with this scheduler yet: for [`LEASE_GRACE`] the lease
    /// is neither given up for silence nor counted against the worker, and when the
    /// worker registers it is booked there as a lease whose `Start` went to an earlier
    /// session of the same daemon process. The worker's first heartbeat then decides: a
    /// lease it lists is kept (the run is re-bound), one it leaves out, which includes a
    /// grant that was committed but whose `Start` never reached it, goes back to the
    /// queue at once, charged to no budget. A worker that does not register in the grace
    /// has its leases given up as a silent worker's are.
    pub fn recover(
        &mut self,
        term: u64,
        next_operation: OperationId,
        next_seq: u64,
        operations: Vec<Recovered>,
    ) {
        self.term = term;
        self.next_op = next_operation.0;
        self.next_seq = next_seq;
        let until = self.now.saturating_add(LEASE_GRACE);
        for recovered in operations {
            let Recovered {
                operation: id,
                request,
                waiter,
                lease,
            } = recovered;
            if request.joinable() {
                self.in_flight.insert(request.key.clone(), id);
            }
            let committed_lease = lease.as_ref().map(|l| l.lease);
            let state = match lease {
                Some(RecoveredLease { lease, worker }) => {
                    // A session no worker has: the heartbeat reconcile takes the lease
                    // for one that went to an earlier session of the process.
                    let sent = StartSent {
                        at: self.now,
                        session: u64::MAX,
                        process: 0,
                    };
                    let held = Held {
                        operation: id,
                        booked: request.resources,
                        device: None,
                        start_sent: Some(sent),
                    };
                    self.held.insert(lease, held);
                    self.awaited.entry(worker.clone()).or_insert(until);
                    OpState::Leased {
                        lease,
                        worker,
                        committed: true,
                    }
                }
                None => {
                    self.queue.insert((Reverse(request.qos.clone()), id));
                    OpState::Queued
                }
            };
            self.ops.insert(
                id,
                Operation {
                    request,
                    waiters: vec![waiter],
                    state,
                    committed_lease,
                    result_proposed: false,
                    gave_up: false,
                    unservable: None,
                    farm_runs: Vec::new(),
                    avoid: std::collections::BTreeSet::new(),
                    not_started: 0,
                },
            );
        }
    }

    /// Books the recovered leases of `id`, which is registering as `worker`, on it.
    pub(super) fn adopt(&mut self, id: &WorkerId, worker: &mut Worker) {
        if self.awaited.remove(id).is_none() {
            return;
        }
        for held in self.held.values_mut() {
            let op = &self.ops[&held.operation];
            if op.state.holding().is_none_or(|(_, holder)| holder != id) {
                continue;
            }
            held.booked = match op.request.kind {
                LeaseKind::WholeMachine => {
                    worker.whole = true;
                    worker.capacity
                }
                LeaseKind::Action | LeaseKind::Vm => {
                    worker.whole |= worker.exclusive(&op.request);
                    op.request.resources
                }
            };
            worker.booked = worker.booked.saturating_add(held.booked);
            worker.leases += 1;
        }
    }
}
