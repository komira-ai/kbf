//! The pack-read counts of each daemon, from its heartbeats (`Heartbeat.pack_reads`), for the
//! `kbf_pack_daemon_*` families of `GET /metrics` ([`crate::pack_daemon_metrics`]).

use kbf_proto::worker::PackReadCounts;
use kbf_types::WorkerId;

use super::{Farm, StreamId};

impl Farm {
    /// The pack-read counts of a heartbeat of `worker` on `stream`: kept as the node's newest
    /// when the heartbeat carries them, and, when it carries none, the node's are forgotten (a
    /// daemon that was restarted without `--pack-read` reads no packs, and must not show the
    /// counts of the process before). Ignored if `stream` was replaced: the newest stream's
    /// heartbeats say what the node's daemon does now. A node that is gone keeps the counts of
    /// its last heartbeat until this frontend restarts or the node's next stream says
    /// otherwise.
    pub fn note_pack_reads(
        &self,
        worker: &WorkerId,
        stream: StreamId,
        counts: Option<PackReadCounts>,
    ) {
        js_arm!(self, views::note_pack_reads(worker, stream, counts));
        let mut state = self.lock();
        if !state.side.is_current(worker, stream) {
            return;
        }
        match counts {
            Some(counts) => state.side.pack_reads.insert(worker.clone(), counts),
            None => state.side.pack_reads.remove(worker),
        };
    }

    /// The newest pack-read counts of every node that sent any, in node-id order.
    #[must_use]
    pub fn pack_reads(&self) -> Vec<(WorkerId, PackReadCounts)> {
        js_arm!(self, views::pack_reads());
        let state = self.lock();
        state
            .side
            .pack_reads
            .iter()
            .map(|(worker, counts)| (worker.clone(), *counts))
            .collect()
    }
}

#[cfg(test)]
mod tests;
