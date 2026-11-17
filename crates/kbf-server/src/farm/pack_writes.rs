//! The pack-write counts of each worker, from its heartbeats (`Heartbeat.pack_writes`), for the
//! `kbf_pack_daemon_*` families of `GET /metrics` ([`crate::pack_daemon_metrics`]).
//!
//! No current worker sends these counts: the farm's workers are the executor's job-service
//! hosts ([`crate::executor`]), which send none, so the families are absent and the route
//! output holds none. They are read, and shown, for a worker that does send them.

use kbf_proto::worker::PackWriteCounts;
use kbf_types::WorkerId;

use super::{Farm, StreamId};

impl Farm {
    /// The pack-write counts of a heartbeat of `worker` on `stream`: kept as the node's newest
    /// when the heartbeat carries them, and, when it carries none, the node's are forgotten (a
    /// worker that was restarted without `--pack-write` writes no packs, and must not show the
    /// counts of the process before). Ignored if `stream` was replaced: the newest stream's
    /// heartbeats say what the node's worker does now. A node that is gone keeps the counts of
    /// its last heartbeat until this frontend restarts or the node's next stream says
    /// otherwise.
    pub fn note_pack_writes(
        &self,
        worker: &WorkerId,
        stream: StreamId,
        counts: Option<PackWriteCounts>,
    ) {
        let mut state = self.lock();
        if !state.side.is_current(worker, stream) {
            return;
        }
        match counts {
            Some(counts) => state.side.pack_writes.insert(worker.clone(), counts),
            None => state.side.pack_writes.remove(worker),
        };
    }

    /// The newest pack-write counts of every node that sent any, in node-id order.
    #[must_use]
    pub fn pack_writes(&self) -> Vec<(WorkerId, PackWriteCounts)> {
        let state = self.lock();
        state
            .side
            .pack_writes
            .iter()
            .map(|(worker, counts)| (worker.clone(), *counts))
            .collect()
    }
}

#[cfg(test)]
mod tests;
