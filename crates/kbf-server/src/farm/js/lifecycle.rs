//! The farm's state store and lifecycle on the js core: the handover (fence, resume, close
//! intake, flush, follow), the intake poll, the epoch, operator placement, and the reads of
//! the store.
//!
//! Each function is the js arm of the [`Farm`](crate::farm::Farm) method of the same name
//! (`farm/handover.rs`, `farm/durable.rs`, `farm/durable/intake.rs`, `farm.rs`), with the
//! same arguments after `js`. They start as `todo!`; this file is filled in by the
//! lifecycle change. What the farm owns before it serves, the core's construction from the
//! settings and the rebuild around a state store, is in `farm/js/mod.rs`.
//!
//! `Farm::poll_intake` and `Farm::follow_every` have no arm: they only call
//! [`Farm::intake`](crate::farm::Farm::intake) and [`Farm::follow`](crate::farm::Farm::follow)
//! on a timer, whichever core the farm runs on.

// The bodies are `todo!`: the change that writes them removes this line.
#![allow(unused_variables)]

use std::time::Duration;

use kbf_front::Finished;
use kbf_types::{OperationId, WorkerId};
use tokio::sync::watch;

use super::JsFarm;
use crate::farm::{EpochBump, NodeAction, PlaceError, StateError};
use crate::fleet::NodeView;

/// [`Farm::fence`](crate::farm::Farm::fence).
pub(crate) fn fence(js: &JsFarm) {
    todo!("lifecycle: fence")
}

/// [`Farm::resume`](crate::farm::Farm::resume).
pub(crate) fn resume(js: &JsFarm) {
    todo!("lifecycle: resume")
}

/// [`Farm::close_intake`](crate::farm::Farm::close_intake).
pub(crate) fn close_intake(js: &JsFarm) {
    todo!("lifecycle: close_intake")
}

/// [`Farm::flush_unsettled`](crate::farm::Farm::flush_unsettled).
pub(crate) async fn flush_unsettled(js: &JsFarm, deadline: Duration) -> Vec<OperationId> {
    todo!("lifecycle: flush_unsettled")
}

/// [`Farm::follow`](crate::farm::Farm::follow).
///
/// # Errors
/// The state store cannot be read.
pub(crate) fn follow(js: &JsFarm) -> Result<usize, StateError> {
    todo!("lifecycle: follow")
}

/// [`Farm::superseded`](crate::farm::Farm::superseded).
pub(crate) fn superseded(js: &JsFarm) -> bool {
    todo!("lifecycle: superseded")
}

/// [`Farm::epoch_ahead`](crate::farm::Farm::epoch_ahead).
pub(crate) fn epoch_ahead(js: &JsFarm) -> Option<u64> {
    todo!("lifecycle: epoch_ahead")
}

/// [`Farm::intake`](crate::farm::Farm::intake).
pub(crate) fn intake(js: &JsFarm) {
    todo!("lifecycle: intake")
}

/// [`Farm::intake_polls`](crate::farm::Farm::intake_polls).
pub(crate) fn intake_polls(js: &JsFarm) -> watch::Receiver<u64> {
    todo!("lifecycle: intake_polls")
}

/// [`Farm::bump_epoch`](crate::farm::Farm::bump_epoch).
///
/// # Errors
/// The store cannot be read or written, or holds no epoch.
pub(crate) fn bump_epoch(js: &JsFarm) -> Result<Option<EpochBump>, StateError> {
    todo!("lifecycle: bump_epoch")
}

/// [`Farm::ended`](crate::farm::Farm::ended).
///
/// # Errors
/// The store cannot be read, or the row is not a result.
pub(crate) fn ended(js: &JsFarm, operation: OperationId) -> Result<Option<Finished>, StateError> {
    todo!("lifecycle: ended")
}

/// [`Farm::place`](crate::farm::Farm::place).
///
/// # Errors
/// As the method.
pub(crate) fn place(
    js: &JsFarm,
    worker: &WorkerId,
    action: NodeAction,
) -> Result<NodeView, PlaceError> {
    todo!("lifecycle: place")
}

/// [`Farm::write_behind_now`](crate::farm::Farm::write_behind_now).
pub(crate) fn write_behind_now(js: &JsFarm) {
    todo!("lifecycle: write_behind_now")
}
