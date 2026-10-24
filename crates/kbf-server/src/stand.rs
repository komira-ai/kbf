//! What a frontend stops to let go of its work, in the order it stops it: the steps of a
//! retire ([`crate::retire`]).

use std::sync::Arc;
use std::time::Duration;

use futures::future::AbortHandle;
use tonic::Status;

use crate::drain::Drain;
use crate::farm::Farm;
use crate::stop::Stop;

/// The longest a retire waits for the calls between their request and their queueing
/// before it fences the farm anyway.
pub(crate) const QUIESCE_WAIT: Duration = Duration::from_secs(10);

/// How often a retire looks again for the work that keeps it from standing down.
const QUIET_LOOK: Duration = Duration::from_millis(10);

/// What a retire tells a daemon whose stream it ends.
const STANDING_DOWN: &str = "this frontend is standing down; connect again";

/// Everything a retire stops to let go, in the order it stops it.
#[derive(Clone)]
pub(crate) struct Stand {
    pub drain: Arc<Drain>,
    pub farm: Arc<Farm>,
    /// Stops the worker listener.
    pub worker: Stop,
    /// Ends the task that expires the state store's rows.
    pub sweeper: AbortHandle,
    /// The longest to wait for the farm to be [quiet](Farm::quiet), and again the
    /// longest to write the requeues the store refused ([`Farm::flush_unsettled`]).
    pub wait: Duration,
}

impl Stand {
    /// Stands down for a retire: the drain hands over, the farm is quiesced and fenced, the
    /// row expiry and the worker listener stop and the worker streams end, then the
    /// frontend goes not ready. The REAPI and operator API listeners keep serving; the
    /// callers this process holds are answered from the `done/` rows ([`Farm::follow`]) the
    /// other frontends of the state store write.
    pub async fn release(&self) {
        tracing::info!("standing down");
        self.give_up().await;
        self.drain.unready();
    }

    /// The steps of [`Stand::release`] before the frontend goes not ready.
    async fn give_up(&self) {
        self.drain.hand_over();
        self.quiesce().await;
        self.sweeper.abort();
        self.worker.fire();
        self.farm
            .end_worker_streams(&Status::unavailable(STANDING_DOWN));
    }

    /// Waits, for the bounded time, for the calls between their request and their
    /// queueing and the results being taken, then fences the farm (it feeds the
    /// scheduler no input, assigns, grants, requeues and queues nothing) and writes the
    /// requeues the store refused.
    async fn quiesce(&self) {
        let _ = tokio::time::timeout(self.wait, self.until_quiet()).await;
        self.farm.fence();
        // A requeue the store refused is written by nothing else once the farm is fenced;
        // the frontends that recover its operation would find the lease it gave up. The
        // budget is the quiet wait's, again.
        self.farm.flush_unsettled(self.wait).await;
    }

    async fn until_quiet(&self) {
        while !self.farm.quiet() {
            tokio::time::sleep(QUIET_LOOK).await;
        }
    }
}

#[cfg(test)]
mod tests;
