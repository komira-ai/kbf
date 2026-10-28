//! The farm's views and counters on the js core: the node lists and details, the update
//! facts, the queue and lease snapshots, and the gauges and counters operators read.
//!
//! Each function is the js arm of the [`Farm`](crate::farm::Farm) method of the same name
//! (`farm/nodes.rs`, `farm/sched_view.rs`, `farm/pack_reads.rs`, `farm/pack_writes.rs`,
//! `farm/failed_actions.rs`, `farm/handover.rs`, `farm.rs`), with the same arguments after
//! `js`. They start as `todo!`; this file is filled in by the views change.

// The bodies are `todo!`: the change that writes them removes this line.
#![allow(unused_variables)]

use std::collections::{BTreeMap, BTreeSet};

use kbf_proto::update::UpdateStatus;
use kbf_proto::worker::{NodeStatus, PackReadCounts, PackWriteCounts};
use kbf_types::{MemoryRun, OperationId, WorkerId};

use super::JsFarm;
use crate::farm::failed_actions::FailedActions;
use crate::farm::sched_view::{LeaseViews, WholeView};
use crate::farm::{NodeLook, NodeUnread, StreamId};
use crate::fleet::{NodeDetail, NodeView, NodesView};
use kbf_sched::QueueView;

/// [`Farm::nodes`](crate::farm::Farm::nodes).
pub(crate) fn nodes(js: &JsFarm) -> NodesView {
    todo!("views: nodes")
}

/// [`Farm::node_view`](crate::farm::Farm::node_view).
pub(crate) fn node_view(js: &JsFarm, worker: &WorkerId) -> Option<NodeView> {
    todo!("views: node_view")
}

/// [`Farm::node_detail`](crate::farm::Farm::node_detail).
pub(crate) fn node_detail(
    js: &JsFarm,
    worker: &WorkerId,
) -> Result<Option<NodeDetail>, NodeUnread> {
    todo!("views: node_detail")
}

/// [`Farm::node_status`](crate::farm::Farm::node_status).
pub(crate) fn node_status(
    js: &JsFarm,
    worker: &WorkerId,
    stream: StreamId,
    status: NodeStatus,
) -> Vec<(bool, String)> {
    todo!("views: node_status")
}

/// [`Farm::update_status`](crate::farm::Farm::update_status).
pub(crate) fn update_status(
    js: &JsFarm,
    worker: &WorkerId,
    stream: StreamId,
    status: UpdateStatus,
) -> bool {
    todo!("views: update_status")
}

/// [`Farm::update_look`](crate::farm::Farm::update_look).
pub(crate) fn update_look(js: &JsFarm) -> Vec<NodeLook> {
    todo!("views: update_look")
}

/// [`Farm::queue_view`](crate::farm::Farm::queue_view).
pub(crate) fn queue_view(js: &JsFarm) -> QueueView {
    todo!("views: queue_view")
}

/// [`Farm::whole_view`](crate::farm::Farm::whole_view).
pub(crate) fn whole_view(js: &JsFarm) -> Option<WholeView> {
    todo!("views: whole_view")
}

/// [`Farm::queue_views`](crate::farm::Farm::queue_views).
pub(crate) fn queue_views(js: &JsFarm) -> (QueueView, Option<WholeView>) {
    todo!("views: queue_views")
}

/// [`Farm::lease_views`](crate::farm::Farm::lease_views).
pub(crate) fn lease_views(js: &JsFarm, node: Option<&WorkerId>) -> LeaseViews {
    todo!("views: lease_views")
}

/// [`Farm::open_executions`](crate::farm::Farm::open_executions).
pub(crate) fn open_executions(js: &JsFarm) -> u64 {
    todo!("views: open_executions")
}

/// [`Farm::open_sessions`](crate::farm::Farm::open_sessions).
pub(crate) fn open_sessions(js: &JsFarm) -> usize {
    todo!("views: open_sessions")
}

/// [`Farm::queued_operations`](crate::farm::Farm::queued_operations).
pub(crate) fn queued_operations(js: &JsFarm) -> u64 {
    todo!("views: queued_operations")
}

/// [`Farm::dropped_writes`](crate::farm::Farm::dropped_writes).
pub(crate) fn dropped_writes(js: &JsFarm) -> u64 {
    todo!("views: dropped_writes")
}

/// [`Farm::failed_actions`](crate::farm::Farm::failed_actions).
pub(crate) fn failed_actions(js: &JsFarm) -> FailedActions {
    todo!("views: failed_actions")
}

/// [`Farm::memory_runs`](crate::farm::Farm::memory_runs).
pub(crate) fn memory_runs(js: &JsFarm) -> BTreeMap<OperationId, Vec<MemoryRun>> {
    todo!("views: memory_runs")
}

/// [`Farm::avoided`](crate::farm::Farm::avoided).
pub(crate) fn avoided(js: &JsFarm) -> BTreeMap<OperationId, BTreeSet<WorkerId>> {
    todo!("views: avoided")
}

/// [`Farm::note_pack_reads`](crate::farm::Farm::note_pack_reads).
pub(crate) fn note_pack_reads(
    js: &JsFarm,
    worker: &WorkerId,
    stream: StreamId,
    counts: Option<PackReadCounts>,
) {
    todo!("views: note_pack_reads")
}

/// [`Farm::pack_reads`](crate::farm::Farm::pack_reads).
pub(crate) fn pack_reads(js: &JsFarm) -> Vec<(WorkerId, PackReadCounts)> {
    todo!("views: pack_reads")
}

/// [`Farm::note_pack_writes`](crate::farm::Farm::note_pack_writes).
pub(crate) fn note_pack_writes(
    js: &JsFarm,
    worker: &WorkerId,
    stream: StreamId,
    counts: Option<PackWriteCounts>,
) {
    todo!("views: note_pack_writes")
}

/// [`Farm::pack_writes`](crate::farm::Farm::pack_writes).
pub(crate) fn pack_writes(js: &JsFarm) -> Vec<(WorkerId, PackWriteCounts)> {
    todo!("views: pack_writes")
}
