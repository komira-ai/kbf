//! The farm's views and counters on the js core: the node lists and details, the update
//! facts, the queue and lease snapshots, and the gauges and counters operators read.
//!
//! Each function is the js arm of the [`Farm`](crate::farm::Farm) method of the same name
//! (`farm/nodes.rs`, `farm/sched_view.rs`, `farm/pack_reads.rs`, `farm/pack_writes.rs`,
//! `farm/failed_actions.rs`, `farm/handover.rs`, `farm.rs`), with the same arguments after
//! `js`. What the scheduler knows is read through [`Core::with`](js_server_core::Core::with),
//! which holds the core's lock for the read; what the farm keeps beside it (the node
//! registry, the update and software status, the pack counts, the failed-operation counts and
//! what each stream said of itself) is the adapter's. A view is built by the same function
//! as the farm's own driver's where there is one (`farm/nodes.rs`, `farm/sched_view.rs`,
//! `fleet.rs`), so the two drivers answer alike for the same state.
//!
//! Where the legacy method first notes the worker streams that have closed, so does the js
//! arm ([`KbfAdapter::note_closed_streams`]).

use std::collections::{BTreeMap, BTreeSet};

use js_server_core::View;
use kbf_proto::update::UpdateStatus;
use kbf_proto::worker::{InputCacheCounts, NodeStatus, PackReadCounts, PackWriteCounts};
use kbf_sched::{LoadCounters, QueueView};
use kbf_types::{MemoryRun, OperationId, WorkerId};

use super::JsFarm;
use crate::farm::cordon_owner;
use crate::farm::failed_actions::FailedActions;
use crate::farm::nodes::{detail_of, look_of, node_of};
use crate::farm::sched_view::{LeaseViews, WholeView, whole_of};
use crate::farm::{NodeLook, NodeUnread, StreamId, node_load};
use crate::farm_js::KbfAdapter;
use crate::farm_js::ablation::Ablation;
use crate::farm_js::usage::UsageStats;
use crate::fleet::{NodeDetail, NodeView, NodesView};

/// `worker`, which the machine knows, as the node list shows it.
fn listed(js: &JsFarm, view: &View<'_>, adapter: &KbfAdapter, worker: &WorkerId) -> NodeView {
    let protocol = adapter
        .worker_facts(worker)
        .and_then(|facts| facts.protocol);
    node_of(
        view.sched(),
        adapter.machine(),
        js.epoch_unix_ms,
        protocol,
        worker,
    )
}

/// [`Farm::nodes`](crate::farm::Farm::nodes).
pub(crate) fn nodes(js: &JsFarm) -> NodesView {
    let nodes = js.core.with(|view, adapter| {
        adapter.note_closed_streams();
        let adapter = &*adapter;
        adapter
            .machine()
            .nodes()
            .map(|worker| listed(js, view, adapter, worker))
            .collect()
    });
    NodesView::of_this_build(nodes)
}

/// [`Farm::node_view`](crate::farm::Farm::node_view).
pub(crate) fn node_view(js: &JsFarm, worker: &WorkerId) -> Option<NodeView> {
    js.core.with(|view, adapter| {
        adapter.note_closed_streams();
        let adapter = &*adapter;
        let registered = adapter.machine().node(worker).is_some();
        registered.then(|| listed(js, view, adapter, worker))
    })
}

/// [`Farm::node_detail`](crate::farm::Farm::node_detail).
///
/// A node another frontend of the state store holds is not answered from the store: this
/// arm answers only the nodes registered here. One not registered here is `None` when the
/// store can be read (or the farm has none), and unread when the `cordon/` rows cannot be
/// read and the node is not connected. Only the `cordon/` read decides readability here: a
/// node registered here but not connected, whose `owner/` or `lease/` rows cannot be read,
/// is answered from this frontend, where the farm's own driver says it is unread.
pub(crate) fn node_detail(
    js: &JsFarm,
    worker: &WorkerId,
) -> Result<Option<NodeDetail>, NodeUnread> {
    // The `cordon/` rows are taken up first, so the node's own frontend shows a drain
    // another frontend wrote at once. The core's lock is not held across the two calls.
    let cordons = js.core.cordons_now(js.now());
    let readable = cordons.is_some() || js.store.is_none();
    js.core.with(|view, adapter| {
        adapter.note_closed_streams();
        let adapter = &*adapter;
        let node = adapter.machine().node(worker);
        if !readable && !node.is_some_and(|node| node.connected) {
            return Err(NodeUnread(worker.as_str().to_owned()));
        }
        let detail = |_| {
            let capacity = view.sched().capacity(worker).unwrap_or_default();
            let load = node_load::shown(adapter.node_loads(), worker, &capacity, js.now());
            let input_cache = adapter.input_cache_of(worker);
            let detail = detail_of(
                listed(js, view, adapter, worker),
                view.sched(),
                worker,
                load,
                input_cache,
            );
            cordon_owner::with_owner(detail, || cordon_owner::shown(js.store.as_deref(), worker))
        };
        Ok(node.map(detail))
    })
}

/// [`Farm::node_status`](crate::farm::Farm::node_status).
pub(crate) fn node_status(
    js: &JsFarm,
    worker: &WorkerId,
    stream: StreamId,
    status: NodeStatus,
) -> Vec<(bool, String)> {
    js.core.with(|_, adapter| {
        match adapter.current_stream(worker, stream.0) {
            Some(stream) => adapter.report_node_status(worker, stream, status),
            // A replaced stream's status is ignored.
            None => Vec::new(),
        }
    })
}

/// [`Farm::update_status`](crate::farm::Farm::update_status).
pub(crate) fn update_status(
    js: &JsFarm,
    worker: &WorkerId,
    stream: StreamId,
    status: UpdateStatus,
) -> bool {
    js.core.with(|_, adapter| {
        let current = adapter.current_stream(worker, stream.0);
        current.is_some_and(|stream| adapter.report_update_status(worker, stream, status))
    })
}

/// [`Farm::update_look`](crate::farm::Farm::update_look).
pub(crate) fn update_look(js: &JsFarm) -> Vec<NodeLook> {
    js.core.with(|view, adapter| {
        adapter.note_closed_streams();
        let adapter = &*adapter;
        adapter
            .machine()
            .nodes()
            .map(|worker| {
                let facts = adapter.worker_facts(worker);
                let identity = facts.map_or((String::new(), ""), |f| (f.os.clone(), f.arch));
                look_of(view.sched(), adapter.machine(), identity, worker)
            })
            .collect()
    })
}

/// [`Farm::queue_view`](crate::farm::Farm::queue_view).
pub(crate) fn queue_view(js: &JsFarm) -> QueueView {
    js.core.with(|view, _| view.sched().queue_snapshot())
}

/// [`Farm::load_counters`](crate::farm::Farm::load_counters).
pub(crate) fn load_counters(js: &JsFarm) -> LoadCounters {
    js.core.with(|view, _| view.sched().load_counters())
}

/// [`Farm::usage_stats`](crate::farm::Farm::usage_stats).
pub(crate) fn usage_stats(js: &JsFarm) -> Option<UsageStats> {
    js.usage.as_ref().map(|usage| usage.stats())
}

/// [`Farm::ablation`](crate::farm::Farm::ablation).
pub(crate) fn ablation(js: &JsFarm) -> Option<(Ablation, LoadCounters)> {
    js.core
        .with(|view, adapter| Some((adapter.ablation().clone(), view.sched().load_counters())))
}

/// [`Farm::whole_view`](crate::farm::Farm::whole_view).
pub(crate) fn whole_view(js: &JsFarm) -> Option<WholeView> {
    let now = js.now();
    js.core.with(|view, _| whole_of(now, view.sched()))
}

/// [`Farm::queue_views`](crate::farm::Farm::queue_views).
pub(crate) fn queue_views(js: &JsFarm) -> (QueueView, Option<WholeView>) {
    let now = js.now();
    js.core.with(|view, _| {
        let sched = view.sched();
        (sched.queue_snapshot(), whole_of(now, sched))
    })
}

/// [`Farm::lease_views`](crate::farm::Farm::lease_views).
pub(crate) fn lease_views(js: &JsFarm, node: Option<&WorkerId>) -> LeaseViews {
    js.core.with(|view, _| LeaseViews {
        term: view.sched().term(),
        epoch_unix_ms: js.epoch_unix_ms,
        leases: view.sched().lease_snapshot(node),
    })
}

/// [`Farm::open_executions`](crate::farm::Farm::open_executions).
pub(crate) fn open_executions(js: &JsFarm) -> u64 {
    js.cache.metrics().execute.open_executions.get()
}

/// [`Farm::open_sessions`](crate::farm::Farm::open_sessions).
pub(crate) fn open_sessions(js: &JsFarm) -> usize {
    js.core.sessions()
}

/// [`Farm::queued_operations`](crate::farm::Farm::queued_operations).
pub(crate) fn queued_operations(js: &JsFarm) -> u64 {
    js.core.queued_operations()
}

/// [`Farm::lost_races`](crate::farm::Farm::lost_races): the js core does not tell a lost
/// race from a dropped write, and counts it in [`dropped_writes`].
pub(crate) fn lost_races(_js: &JsFarm) -> u64 {
    0
}

/// [`Farm::dropped_writes`](crate::farm::Farm::dropped_writes).
pub(crate) fn dropped_writes(js: &JsFarm) -> u64 {
    js.core.dropped_writes()
}

/// [`Farm::failed_actions`](crate::farm::Farm::failed_actions).
pub(crate) fn failed_actions(js: &JsFarm) -> FailedActions {
    js.core.with(|_, adapter| adapter.failed_actions())
}

/// [`Farm::memory_runs`](crate::farm::Farm::memory_runs).
pub(crate) fn memory_runs(js: &JsFarm) -> BTreeMap<OperationId, Vec<MemoryRun>> {
    js.core
        .with(|view, _| view.sched().memory_state().runs_by_operation())
}

/// [`Farm::avoided`](crate::farm::Farm::avoided).
pub(crate) fn avoided(js: &JsFarm) -> BTreeMap<OperationId, BTreeSet<WorkerId>> {
    js.core.with(|view, _| view.sched().avoided())
}

/// [`Farm::note_pack_reads`](crate::farm::Farm::note_pack_reads).
pub(crate) fn note_pack_reads(
    js: &JsFarm,
    worker: &WorkerId,
    stream: StreamId,
    counts: Option<PackReadCounts>,
) {
    js.core.with(|_, adapter| {
        if let Some(stream) = adapter.current_stream(worker, stream.0) {
            adapter.note_pack_reads(worker, stream, counts);
        }
    });
}

/// [`Farm::pack_reads`](crate::farm::Farm::pack_reads).
pub(crate) fn pack_reads(js: &JsFarm) -> Vec<(WorkerId, PackReadCounts)> {
    js.core.with(|_, adapter| adapter.pack_reads())
}

/// [`Farm::note_pack_writes`](crate::farm::Farm::note_pack_writes).
pub(crate) fn note_pack_writes(
    js: &JsFarm,
    worker: &WorkerId,
    stream: StreamId,
    counts: Option<PackWriteCounts>,
) {
    js.core.with(|_, adapter| {
        if let Some(stream) = adapter.current_stream(worker, stream.0) {
            adapter.note_pack_writes(worker, stream, counts);
        }
    });
}

/// [`Farm::pack_writes`](crate::farm::Farm::pack_writes).
pub(crate) fn pack_writes(js: &JsFarm) -> Vec<(WorkerId, PackWriteCounts)> {
    js.core.with(|_, adapter| adapter.pack_writes())
}

/// [`Farm::note_input_cache`](crate::farm::Farm::note_input_cache).
pub(crate) fn note_input_cache(
    js: &JsFarm,
    worker: &WorkerId,
    stream: StreamId,
    counts: Option<InputCacheCounts>,
) {
    js.core.with(|_, adapter| {
        if let Some(stream) = adapter.current_stream(worker, stream.0) {
            adapter.note_input_cache(worker, stream, counts);
        }
    });
}

/// [`Farm::input_caches`](crate::farm::Farm::input_caches).
pub(crate) fn input_caches(js: &JsFarm) -> Vec<(WorkerId, InputCacheCounts)> {
    js.core.with(|_, adapter| adapter.input_caches())
}
