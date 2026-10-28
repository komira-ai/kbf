//! The farm's hot path on the js core: a daemon's `Hello`, heartbeat and `Result`, the
//! tick, a caller's submit and wait, and the handover's questions about streams in flight.
//!
//! Each function is the js arm of the [`Farm`](crate::farm::Farm) method of the same name
//! (`farm.rs`, `farm/beat.rs`, `farm/report.rs`, `farm/handover.rs`), with the same
//! arguments after `js`; `submit` and `wait` are the arms of
//! [`Dispatch`](kbf_front::Dispatch) for the farm. They start as `todo!`; this file is
//! filled in by the hot-path change.

// The bodies are `todo!`: the change that writes them removes this line.
#![allow(unused_variables)]

use kbf_front::{Submission, Ticket};
use kbf_proto::worker::{self, ResultAck, ServerMessage};
use kbf_sched::DaemonInstance;
use kbf_types::{LeaseId, WorkerId};
use tonic::Status;

use super::JsFarm;
use crate::farm::{Outbound, Reported, StreamId};

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
    todo!("hot: register")
}

/// [`Farm::resize`](crate::farm::Farm::resize).
pub(crate) fn resize(js: &JsFarm, worker: &WorkerId, stream: StreamId, reported: Reported) {
    todo!("hot: resize")
}

/// [`Farm::heartbeat`](crate::farm::Farm::heartbeat).
pub(crate) async fn heartbeat(
    js: &JsFarm,
    worker: &WorkerId,
    stream: StreamId,
    seq: u64,
    running: Vec<LeaseId>,
) -> bool {
    todo!("hot: heartbeat")
}

/// [`Farm::report`](crate::farm::Farm::report).
pub(crate) async fn report(
    js: &JsFarm,
    worker: &WorkerId,
    result: worker::Result,
) -> Option<ResultAck> {
    todo!("hot: report")
}

/// [`Farm::tick`](crate::farm::Farm::tick).
pub(crate) async fn tick(js: &JsFarm) {
    todo!("hot: tick")
}

/// [`Dispatch::submit`](kbf_front::Dispatch::submit) for the farm.
pub(crate) fn submit(js: &JsFarm, submission: Submission) -> Result<Ticket, Status> {
    todo!("hot: submit")
}

/// [`Dispatch::wait`](kbf_front::Dispatch::wait) for the farm.
pub(crate) fn wait(js: &JsFarm, name: &str) -> Option<Ticket> {
    todo!("hot: wait")
}

/// [`Farm::quiet`](crate::farm::Farm::quiet).
pub(crate) fn quiet(js: &JsFarm) -> bool {
    todo!("hot: quiet")
}

/// [`Farm::end_worker_streams`](crate::farm::Farm::end_worker_streams).
pub(crate) fn end_worker_streams(js: &JsFarm, status: &Status) -> usize {
    todo!("hot: end_worker_streams")
}
