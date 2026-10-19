//! The real [`Clock`] and [`Requests`] of the supervisor.
//!
//! The clock sleeps on a condition variable, so the proxy shutting down ends a wait of the
//! swap (a bake of minutes, a drain with no deadline) at once with `Halt::Interrupted`
//! instead of at the end of the sleep. The request queue is what the update API's actor
//! (U8) pushes `abort` and `rollback` into and the swap reads.

use std::collections::VecDeque;
use std::sync::{Condvar, Mutex, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use super::model::{Halt, Request};
use super::ports::{Clock, Requests};

/// Wall-clock time that a shutdown can cut short.
#[derive(Default)]
pub struct SystemClock {
    stopped: Mutex<bool>,
    wake: Condvar,
}

impl SystemClock {
    pub fn new() -> SystemClock {
        SystemClock::default()
    }

    /// End every wait, now and later: the proxy is going away.
    pub fn stop(&self) {
        *self.stopped.lock().unwrap_or_else(PoisonError::into_inner) = true;
        self.wake.notify_all();
    }
}

impl Clock for SystemClock {
    fn now(&self) -> Duration {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
    }

    fn sleep(&self, d: Duration) -> Result<(), Halt> {
        let stopped = self.stopped.lock().unwrap_or_else(PoisonError::into_inner);
        let (stopped, _) = self
            .wake
            .wait_timeout_while(stopped, d, |stopped| !*stopped)
            .unwrap_or_else(PoisonError::into_inner);
        if *stopped {
            Err(Halt::Interrupted)
        } else {
            Ok(())
        }
    }
}

/// Operator requests in the order they came; each is read once.
#[derive(Default)]
pub struct RequestQueue {
    queue: Mutex<VecDeque<Request>>,
}

impl RequestQueue {
    pub fn new() -> RequestQueue {
        RequestQueue::default()
    }

    pub fn push(&self, request: Request) {
        self.queue
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push_back(request);
    }
}

impl Requests for RequestQueue {
    fn next(&self) -> Option<Request> {
        self.queue
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .pop_front()
    }
}

#[cfg(test)]
#[path = "runtime_tests.rs"]
mod tests;
