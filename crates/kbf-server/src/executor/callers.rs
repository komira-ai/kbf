//! An operation no client waits on any more is cancelled, once none has come back for a grace
//! period (`--executor-no-caller-grace-secs`).
//!
//! A caller is a receiver of the operation's stage ([`crate::executor::ops`]): an Execute or
//! WaitExecution stream holds one while it is open, a GetOperation or CancelOperation for the
//! length of its call. A stream whose client has gone is dropped once its connection is closed:
//! at once when the connection ends, and for a client that went silent once the listener's
//! watch closes the connection (`--http2-keepalive-interval-secs` and
//! `--http2-keepalive-timeout-secs` together, two and a half minutes by default:
//! [`crate::conn_watch`]). Over a state store, a caller on another frontend counts too: that
//! frontend keeps an `exec/watch/` row of the operation alive while it has one
//! ([`crate::executor::rows`]).
//!
//! Each open operation has a watch of its own, a task that sleeps while a caller is on it. Once
//! the last one has gone it starts the grace; a join (an Execute of the same action, a
//! WaitExecution or GetOperation of the operation) ends the grace, and the next leaving starts a
//! new one. Over a state store the rows of callers on other frontends are read every
//! `--executor-live-secs` of the grace, and each one found starts it again. A grace that runs out
//! cancels the operation as a CancelOperation does ([`Ops::cancel_unwatched`]): its task cancels
//! its job at the job service and answers it CANCELLED, which a later WaitExecution by name sees.
//! The cancel is counted (`kbf_executor_operations_cancelled_no_caller_total`) and logged, one
//! line. An operation taken up from a gone process starts with no caller: its grace starts when
//! it is taken up.
//!
//! What a grace does not touch: a job that ended before the grace ran out is read and its
//! result written to the action cache when the action may be cached, as for any job, whether a
//! caller is there to take the answer or not; and an operation that has been cancelled is not
//! joined, so an Execute of its action after the cancel opens a new operation.

use std::sync::Arc;

use kbf_types::{ActionKey, OperationId};
use tokio::time::{Instant, sleep_until};

use super::Inner;
use super::ops::{Callers, Unwatched};
use super::run::on_rows;

/// Starts the watch of `callers`, of the open `operation` of `key` ([`super::ops::Ops::callers`],
/// none when it is not open): none with the grace off (zero).
pub(crate) fn spawn(
    inner: &Arc<Inner>,
    operation: OperationId,
    key: ActionKey,
    callers: Option<Callers>,
) {
    if inner.cfg.no_caller_grace.is_zero() {
        return;
    }
    if let Some(callers) = callers {
        tokio::spawn(watch(Arc::clone(inner), operation, key, callers));
    }
}

/// What became of a grace.
enum Grace {
    /// A caller came back: the operation is watched again.
    Joined,
    /// The operation is no longer open here, or it was cancelled: the watch ends.
    Over,
}

/// Watches the callers of `operation` until it is no longer open here.
async fn watch(inner: Arc<Inner>, operation: OperationId, key: ActionKey, mut callers: Callers) {
    loop {
        tokio::select! {
            biased;
            joined = callers.joins.changed() => match joined {
                Ok(()) => continue,
                Err(_) => return,
            },
            () = callers.stage.closed() => {}
        }
        let alone = Alone::of(&inner);
        let grace = alone_for(&inner, operation, &key, &mut callers).await;
        drop(alone);
        match grace {
            Grace::Joined => {}
            Grace::Over => return,
        }
    }
}

/// The grace of `operation`, which no caller of this process is on: until a caller joins, or it
/// runs out and the operation is cancelled.
async fn alone_for(
    inner: &Arc<Inner>,
    operation: OperationId,
    key: &ActionKey,
    callers: &mut Callers,
) -> Grace {
    let grace = inner.cfg.no_caller_grace;
    // Over a state store the rows of callers on other frontends are read this often.
    let step = inner
        .rows
        .as_ref()
        .map_or(grace, |_| inner.cfg.live.min(grace));
    // A join since the last caller left is not seen yet, and ends the grace at once.
    let mut since = Instant::now();
    loop {
        let wake = (since + grace).min(Instant::now() + step);
        tokio::select! {
            biased;
            joined = callers.joins.changed() => return match joined {
                Ok(()) => Grace::Joined,
                Err(_) => Grace::Over,
            },
            () = sleep_until(wake) => {}
        }
        if watched_elsewhere(inner, operation).await {
            since = Instant::now();
            continue;
        }
        if Instant::now() < since + grace {
            continue;
        }
        return match inner.ops().cancel_unwatched(operation) {
            Unwatched::Watched => Grace::Joined,
            Unwatched::Gone => Grace::Over,
            Unwatched::Cancelled => {
                inner.deps.metrics.cancelled_no_caller();
                tracing::info!(
                    %operation,
                    instance = %key.instance,
                    action = %key.action,
                    grace_secs = grace.as_secs(),
                    "cancelling an operation no Execute or WaitExecution has waited on for the grace"
                );
                Grace::Over
            }
        };
    }
}

/// Whether a caller on another frontend of the state store waits on `operation`. A store that
/// cannot be read says yes: an operation is not cancelled on what is not known.
async fn watched_elsewhere(inner: &Arc<Inner>, operation: OperationId) -> bool {
    match on_rows(inner, move |rows| rows.watched(operation)).await {
        Some(Ok(watched)) => watched,
        Some(Err(error)) => {
            tracing::warn!(%operation, %error, "the callers of an operation on other frontends could not be read: it is not cancelled now");
            true
        }
        // No state store, no other frontend; with one, the read's task broke.
        None => inner.rows.is_some(),
    }
}

/// One operation counted among those with no caller (`kbf_executor_operations_no_caller`) while
/// it is held.
struct Alone(Arc<Inner>);

impl Alone {
    fn of(inner: &Arc<Inner>) -> Self {
        inner.deps.metrics.no_caller(true);
        Self(Arc::clone(inner))
    }
}

impl Drop for Alone {
    fn drop(&mut self) {
        self.0.deps.metrics.no_caller(false);
    }
}

/// Keeps, for a follower of `operation` (an operation another process runs), the
/// `exec/watch/` row of this process alive while a caller of this process waits on it: renewed
/// every quarter of `--executor-live-secs`, with that time to live. `renewed` is when it was
/// last written. Called at each read of the follower's operation.
pub(crate) async fn follow_watched(
    inner: &Arc<Inner>,
    operation: OperationId,
    renewed: &mut Option<Instant>,
) {
    let live = inner.cfg.live;
    let due = renewed.is_none_or(|at| at.elapsed() >= live / 4);
    if !due || inner.ops().followers(operation) == 0 {
        return;
    }
    match on_rows(inner, move |rows| rows.watch(operation, live)).await {
        Some(Ok(())) => *renewed = Some(Instant::now()),
        Some(Err(error)) => {
            tracing::warn!(%operation, %error, "the row of this frontend's callers of an operation could not be written");
        }
        None => {}
    }
}

#[cfg(test)]
#[path = "callers_tests.rs"]
mod tests;
