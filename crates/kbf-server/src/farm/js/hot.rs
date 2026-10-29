//! The farm's hot path on the js core: a daemon's `Hello`, heartbeat and `Result`, the
//! tick, a caller's submit and wait, and the handover's questions about streams in flight.
//!
//! Each function is the js arm of the [`Farm`](crate::farm::Farm) method of the same name
//! (`farm.rs`, `farm/beat.rs`, `farm/report.rs`, `farm/handover.rs`), with the same
//! arguments after `js`; `submit` and `wait` are the arms of
//! [`Dispatch`](kbf_front::Dispatch) for the farm. The calls on the core are those of
//! [`crate::farm_js::dispatch`]; what the farm's driver does around a `Result`'s feed is the
//! adapter's (`crate::farm_js::report_hooks`).

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use js_server_core::Beat;
use kbf_front::{Submission, Ticket};
use kbf_proto::worker::{self, ResultAck, ServerMessage};
use kbf_sched::DaemonInstance;
use kbf_types::sched::usage::NodeLoad;
use kbf_types::{LeaseId, WorkerId};
use tonic::Status;

use super::JsFarm;
use crate::farm::{Outbound, Reported, StreamId};
use crate::farm_js::host::{Connection, stream_number};
use crate::farm_js::{Wall, dispatch};

/// One `Result` being taken, counted until dropped ([`quiet`] waits for none).
struct Taking<'a>(&'a AtomicUsize);

impl<'a> Taking<'a> {
    fn of(count: &'a AtomicUsize) -> Self {
        count.fetch_add(1, Ordering::SeqCst);
        Self(count)
    }
}

impl Drop for Taking<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Sets the scheduler gauges of `/metrics` from the core, in one hold of its lock: called
/// after a submit and on each tick, so a scrape sees the queue and the leases at most a
/// tick old.
fn publish(js: &JsFarm) {
    let metrics = js.cache.metrics();
    js.core.with(|view, _| {
        let sched = view.sched();
        metrics.sched_queued.set(sched.queued_len() as u64);
        metrics.sched_leases.set(sched.leases_held() as u64);
        metrics.sched_whole.set(sched.whole_summary().as_ref());
    });
}

/// [`Farm::register`](crate::farm::Farm::register).
pub(crate) fn register(
    js: &JsFarm,
    worker: &WorkerId,
    instance: DaemonInstance,
    predecessor: DaemonInstance,
    reported: Reported,
    outbound: Outbound,
    welcome: ServerMessage,
) -> StreamId {
    let connection = Connection { outbound, welcome };
    let registered = dispatch::register(
        &js.core,
        js.now(),
        worker,
        instance,
        predecessor,
        reported,
        connection,
    );
    // The core's admission is `AllowAll`: the worker service decides who is heard before
    // it calls the farm.
    let stream = registered.ok().and_then(stream_number).unwrap_or_default();
    StreamId(stream)
}

/// [`Farm::resize`](crate::farm::Farm::resize).
pub(crate) fn resize(js: &JsFarm, worker: &WorkerId, stream: StreamId, reported: Reported) {
    // A replaced stream's `Hello` is ignored.
    let Some(stream) = js
        .core
        .with(|_, adapter| adapter.current_stream(worker, stream.0))
    else {
        return;
    };
    // `AllowAll` never refuses.
    let _ = dispatch::resize(&js.core, js.now(), worker, stream, reported);
}

/// [`Farm::heartbeat_with_load`](crate::farm::Farm::heartbeat_with_load): `load` is the
/// reading the farm accepted on this heartbeat ([`super::load::note_node_load`]), which the core is
/// handed with the beat.
pub(crate) async fn heartbeat(
    js: &JsFarm,
    worker: &WorkerId,
    stream: StreamId,
    seq: u64,
    running: Vec<LeaseId>,
    load: Option<NodeLoad>,
) -> bool {
    // A heartbeat from a replaced stream is dropped and not acknowledged.
    let Some(stream) = js
        .core
        .with(|_, adapter| adapter.current_stream(worker, stream.0))
    else {
        return false;
    };
    let beat = Beat::new(seq, running);
    let beat = match load {
        Some(load) => beat.with_load(load),
        None => beat,
    };
    js.core
        .heartbeat(js.now(), worker, stream, beat)
        .await
        .unwrap_or(false)
}

/// [`Farm::report`](crate::farm::Farm::report).
pub(crate) async fn report(
    js: &JsFarm,
    worker: &WorkerId,
    result: worker::Result,
) -> Option<ResultAck> {
    result.lease_id?;
    let _taking = Taking::of(&js.reports);
    let ambient = js.ambient;
    let wall: Wall = Arc::new(move || ambient.wall());
    dispatch::report(&js.core, &js.cache, &wall, || js.now(), worker, result)
        .await
        .ok()
        .flatten()
}

/// [`Farm::tick`](crate::farm::Farm::tick).
pub(crate) async fn tick(js: &JsFarm) {
    js.core.tick(js.now()).await;
    publish(js);
}

/// [`Dispatch::submit`](kbf_front::Dispatch::submit) for the farm.
///
/// Known gap: the submission's `max_wall` is not used. The legacy farm admits with it, keeps it
/// in the operation's `opwall` row and gives it to the scheduler (`set_max_wall`), where a
/// whole-node drain reads it; `js-server-core` has no way to take it, so on this core a
/// whole-node drain sees no per-operation deadline (whole-node placement is off at the flip).
pub(crate) fn submit(js: &JsFarm, submission: Submission) -> Result<Ticket, Status> {
    let ticket = dispatch::submit(&js.core, js.now(), &js.shapes, submission)?;
    publish(js);
    Ok(ticket)
}

/// [`Dispatch::wait`](kbf_front::Dispatch::wait) for the farm.
pub(crate) fn wait(js: &JsFarm, name: &str) -> Option<Ticket> {
    dispatch::wait(&js.core, name)
}

/// [`Farm::quiet`](crate::farm::Farm::quiet).
pub(crate) fn quiet(js: &JsFarm) -> bool {
    let presubmit = js.cache.metrics().execute_presubmit_in_flight.get();
    let taking = u64::try_from(js.reports.load(Ordering::SeqCst)).unwrap_or(u64::MAX);
    presubmit + taking == 0
}

/// [`Farm::end_worker_streams`](crate::farm::Farm::end_worker_streams).
pub(crate) fn end_worker_streams(js: &JsFarm, status: &Status) -> usize {
    js.core.with(|_, adapter| adapter.end_streams(status))
}
