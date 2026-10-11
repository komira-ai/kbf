//! Node phases ([`NodePhase`], `docs/design/scheduler.md#node-phases`): taking a
//! worker's phase, and sending a switching lease's `Start` again to the session that
//! names it after the daemon restarted for the switch. How long a silent switching
//! worker keeps its lease is [`super::Scheduler`]'s `expire`, by `Worker::switching`.

use kbf_types::{Effect, NodePhase, StartLease, WorkerId};

use super::{Scheduler, StartSent};
use crate::fence::SESSION_SWITCH_GRACE;

impl Scheduler {
    /// Takes `worker`'s phase (see [`Event::Phase`]); returns the `Start` sent again
    /// for the lease it switches the session for, if that `Start` went to an earlier
    /// session.
    pub(super) fn take_phase(&mut self, worker: &WorkerId, phase: NodePhase) -> Vec<Effect> {
        let now = self.now;
        let Some(w) = self.workers.get_mut(worker) else {
            return Vec::new();
        };
        w.last_heard = w.last_heard.max(now);
        if w.phase != phase {
            w.switch_ends = now.saturating_add(SESSION_SWITCH_GRACE);
        }
        w.phase = phase;
        let (session, process) = (w.session, w.process);
        let Some(lease) = w.phase.switching() else {
            return Vec::new();
        };
        let Some(held) = self.held.get_mut(&lease) else {
            return Vec::new();
        };
        let op = &self.ops[&held.operation];
        let here = op.state.holding().is_some_and(|(_, w)| w == worker);
        // Not committed yet (its `Start` goes out when it is), or already sent to this
        // session.
        if !held
            .start_sent
            .is_some_and(|sent| here && sent.session != session)
        {
            return Vec::new();
        }
        held.start_sent = Some(StartSent {
            at: now,
            session,
            process,
        });
        vec![Effect::Start(StartLease {
            worker: worker.clone(),
            lease,
            operation: held.operation,
            key: op.request.key.clone(),
            kind: op.request.kind,
            resources: held.booked,
            fence: op.request.fence(),
            device: held.device.clone(),
        })]
    }
}
