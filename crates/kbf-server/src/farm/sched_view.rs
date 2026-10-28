//! The farm's views of the scheduler for the operator API (`GET /v1/leases`, `GET
//! /v1/queue`; [`crate::http_sched`]): copied under the farm's lock, rendered after it
//! is released.

use kbf_sched::{LeaseView, QueueView};
use kbf_types::WorkerId;

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

impl Farm {
    /// The leases held now: all of them, or only `node`'s (none for a node that holds
    /// none or does not exist).
    pub fn lease_views(&self, node: Option<&WorkerId>) -> LeaseViews {
        let state = self.lock();
        LeaseViews {
            term: state.engine.sched.term(),
            epoch_unix_ms: self.epoch_unix_ms,
            leases: state.engine.sched.lease_snapshot(node),
        }
    }

    /// The queue now, summarised.
    pub fn queue_view(&self) -> QueueView {
        self.lock().engine.sched.queue_snapshot()
    }
}
