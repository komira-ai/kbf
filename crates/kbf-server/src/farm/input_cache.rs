//! The input cache counts of each daemon, from its heartbeats (`Heartbeat.input_cache`), for the
//! `kbf_node_input_cache_*` families of `GET /metrics` ([`crate::input_cache_metrics`]) and the
//! `input_cache` object of `GET /v1/nodes/{id}`.

use kbf_proto::worker::InputCacheCounts;
use kbf_types::WorkerId;

use super::{Farm, StreamId};

impl Farm {
    /// The input cache counts of a heartbeat of `worker` on `stream`: kept as the node's newest
    /// when the heartbeat carries them, and, when it carries none, the node's are forgotten (a
    /// daemon restarted with its cache off has none, and must not show the counts of the process
    /// before). Ignored if `stream` was replaced. A node that is gone keeps the counts of its
    /// last heartbeat until this frontend restarts or the node's next stream says otherwise.
    pub fn note_input_cache(
        &self,
        worker: &WorkerId,
        stream: StreamId,
        counts: Option<InputCacheCounts>,
    ) {
        js_arm!(self, views::note_input_cache(worker, stream, counts));
        let mut state = self.lock();
        if !state.side.is_current(worker, stream) {
            return;
        }
        match counts {
            Some(counts) => state.side.input_caches.insert(worker.clone(), counts),
            None => state.side.input_caches.remove(worker),
        };
    }

    /// The newest input cache counts of every node that sent any, in node-id order.
    #[must_use]
    pub fn input_caches(&self) -> Vec<(WorkerId, InputCacheCounts)> {
        js_arm!(self, views::input_caches());
        let state = self.lock();
        let kept = state.side.input_caches.iter();
        kept.map(|(worker, counts)| (worker.clone(), *counts))
            .collect()
    }
}

#[cfg(test)]
mod tests;
