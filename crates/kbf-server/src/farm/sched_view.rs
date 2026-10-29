//! The farm's views of the scheduler for the operator API (`GET /v1/leases`, `GET
//! /v1/queue`; [`crate::http_sched`]): copied under the farm's lock, rendered after it
//! is released.

use kbf_sched::{LeaseView, MAX_AVOIDED, QueueView, Scheduler, WholeState, WholeSummary};
use kbf_types::{FarmTime, LeaseKind, OperationId, WorkKey, WorkerId};

use super::Farm;

/// The leases the scheduler holds, with what turns its farm times into wall-clock ones.
#[derive(Clone, Debug)]
pub struct LeaseViews {
    /// The scheduler's leader term: every lease's term, and not stable across a restart.
    pub term: u64,
    /// The wall-clock time of farm time zero, in milliseconds since the Unix epoch.
    pub epoch_unix_ms: u64,
    /// The leases, in lease order.
    pub leases: Vec<LeaseView>,
}

/// The whole-node view of the queue, with `--whole-node-max` on: the caps' numbers and the
/// queued whole-machine requests that wait.
#[derive(Clone, Debug)]
pub struct WholeView {
    /// Farm time when it was read: what the times in `requests` are measured from.
    pub now: FarmTime,
    /// What the caps count and allow, and how many requests wait for each reason.
    pub summary: WholeSummary,
    /// The queued whole-machine requests, in queue order, at most [`MAX_AVOIDED`].
    pub requests: Vec<WholeRequest>,
    /// How many there are, before the cap.
    pub total: usize,
}

/// One queued whole-machine request and why it waits.
#[derive(Clone, Debug)]
pub struct WholeRequest {
    /// The operation.
    pub operation: OperationId,
    /// Its dedup key: the instance and the action digest.
    pub key: WorkKey,
    /// Why it is not granted a worker.
    pub state: WholeState,
}

impl Farm {
    /// The leases held now: all of them, or only `node`'s (none for a node that holds
    /// none or does not exist).
    pub fn lease_views(&self, node: Option<&WorkerId>) -> LeaseViews {
        js_arm!(self, views::lease_views(node));
        let state = self.lock();
        LeaseViews {
            term: state.engine.sched.term(),
            epoch_unix_ms: self.epoch_unix_ms,
            leases: state.engine.sched.lease_snapshot(node),
        }
    }

    /// The queue now, summarised.
    pub fn queue_view(&self) -> QueueView {
        js_arm!(self, views::queue_view());
        self.lock().engine.sched.queue_snapshot()
    }

    /// The whole-node view of the queue now; `None` unless the farm runs with whole-node
    /// settings.
    pub fn whole_view(&self) -> Option<WholeView> {
        js_arm!(self, views::whole_view());
        whole_of(self.now(), &self.lock().engine.sched)
    }

    /// The queue and its whole-node view from one read of the scheduler, so the two agree.
    pub fn queue_views(&self) -> (QueueView, Option<WholeView>) {
        js_arm!(self, views::queue_views());
        let state = self.lock();
        let sched = &state.engine.sched;
        (sched.queue_snapshot(), whole_of(self.now(), sched))
    }
}

/// The whole-node view of `sched` at farm time `now`; `None` unless the farm runs with
/// whole-node settings. Both drivers of the farm read the queue through this.
pub(super) fn whole_of(now: FarmTime, sched: &Scheduler) -> Option<WholeView> {
    let summary = sched.whole_summary()?;
    let mut requests: Vec<WholeRequest> = sched
        .queued()
        .filter(|op| {
            sched
                .request(*op)
                .is_some_and(|r| r.kind == LeaseKind::WholeMachine)
        })
        .filter_map(|operation| {
            Some(WholeRequest {
                operation,
                key: sched.request(operation)?.key.clone(),
                state: sched.whole_state(operation)?,
            })
        })
        .collect();
    let total = requests.len();
    requests.truncate(MAX_AVOIDED);
    Some(WholeView {
        now,
        summary,
        requests,
        total,
    })
}
