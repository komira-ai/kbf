//! The farm's state store and lifecycle on the js core: the handover (fence, resume, close
//! intake, flush, follow), the intake poll, the epoch, operator placement, and the reads of
//! the store.
//!
//! Each function is the js arm of the [`Farm`](crate::farm::Farm) method of the same name
//! (`farm/handover.rs`, `farm/durable.rs`, `farm/durable/intake.rs`, `farm.rs`), with the
//! same arguments after `js`. What the farm owns before it serves, the core's construction
//! from the settings and the rebuild around a state store, is in `farm/js/mod.rs`.
//!
//! `Farm::poll_intake` and `Farm::follow_every` have no arm: they only call
//! [`Farm::intake`](crate::farm::Farm::intake) and [`Farm::follow`](crate::farm::Farm::follow)
//! on a timer, whichever core the farm runs on.

use std::time::Duration;

use js_server_core::{FLUSH_LOOK, NodeAction as CoreAction, PlaceError as CorePlaceError};
use kbf_front::Finished;
use kbf_sched::Cordon;
use kbf_types::{FarmTime, OperationId, WorkerId};
use tokio::sync::watch;

use super::{JsFarm, views};
use crate::farm::{EpochBump, NodeAction, NodeUnread, PlaceError, StateError, UnknownNode};
use crate::fleet::{
    NodeView, PhaseView, PlacementView, ResourcesView, needs_attention, shown_deadline,
};

/// [`Farm::fence`](crate::farm::Farm::fence): [`Core::fence`](js_server_core::Core::fence).
pub(crate) fn fence(js: &JsFarm) {
    js.core.fence();
}

/// [`Farm::resume`](crate::farm::Farm::resume): [`Core::resume`](js_server_core::Core::resume).
pub(crate) fn resume(js: &JsFarm) {
    js.core.resume();
}

/// [`Farm::close_intake`](crate::farm::Farm::close_intake):
/// [`Core::close_intake`](js_server_core::Core::close_intake).
pub(crate) fn close_intake(js: &JsFarm) {
    js.core.close_intake();
}

/// [`Farm::flush_unsettled`](crate::farm::Farm::flush_unsettled).
///
/// The core's wait is [`FLUSH_LOOK`] on tokio's clock, and the deadline is checked before the
/// wait, so a farm past its deadline does not sleep once more.
pub(crate) async fn flush_unsettled(js: &JsFarm, deadline: Duration) -> Vec<OperationId> {
    let until = tokio::time::Instant::now() + deadline;
    js.core
        .flush_unsettled(|| async move {
            if tokio::time::Instant::now() >= until {
                return false;
            }
            tokio::time::sleep(FLUSH_LOOK).await;
            true
        })
        .await
}

/// [`Farm::follow`](crate::farm::Farm::follow).
///
/// # Errors
/// The state store cannot be read.
pub(crate) fn follow(js: &JsFarm) -> Result<usize, StateError> {
    Ok(js.core.follow()?)
}

/// [`Farm::superseded`](crate::farm::Farm::superseded).
pub(crate) fn superseded(js: &JsFarm) -> bool {
    js.core.superseded()
}

/// [`Farm::epoch_ahead`](crate::farm::Farm::epoch_ahead).
pub(crate) fn epoch_ahead(js: &JsFarm) -> Option<u64> {
    js.core.epoch_ahead()
}

/// [`Farm::intake`](crate::farm::Farm::intake): one poll of the core at farm time now, then
/// the count of polls the core has run to the end is shown to those who wait for it. The
/// count is sent after the poll returns, so a reader of count `n` knows poll `n` is over.
pub(crate) fn intake(js: &JsFarm) {
    js.core.intake(js.now());
    let polls = js.core.intake_polls();
    js.polls.send_if_modified(|shown| {
        let changed = *shown != polls;
        *shown = polls;
        changed
    });
}

/// [`Farm::intake_polls`](crate::farm::Farm::intake_polls).
pub(crate) fn intake_polls(js: &JsFarm) -> watch::Receiver<u64> {
    js.polls.subscribe()
}

/// [`Farm::bump_epoch`](crate::farm::Farm::bump_epoch).
///
/// # Errors
/// The store cannot be read or written, or holds no epoch.
pub(crate) fn bump_epoch(js: &JsFarm) -> Result<Option<EpochBump>, StateError> {
    let bumped = js.core.bump_epoch()?;
    Ok(bumped.map(|bump| EpochBump {
        previous: bump.previous,
        epoch: bump.epoch,
    }))
}

/// [`Farm::ended`](crate::farm::Farm::ended): the core answers with the bytes of the `done/`
/// row, which are read as the result they hold.
///
/// # Errors
/// The store cannot be read, or the row is not a result.
pub(crate) fn ended(js: &JsFarm, operation: OperationId) -> Result<Option<Finished>, StateError> {
    let Some(row) = js.core.ended(operation)? else {
        return Ok(None);
    };
    Finished::from_row(&row)
        .map(Some)
        .map_err(|why| StateError::Corrupt {
            key: format!("{}{:016x}", js_store::keys::DONE, operation.0),
            why,
        })
}

/// [`Farm::place`](crate::farm::Farm::place): the core applies the action, then the node is
/// shown as it stands after.
///
/// # Errors
/// As the method.
pub(crate) fn place(
    js: &JsFarm,
    worker: &WorkerId,
    action: NodeAction,
) -> Result<NodeView, PlaceError> {
    let action = match action {
        NodeAction::Cordon => CoreAction::Cordon,
        NodeAction::Drain(within) => CoreAction::Drain(within),
        NodeAction::Uncordon => CoreAction::Uncordon,
    };
    js.core
        .place(js.now(), worker, action)
        .map_err(|refused| match refused {
            CorePlaceError::Unknown(unknown) => UnknownNode(unknown.0).into(),
            CorePlaceError::Unread(unread) => NodeUnread(unread.0).into(),
            CorePlaceError::Unwritten(node) => PlaceError::Unwritten(node),
        })?;
    // A node another frontend of the state store holds is not registered here: it is shown
    // as the scheduler has it, which is what the action just fed.
    Ok(views::node_view(js, worker).unwrap_or_else(|| elsewhere(js, worker)))
}

/// `worker`, which is not registered here, as the scheduler shows it: placement, phase and
/// leases, with nothing of a stream or software.
fn elsewhere(js: &JsFarm, worker: &WorkerId) -> NodeView {
    js.core.with(|view, _| {
        let sched = view.sched();
        let wall = |at: FarmTime| js.epoch_unix_ms.saturating_add(at.as_millis());
        let held = sched.leases_on(worker);
        let leases = || held.iter().map(ToString::to_string).collect();
        let placement = match sched.cordon(worker) {
            None => PlacementView::Serving,
            Some(Cordon::Cordoned) => PlacementView::Cordoned,
            Some(Cordon::Draining { deadline }) => PlacementView::Draining {
                deadline_unix_ms: shown_deadline(wall(*deadline)),
                leases: leases(),
            },
            Some(Cordon::Drained) => PlacementView::Drained,
            Some(Cordon::Paused { deadline }) => PlacementView::DrainPaused {
                deadline_unix_ms: shown_deadline(wall(*deadline)),
                leases: leases(),
            },
        };
        let phase = sched.phase(worker).cloned().unwrap_or_default();
        NodeView {
            node_id: worker.as_str().to_owned(),
            connected: false,
            protocol: None,
            needs_attention: needs_attention(None, &phase),
            software: None,
            placement,
            phase: PhaseView::new(&phase),
            lease_count: u32::try_from(held.len()).unwrap_or(u32::MAX),
            booked: ResourcesView::of(sched.booked(worker).unwrap_or_default()),
        }
    })
}

/// [`Farm::write_behind_now`](crate::farm::Farm::write_behind_now).
///
/// Nothing to write: the farm's write-behind is the learned size summaries, which this core
/// does not keep (its store does not read or write sizes). The core's own write-behind, the
/// effects of records the store has not taken ([`Core::write_behind`]), is retried by the
/// write that follows and not on request.
///
/// [`Core::write_behind`]: js_server_core::Core::write_behind
pub(crate) fn write_behind_now(_js: &JsFarm) {}

#[cfg(test)]
#[path = "lifecycle_tests.rs"]
mod tests;
