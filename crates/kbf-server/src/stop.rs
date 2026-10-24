//! The stop signal: a signal that, once given, stays given, and closes the listeners
//! registered with it as it is given.

use std::sync::{Arc, Mutex, PoisonError};

use futures::future::BoxFuture;
use tokio::sync::watch;

use crate::listen::Latch;

/// A signal that, once given, stays given: what stops a listener. The listeners it is
/// to close are closed as it is given, not when the server next looks at them.
#[derive(Clone)]
pub(crate) struct Stop(Arc<Signal>);

struct Signal {
    given: watch::Sender<bool>,
    latches: Mutex<Vec<Latch>>,
}

impl Stop {
    pub fn new() -> Self {
        Self(Arc::new(Signal {
            given: watch::Sender::new(false),
            latches: Mutex::new(Vec::new()),
        }))
    }

    /// Gives the signal: closes the listeners registered with [`Stop::closes`], and
    /// completes every [`Stop::fired`].
    pub fn fire(&self) {
        let mut latches = self
            .0
            .latches
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        self.0.given.send_replace(true);
        latches.drain(..).for_each(|latch| latch.close());
    }

    /// Has `latch` close its listener when the signal is given, at once if it was.
    pub fn closes(&self, latch: Latch) {
        let mut latches = self
            .0
            .latches
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        latches.push(latch.clone());
        if *self.0.given.borrow() {
            latch.close();
        }
    }

    /// Completes when the signal is given (immediately if it was).
    pub fn fired(&self) -> BoxFuture<'static, ()> {
        let mut fired = self.0.given.subscribe();
        // Held by the caller too: the sender outlives every future this returns.
        Box::pin(async move {
            let _ = fired.wait_for(|fired| *fired).await;
        })
    }
}

#[cfg(test)]
mod tests;
