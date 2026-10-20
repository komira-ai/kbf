//! What the supervisor's status is read from: the newest intent record the swap thread
//! wrote, kept in memory beside `update.json`.

use std::sync::{Condvar, Mutex, MutexGuard, PoisonError};

use super::super::model::{CrashPoint, Halt, Intent, Slot};
use super::super::ports::Recorder;

/// State shared by the API's thread (which asks for the status) and the swap thread.
pub(super) struct Shared {
    state: Mutex<State>,
    ended: Condvar,
}

pub(super) struct State {
    /// The newest intent record: the update that is open, or the last one that ended.
    pub intent: Option<Intent>,
    /// The slot that serves when no update is open. The swap thread sets it when it ends.
    pub active: Slot,
    /// The id of the update whose swap thread is alive.
    pub open: Option<String>,
}

impl Shared {
    pub fn new(active: Slot, intent: Option<Intent>, open: Option<String>) -> Shared {
        Shared {
            state: Mutex::new(State {
                intent,
                active,
                open,
            }),
            ended: Condvar::new(),
        }
    }

    pub fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The swap thread is over: no update is open any more.
    pub fn close(&self) {
        self.lock().open = None;
        self.ended.notify_all();
    }

    /// Wait until no swap thread is alive, for at most `limit`.
    pub fn wait_closed(&self, limit: std::time::Duration) -> bool {
        let guard = self.lock();
        let (guard, _) = self
            .ended
            .wait_timeout_while(guard, limit, |s| s.open.is_some())
            .unwrap_or_else(PoisonError::into_inner);
        guard.open.is_none()
    }
}

/// The swap's recorder: the durable one first, then the copy the status reads. A record
/// the durable recorder refused is not shown, so the status never runs ahead of `update.json`.
pub(super) struct Mirror<'a> {
    pub inner: &'a dyn Recorder,
    pub shared: &'a Shared,
}

impl Recorder for Mirror<'_> {
    fn record(&self, intent: &Intent) -> Result<(), Halt> {
        self.inner.record(intent)?;
        self.shared.lock().intent = Some(intent.clone());
        Ok(())
    }

    fn crash_point(&self, point: CrashPoint) -> Result<(), Halt> {
        self.inner.crash_point(point)
    }
}
