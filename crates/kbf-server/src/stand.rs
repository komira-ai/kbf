//! What a frontend stops to let go of its work, in the order it stops it: the steps a
//! handover ([`crate::handover`], step 3) and a retire ([`crate::retire`]) share.

use std::sync::Arc;
use std::time::Duration;

use futures::future::AbortHandle;
use kbf_state::{Store, StoreError};
use tonic::Status;

use crate::drain::Drain;
use crate::farm::Farm;
use crate::stop::Stop;

/// How often a holder looks again for the work that keeps it from standing down.
const QUIET_LOOK: Duration = Duration::from_millis(10);

/// What the holder tells a daemon whose stream it ends.
const STANDING_DOWN: &str = "this frontend is standing down; connect again";

/// Everything a holder stops to let go, in the order it stops it.
#[derive(Clone)]
pub(crate) struct Stand {
    pub drain: Arc<Drain>,
    pub farm: Arc<Farm>,
    /// Stops the worker listener.
    pub worker: Stop,
    /// Ends the task that expires the state store's rows.
    pub sweeper: AbortHandle,
    pub store: Arc<dyn Store>,
    /// The longest to wait for the farm to be [quiet](Farm::quiet), and again the
    /// longest to write the requeues the store refused ([`Farm::flush_unsettled`]).
    pub wait: Duration,
}

impl Stand {
    /// Stands down: see the module docs of [`crate::handover`], step 3. The listeners that
    /// answer Envoy (REAPI, operator API) are not stopped, and readiness is not touched: the
    /// holder stays ready until its successor has taken Envoy's streams.
    ///
    /// # Errors
    /// The state store cannot give up its writer role. The farm is fenced and the worker
    /// listener is stopped by then, so the drain is closed: the process only has its open
    /// streams to end.
    pub async fn run(&self) -> Result<(), StoreError> {
        tracing::info!("standing down for a successor");
        self.give_up().await;
        let demoted = self.store.demote();
        if demoted.is_err() {
            self.drain.close();
        }
        demoted
    }

    /// The steps of [`Stand::run`] before the state store is given up, which a
    /// stand-down that follows a takeover elsewhere does not take: the store is fenced
    /// to the new writer already, and a backend with no writer role to give up (Postgres)
    /// has nothing to demote.
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
    /// requeues the store refused. [`Farm::resume`] undoes it.
    pub async fn quiesce(&self) {
        let _ = tokio::time::timeout(self.wait, self.until_quiet()).await;
        self.farm.fence();
        // A requeue the store refused is written by nothing else once the farm is fenced;
        // the successor would recover its operation under the lease it gave up. The
        // budget is the quiet wait's, again.
        self.farm.flush_unsettled(self.wait).await;
    }

    /// Stands down after another process took the generation (`POST stand-down`): the
    /// steps of [`Stand::run`] without giving up the store, then not ready. The REAPI
    /// and operator API listeners keep serving; the callers this process holds are
    /// answered from the `done/` rows ([`Farm::follow`]). A retire ([`crate::retire`]) takes
    /// the same steps over a state store the other frontends keep writing.
    pub async fn release(&self) {
        tracing::info!("standing down");
        self.give_up().await;
        self.drain.unready();
    }

    async fn until_quiet(&self) {
        while !self.farm.quiet() {
            tokio::time::sleep(QUIET_LOOK).await;
        }
    }
}

#[cfg(test)]
mod tests;
