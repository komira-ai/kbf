//! The node load a daemon's heartbeat reports, as the js core takes it: the same accept rule
//! and vet as the farm's own driver ([`crate::farm::node_load`]), over the readings the
//! adapter keeps for each node.

use kbf_proto::worker::NodeLoad;
use kbf_types::WorkerId;
use kbf_types::sched::usage;

use super::JsFarm;
use crate::farm::StreamId;
use crate::farm::node_load::{Session, accept, shown_all};
use crate::fleet::LoadView;

/// [`Farm::note_node_load`](crate::farm::Farm::note_node_load): the reading, taken if the
/// accept rule takes it, as the load `Beat::with_load` takes
/// ([`hot::heartbeat`](super::hot::heartbeat)). `None` when there was no reading, it was
/// refused, or `stream` was replaced.
pub(crate) fn note_node_load(
    js: &JsFarm,
    worker: &WorkerId,
    stream: StreamId,
    load: Option<&NodeLoad>,
) -> Option<usage::NodeLoad> {
    let load = load?;
    let now = js.now();
    js.core.with(|view, adapter| {
        adapter.current_stream(worker, stream.0)?;
        let instance = adapter.worker_facts(worker)?.instance.clone();
        let session = Session::of(&instance, stream);
        let capacity = view.sched().capacity(worker).unwrap_or_default();
        accept(
            adapter.node_loads_mut(),
            worker,
            &session,
            load,
            &capacity,
            now,
        )
    })
}

/// [`Farm::node_loads`](crate::farm::Farm::node_loads).
pub(crate) fn node_loads(js: &JsFarm) -> Vec<(WorkerId, LoadView)> {
    let now = js.now();
    js.core.with(|view, adapter| {
        shown_all(
            adapter.node_loads(),
            |worker| view.sched().capacity(worker).unwrap_or_default(),
            now,
        )
    })
}

/// What is kept of `worker`'s readings.
#[cfg(test)]
pub(crate) fn kept_load(js: &JsFarm, worker: &WorkerId) -> Option<crate::farm::node_load::Kept> {
    js.core
        .with(|_, adapter| adapter.node_loads().get(worker).cloned())
}
