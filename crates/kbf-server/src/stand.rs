//! What a frontend stops to let go of its work, in the order it stops it: the steps of a
//! retire ([`crate::retire`]) and of the end of a drain ([`crate::sequence`]).

use std::sync::Arc;

use futures::future::AbortHandle;
use kbf_front::Backend;

use crate::drain::Drain;
use crate::stop::Stop;

/// Everything a retire stops to let go, in the order it stops it.
#[derive(Clone)]
pub(crate) struct Stand {
    pub drain: Arc<Drain>,
    /// The REAPI front's store, whose metrics count the open Execute and WaitExecution streams.
    pub backend: Backend,
    /// Stops the worker listener.
    pub worker: Stop,
    /// Ends the task that expires the state store's rows.
    pub sweeper: AbortHandle,
}

impl Stand {
    /// Stands down: the drain hands over, the row expiry and the worker listener stop, then
    /// the frontend goes not ready. The REAPI and operator API listeners keep serving, and
    /// the operations this process runs go on until they are answered.
    pub async fn release(&self) {
        tracing::info!("standing down");
        self.drain.hand_over();
        self.sweeper.abort();
        self.worker.fire();
        self.drain.unready();
    }

    /// The Execute and WaitExecution streams open now.
    pub fn open_executions(&self) -> u64 {
        self.backend.metrics().execute.open_executions.get()
    }

    /// The REAPI calls being answered now, open streams among them.
    pub fn open_calls(&self) -> u64 {
        self.backend.metrics().execute.open_calls.get()
    }

    /// Whether the REAPI front still answers anything: an open Execute or WaitExecution
    /// stream, or any call (a ByteStream write or read, a cache call, an Execute still being
    /// prepared) whose answer has not been sent.
    pub fn busy(&self) -> bool {
        self.open_executions() > 0 || self.open_calls() > 0
    }
}

#[cfg(test)]
mod tests;
