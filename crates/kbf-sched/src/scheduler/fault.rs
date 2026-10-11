//! Farm faults (failure classes, 6.5; issue #22): runs that ended as the farm's fault
//! and are run again by the scheduler, from one budget per operation.
//!
//! Three kinds of run are charged, each recorded as a [`FarmRun`]:
//!
//! - a busy node's memory kill below the action's own limit
//!   ([`Failure::NodeMemoryPressure`], [`super::memory`]);
//! - a run the worker reports lost ([`Failure::Lost`]): fenced, or a whole-machine
//!   action whose result a daemon restart took;
//! - a lease given up because the daemon that ran it was proven stopped by its
//!   successor ([`RequeueReason::Restarted`]): the successor's drivers killed every
//!   run it left before registering.
//!
//! After each, the operation runs again while it has ended at most [`FARM_RERUNS`]
//! runs so (so at most `1 + FARM_RERUNS` runs end as farm faults), and placement
//! prefers a node none of them ran on ([`crate::servable::Servable::fit`]). The run
//! past that finishes the operation with its failure, and its callers are told every
//! farm run. A restart past the budget has no result of its own: the scheduler
//! proposes one ([`Failure::Lost`] with [`LostWhy::DaemonRestarted`]), so the
//! operation ends only through the log, as every operation does.
//!
//! Leases given up for silence, replacement (no proof), reconnection, not starting or
//! a session switch are not charged: the last three never ran, and the first two are
//! left to a later change (issue #22).

use kbf_types::{
    ControlRecord, Effect, Failure, FarmFault, FarmRun, LeaseId, LostWhy, OperationId, Outcome,
    ResultRecord, WorkerId,
};

use super::Scheduler;
use super::memory::FARM_RERUNS;
use crate::requeue::RequeueReason;

impl Scheduler {
    /// Records that `operation`'s run on `worker` ended with `fault`. Runs it again,
    /// requeued for `reason` and preferring another node, while the budget lasts, and
    /// returns whether it does; when it does not, the caller finishes the operation.
    pub(super) fn farm_fault(
        &mut self,
        id: OperationId,
        worker: WorkerId,
        fault: FarmFault,
        reason: RequeueReason,
    ) -> bool {
        let op = self.ops.get_mut(&id).expect("faulted operations exist");
        op.avoid.insert(worker.clone());
        op.farm_runs.push(FarmRun { worker, fault });
        let ended = u32::try_from(op.farm_runs.len()).unwrap_or(u32::MAX);
        if ended > FARM_RERUNS {
            return false;
        }
        self.requeue(id, reason);
        true
    }

    /// A committed lost run of `record`'s operation, whose newest committed grant is
    /// the record's lease. Returns whether the operation runs again; when it does not,
    /// the caller finishes it with the record's failure. It runs again, charged nothing,
    /// when it no longer holds the lease (given up, and waiting to be granted again).
    pub(super) fn rerun_after_lost(&mut self, record: &ResultRecord, why: LostWhy) -> bool {
        let id = record.operation;
        let Some((_, worker)) = self.ops[&id]
            .state
            .holding()
            .filter(|(lease, _)| *lease == record.lease)
        else {
            return true;
        };
        let worker = worker.clone();
        self.farm_fault(id, worker, FarmFault::Lost(why), RequeueReason::Lost(why))
    }

    /// `operation`'s `lease` on `worker` was given up: the daemon that ran it was proven
    /// stopped. Charged as a lost run: requeued while the budget lasts; past it, the
    /// lease's result is proposed (the restart is that run's result), which the caller
    /// emits.
    pub(super) fn restarted(
        &mut self,
        id: OperationId,
        lease: LeaseId,
        worker: &WorkerId,
    ) -> Vec<Effect> {
        let op = self
            .ops
            .get_mut(&id)
            .expect("held leases name live operations");
        let ended = u32::try_from(op.farm_runs.len()).unwrap_or(u32::MAX);
        if ended >= FARM_RERUNS {
            op.result_proposed = true;
            return vec![Effect::Commit(ControlRecord::Result(ResultRecord {
                lease,
                operation: id,
                outcome: Outcome::Failed(Failure::Lost(LostWhy::DaemonRestarted)),
            }))];
        }
        let fault = FarmFault::Lost(LostWhy::DaemonRestarted);
        self.farm_fault(id, worker.clone(), fault, RequeueReason::Restarted);
        Vec::new()
    }
}
