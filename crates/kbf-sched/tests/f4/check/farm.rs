//! Farm faults in the model: leases given up for silence or replacement, charged to
//! their operation's farm budget.

use super::*;

impl Checker {
    /// Gives up every lease of a silent worker, in lease order. Returns the results
    /// proposed for those past their operation's farm budget.
    pub(super) fn expire(&mut self) -> Vec<Effect> {
        let now = self.now;
        let mut silent: Vec<LeaseId> = self
            .workers
            .values()
            .filter(|w| !w.alive(now))
            .flat_map(|w| w.leases.iter().copied())
            .collect();
        silent.sort();
        let mut proposed = Vec::new();
        for lease in silent {
            let op = self.leases[&lease].op;
            // The give-up's own result is on its way to the log.
            if self.op(op).silenced {
                continue;
            }
            self.hit("requeued: worker silent for G");
            // A worker's result on its way to the log decides the operation: the lease
            // is given up with no charge.
            if self.leases[&lease].sent.is_some() && !self.op(op).proposed {
                proposed.extend(self.give_up_charged(lease));
            } else {
                self.requeue(op);
            }
        }
        proposed
    }

    /// Gives up `lease`, whose `Start` was sent and which ended with no word (silence or
    /// a replacement), as one of its operation's farm runs: requeued while the budget
    /// lasts, avoiding its worker; past it, the result the scheduler proposes.
    pub(super) fn give_up_charged(&mut self, lease: LeaseId) -> Option<Effect> {
        let (id, worker) = {
            let l = &self.leases[&lease];
            (l.op, l.worker.clone())
        };
        let ended = self.op(id).farm_runs.len();
        let fault = FarmFault::Lost(LostWhy::Fenced);
        if u32::try_from(ended).unwrap_or(u32::MAX) >= FARM_RERUNS {
            self.hit("a give-up past the farm budget proposes its result");
            let op = self.op_mut(id);
            op.proposed = true;
            op.silenced = true;
            return Some(Effect::Commit(ControlRecord::Result(ResultRecord {
                lease,
                operation: id,
                outcome: Outcome::Failed(Failure::Lost(LostWhy::Fenced)),
            })));
        }
        self.hit("a give-up charged to the farm budget");
        let op = self.op_mut(id);
        op.avoid.insert(worker.clone());
        op.farm_runs.push(FarmRun { worker, fault });
        self.requeue(id);
        None
    }

    /// Gives up `lease`, whose `Start` the node never listed, as one of its operation's
    /// not-started give-ups: requeued while that budget lasts, avoiding its worker; past
    /// it, the `Infra` result the scheduler proposes. Charges nothing to the farm budget.
    pub(super) fn give_up_not_started(&mut self, lease: LeaseId) -> Option<Effect> {
        let (id, worker) = {
            let l = &self.leases[&lease];
            (l.op, l.worker.clone())
        };
        if self.op(id).not_started >= NOT_STARTED_RERUNS {
            self.hit("a not-started give-up past its budget proposes its result");
            let op = self.op_mut(id);
            op.proposed = true;
            op.silenced = true;
            return Some(Effect::Commit(ControlRecord::Result(ResultRecord {
                lease,
                operation: id,
                outcome: Outcome::Failed(Failure::Infra),
            })));
        }
        self.hit("a not-started give-up within its budget");
        let op = self.op_mut(id);
        op.not_started += 1;
        op.avoid.insert(worker);
        self.requeue(id);
        None
    }
}
