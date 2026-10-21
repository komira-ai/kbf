//! Farm faults (failure classes, 6.5; issue #22): runs that ended as the farm's fault
//! and are run again by the scheduler, from one budget per operation.
//!
//! Five kinds of run are charged, each recorded as a [`FarmRun`]:
//!
//! - a busy node's memory kill below the action's own limit
//!   ([`Failure::NodeMemoryPressure`], [`super::memory`]);
//! - a run the worker reports lost ([`Failure::Lost`]): fenced, or a whole-machine
//!   action whose result a daemon restart took;
//! - a run the farm could not do ([`Failure::Infra`]): the daemon's driver failed
//!   around the action, which is not the action's failure;
//! - a lease given up because the daemon that ran it was proven stopped by its
//!   successor ([`RequeueReason::Restarted`]): the successor's drivers killed every
//!   run it left before registering;
//! - a lease that had been sent its `Start` and was given up without proof, because its
//!   node went silent past the lease grace ([`RequeueReason::Silent`]) or another
//!   daemon process replaced it and the handover grace ended
//!   ([`RequeueReason::Replaced`]). The run may have happened and its result is
//!   gone; the node's self-fence ([`crate::fence`]) ends it within the grace, so it is
//!   charged as lost ([`LostWhy::Fenced`]).
//!
//! After each, the operation runs again while it has ended at most [`FARM_RERUNS`]
//! runs so (so at most `1 + FARM_RERUNS` runs end as farm faults), and placement
//! prefers a node none of them ran on ([`crate::servable::Servable::fit`]). The run
//! past that finishes the operation with its failure, and its callers are told every
//! farm run. A give-up past the budget (a restart, silence or a replacement) has no
//! result of its own: the scheduler proposes one ([`Failure::Lost`] with
//! [`LostWhy::DaemonRestarted`] for a restart, [`LostWhy::Fenced`] otherwise), so the
//! operation ends only through the log, as every operation does.
//!
//! A lease whose own result is already on its way to the log when the lease is given up
//! for silence is requeued, charged nothing, and the log order decides: the result
//! decides the operation if it commits before the next grant does (an OK one is
//! answered). A farm-fault result ([`Failure::Infra`] or [`Failure::Lost`]) that commits
//! after the give-up finds the operation no longer holding the lease, and is not
//! charged either, so that one run is free and the bound of `1 + FARM_RERUNS` runs is
//! exact only without that race. (Pinned by
//! `a_farm_fault_result_committed_after_the_give_up_is_not_charged`; whether it should
//! be charged is the lead's decision.) [`Scheduler::reconcile`] skips such a lease
//! instead.
//!
//! Leases given up for reconnection or a session switch are charged to no budget: those
//! runs never ran.
//!
//! A lease given up as not started (the node heartbeats but never listed it within the
//! start grace) never ran either, so it is always safe to run elsewhere and spends none
//! of [`FARM_RERUNS`]. But a node that drops every `Start` would hold the operation for
//! ever, so it has a small budget of its own, [`NOT_STARTED_RERUNS`] per operation:
//! each such give-up adds the node to the nodes placement avoids and runs the operation
//! again while the budget lasts, and the give-up past it ends the operation with
//! [`Failure::Infra`] (the farm never started the action; the action did not fail),
//! proposed through the log like the other past-budget give-ups. Because that result
//! is the scheduler's own, its commit is not run again as a farm fault. Infra reruns
//! spend none of the not-started budget.
//!
//! Accepted gap (the lead's decision): a worker's farm-fault result committed after
//! silence gave its lease up is not charged, so that operation gets one free rerun; the
//! log order still decides.

use kbf_types::{
    ControlRecord, Effect, Failure, FarmFault, FarmRun, LeaseId, LostWhy, OperationId, Outcome,
    ResultRecord, WorkerId,
};

use super::Scheduler;
use super::memory::{FARM_RERUNS, NOT_STARTED_RERUNS};
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
        let fault = FarmFault::Lost(why);
        self.rerun_after_farm_result(record, fault, RequeueReason::Lost(why))
    }

    /// A committed result of `record`'s operation that the farm could not run
    /// ([`Failure::Infra`]), charged like a lost run. Returns whether the operation
    /// runs again.
    pub(super) fn rerun_after_infra(&mut self, record: &ResultRecord) -> bool {
        self.rerun_after_farm_result(record, FarmFault::Infra, RequeueReason::Infra)
    }

    fn rerun_after_farm_result(
        &mut self,
        record: &ResultRecord,
        fault: FarmFault,
        reason: RequeueReason,
    ) -> bool {
        let id = record.operation;
        let Some((_, worker)) = self.ops[&id]
            .state
            .holding()
            .filter(|(lease, _)| *lease == record.lease)
        else {
            return true;
        };
        let worker = worker.clone();
        self.farm_fault(id, worker, fault, reason)
    }

    /// `operation`'s `lease` on `worker` was given up: the daemon that ran it was proven
    /// stopped. Charged as a lost run ([`Scheduler::give_up_charged`]).
    pub(super) fn restarted(
        &mut self,
        id: OperationId,
        lease: LeaseId,
        worker: &WorkerId,
    ) -> Vec<Effect> {
        let why = LostWhy::DaemonRestarted;
        self.give_up_charged(id, lease, worker, why, RequeueReason::Restarted)
    }

    /// `operation`'s `lease` on `worker`, whose `Start` was sent, was given up for
    /// `reason` (silence or replacement) with no proof of how the run ended. Charged as
    /// a fenced run ([`Scheduler::give_up_charged`]). A lease whose `Start` was never
    /// sent ran nothing: it is requeued, charged nothing.
    pub(super) fn unproven(
        &mut self,
        id: OperationId,
        lease: LeaseId,
        worker: &WorkerId,
        reason: RequeueReason,
    ) -> Vec<Effect> {
        let sent = self
            .held
            .get(&lease)
            .is_some_and(|h| h.start_sent.is_some());
        let op = &self.ops[&id];
        // A worker's result already on its way to the log decides the operation. (A
        // lease whose `Start` was sent is the newest committed grant: the send is the
        // commit.)
        if !sent || op.result_proposed {
            self.requeue(id, reason);
            return Vec::new();
        }
        self.give_up_charged(id, lease, worker, LostWhy::Fenced, reason)
    }

    /// `operation`'s `lease` on `worker` was given up as not started. Runs it again
    /// preferring another node while [`NOT_STARTED_RERUNS`] lasts, charging no farm run;
    /// past it, proposes the lease's result ([`Failure::Infra`]), which the caller emits.
    pub(super) fn not_started(
        &mut self,
        id: OperationId,
        lease: LeaseId,
        worker: &WorkerId,
    ) -> Vec<Effect> {
        let op = self
            .ops
            .get_mut(&id)
            .expect("held leases name live operations");
        if op.not_started >= NOT_STARTED_RERUNS {
            op.result_proposed = true;
            op.gave_up = true;
            return vec![Effect::Commit(ControlRecord::Result(ResultRecord {
                lease,
                operation: id,
                outcome: Outcome::Failed(Failure::Infra),
            }))];
        }
        op.not_started += 1;
        op.avoid.insert(worker.clone());
        self.requeue(id, RequeueReason::NotStarted);
        Vec::new()
    }

    /// Charges a given-up lease to the operation's farm budget as a run lost for `why`:
    /// requeued for `reason` while the budget lasts; past it, the lease's result is
    /// proposed (the loss is that run's result), which the caller emits.
    fn give_up_charged(
        &mut self,
        id: OperationId,
        lease: LeaseId,
        worker: &WorkerId,
        why: LostWhy,
        reason: RequeueReason,
    ) -> Vec<Effect> {
        let op = self
            .ops
            .get_mut(&id)
            .expect("held leases name live operations");
        let ended = u32::try_from(op.farm_runs.len()).unwrap_or(u32::MAX);
        if ended >= FARM_RERUNS {
            op.result_proposed = true;
            op.gave_up = true;
            return vec![Effect::Commit(ControlRecord::Result(ResultRecord {
                lease,
                operation: id,
                outcome: Outcome::Failed(Failure::Lost(why)),
            }))];
        }
        self.farm_fault(id, worker.clone(), FarmFault::Lost(why), reason);
        Vec::new()
    }
}
