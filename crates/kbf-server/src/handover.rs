//! Handing a running frontend's work to a new process of the same build or the next, on
//! one machine, without failing a build (`docs/design/architecture-v2.md`, section 9).
//!
//! Two processes, the holder and the successor, share the listener sockets, the state
//! store and a control socket:
//!
//! 1. The successor connects to the control socket and says what it is started with
//!    (`hello`). A holder refuses a different listen address or state file, or a peer
//!    that is another user's ([`outgoing`]). Nothing is connected, or the connection is
//!    refused: the successor is the only frontend and starts as any start does.
//! 2. The holder offers its three listeners (REAPI, worker, operator API) as
//!    `SCM_RIGHTS` and the state store's generation. It goes on serving. The successor
//!    adopts each socket, checking it listens where its own flags say (so the plain text
//!    and TLS rules of the flags still describe what is served), builds everything that
//!    does not need the store, and answers `ready`. A successor that fails, or goes
//!    away, up to here has changed nothing: the holder is as it was.
//! 3. On `ready` the holder stands down, in this order ([`outgoing::Stand`]): it stops
//!    accepting REAPI calls (GOAWAY; a call that raced it is UNAVAILABLE, which Buck2
//!    asks again) and waits, for a bounded time, for the calls between their request and
//!    their queueing and the results being taken; it fences its farm; it ends the worker
//!    streams and stops the worker and operator listeners, so daemons dial the
//!    successor; and it gives up the state store ([`kbf_state::Store::demote`]: the
//!    writer stops and the file lock is released, the reader stays). It answers
//!    `released`.
//! 4. The successor takes the next generation of the store, expecting exactly the one it
//!    was told ([`crate::StateStoreUri::open_after_handover`]), keeps the epoch (so the
//!    daemons that reconnect keep the runs of their leases), serves from the sockets it
//!    holds (the connections that arrived in the gap wait in their backlog), and, once
//!    its probes pass, tells systemd it is ready ([`notify`]).
//! 5. The holder answers the callers whose streams it kept open from the `done/` rows
//!    the successor writes ([`crate::farm::Farm::follow`]), and exits when its REAPI
//!    connections have closed.
//!
//! The one window where a fault is not harmless is between `released` and the successor
//! serving: if the successor dies there the holder cannot take the store back. The
//! successor is started again as any start is (a cold start bumps the epoch, so daemons
//! rerun their leases), and the holder still answers its callers from the rows.

use std::os::fd::OwnedFd;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use futures::future::BoxFuture;
use tokio::sync::watch;

mod incoming;
pub mod notify;
mod outgoing;
mod ready;
pub mod wire;

pub(crate) use incoming::{Offer, knock};
pub(crate) use outgoing::{Facts, Holder, Stand};
pub(crate) use ready::Ready;
pub use wire::Message;

use crate::farm::StateError;

/// `kbf frontend`'s flags for handing over.
#[derive(Clone, Debug)]
pub struct Handover {
    /// The control socket. A frontend serving binds it; a starting one asks it.
    pub socket: PathBuf,
    /// The longest a holder waits for calls between their request and their queueing, and
    /// for results being taken, before it stands down anyway.
    pub quiesce: Duration,
    /// How often a holder that handed over looks in the state store for the results of
    /// the calls it kept open.
    pub follow_interval: Duration,
}

/// The listener sockets a holder offers and a successor adopts.
#[derive(Debug)]
pub(crate) struct Sockets {
    pub reapi: OwnedFd,
    pub worker: OwnedFd,
    pub api: Option<OwnedFd>,
}

/// Why a handover did not happen.
#[derive(Debug, thiserror::Error)]
pub enum HandoverError {
    /// The control connection failed.
    #[error(transparent)]
    Wire(#[from] wire::WireError),
    /// The control socket could not be used.
    #[error("control socket: {0}")]
    Socket(#[from] std::io::Error),
    /// The frontend serving would not hand over.
    #[error("the serving frontend refused the handover: {0}")]
    Refused(String),
    /// The frontend serving answered with a message this step does not take.
    #[error("unexpected handover message: {0:?}")]
    Unexpected(Message),
    /// The offer did not carry the sockets it announced.
    #[error("the offer carried {got} sockets, not the {want} it announced")]
    Sockets { got: usize, want: usize },
    /// The frontend serving could not stand down.
    #[error("the serving frontend could not stand down: {0}")]
    Failed(String),
    /// A frontend that hands over has no `--state-store`, which the flag requires.
    #[error("a handover needs --state-store")]
    NoState,
    /// The state store failed, or could not be given up.
    #[error("state store: {0}")]
    Store(#[from] kbf_state::StoreError),
    /// The state store could not be taken over.
    #[error(transparent)]
    State(#[from] StateError),
}

/// A signal that, once given, stays given: what stops a listener.
#[derive(Clone, Debug)]
pub(crate) struct Stop(Arc<watch::Sender<bool>>);

impl Stop {
    pub fn new() -> Self {
        Self(Arc::new(watch::Sender::new(false)))
    }

    /// Gives the signal.
    pub fn fire(&self) {
        self.0.send_replace(true);
    }

    /// Completes when the signal is given (immediately if it was).
    pub fn fired(&self) -> BoxFuture<'static, ()> {
        let mut fired = self.0.subscribe();
        // Held by the caller too: the sender outlives every future this returns.
        Box::pin(async move {
            let _ = fired.wait_for(|fired| *fired).await;
        })
    }
}

#[cfg(test)]
mod tests;
