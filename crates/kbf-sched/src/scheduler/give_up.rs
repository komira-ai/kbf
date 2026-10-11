//! Giving leases up: a worker silent for G ([`Scheduler::expire`]), and the leases a
//! worker's heartbeat leaves out ([`Scheduler::reconcile`]; the rules are
//! [`crate::Event::Heartbeat`]'s).

use std::collections::BTreeSet;

use kbf_types::{Effect, FarmTime, LeaseId, OperationId, WorkerId};

use super::Scheduler;
use crate::fence::START_GRACE;
use crate::requeue::RequeueReason;

impl Scheduler {
    /// Sends every lease held on a worker not heard from for G
    /// ([`crate::fence::LEASE_GRACE`]) back to the queue, except the lease a worker
    /// switches the GUI session for, which is kept until the switch grace ends
    /// ([`crate::fence::SESSION_SWITCH_GRACE`]).
    pub(super) fn expire(&mut self) {
        let now = self.now;
        let expired: Vec<(OperationId, RequeueReason)> = self
            .held
            .iter()
            .filter_map(|(lease, held)| {
                let worker = self.ops[&held.operation].state.holding().map(|(_, w)| w);
                let w = worker.and_then(|w| self.workers.get(w));
                if w.is_some_and(|w| w.alive(now) || w.switching(*lease, now)) {
                    return None;
                }
                let switched = w.is_some_and(|w| w.phase.switching() == Some(*lease));
                let reason = if switched {
                    RequeueReason::SessionSwitch
                } else {
                    RequeueReason::Silent
                };
                Some((held.operation, reason))
            })
            .collect();
        for (id, reason) in expired {
            self.requeue(id, reason);
        }
    }

    /// Sends back to the queue every committed lease held on `worker` that `running`
    /// leaves out, if its `Start` went to an earlier session of the current daemon
    /// process, to the earlier process proven stopped (at once, as a farm fault:
    /// [`Scheduler::farm_fault`]), to another earlier process once the handover grace
    /// has ended, or to the current session and has been out for [`START_GRACE`]. A
    /// lease whose result was reported is kept: that result is on its way to the log.
    /// Returns the result proposed for a lease lost to a restart past the operation's
    /// farm reruns, which ends it once committed.
    pub(super) fn reconcile(&mut self, worker: &WorkerId, running: &[LeaseId]) -> Vec<Effect> {
        let (session, process, handover_ends, stopped) = self
            .workers
            .get(worker)
            .map_or((0, 0, FarmTime::default(), None), |w| {
                (w.session, w.process, w.handover_ends, w.stopped_process)
            });
        let running: BTreeSet<LeaseId> = running.iter().copied().collect();
        let now = self.now;
        let lost: Vec<(OperationId, LeaseId, RequeueReason)> = self
            .held
            .iter()
            .filter_map(|(lease, held)| {
                let sent = held.start_sent?;
                let (due, reason) = if sent.process != process && Some(sent.process) == stopped {
                    // Its successor killed it before registering.
                    (true, RequeueReason::Restarted)
                } else if sent.process != process {
                    // Another process may still run it: only its fence ends that.
                    (now >= handover_ends, RequeueReason::Replaced)
                } else if sent.session != session {
                    // A `Start` sent to an earlier session of this process reached it
                    // before it registered again, and then it lists it, or never will.
                    (true, RequeueReason::Reconnected)
                } else {
                    let due = now >= sent.at.saturating_add(START_GRACE);
                    (due, RequeueReason::NotStarted)
                };
                let op = &self.ops[&held.operation];
                let lost = due
                    && !running.contains(lease)
                    && !op.result_proposed
                    && op.state.holding().is_some_and(|(_, w)| w == worker);
                lost.then_some((held.operation, *lease, reason))
            })
            .collect();
        let mut effects = Vec::new();
        for (id, lease, reason) in lost {
            if reason == RequeueReason::Restarted {
                effects.extend(self.restarted(id, lease, worker));
            } else {
                self.requeue(id, reason);
            }
        }
        effects
    }
}
