//! Handing a running frontend's work to a new process of the same build or the next, on
//! one machine, behind Envoy, without failing a build
//! (`docs/design/aio-handover.md`, sections 5 and 8; `docs/design/frontend-handover.md`).
//!
//! Two processes, the holder and the successor, run side by side: each binds its own
//! listeners (`--instance a|b` puts instance b's ports [`Instance::SECOND_OFFSET`] above
//! instance a's, the block Envoy's static configuration names), and they share a state
//! file and a control socket. Envoy sends new streams to whichever is ready.
//!
//! 1. The successor binds its listeners and connects to the control socket, saying what
//!    it is started with (`hello`: protocol, instance, where its REAPI listener is, state
//!    file). A holder refuses the same instance, another state file or protocol, a REAPI
//!    address that is not loopback, a peer that is another user's ([`outgoing`]).
//!    Nothing is connected, or the connection is refused: the successor is the only
//!    frontend and starts as any start does.
//! 2. The holder answers `offer` with the state store's generation, and goes on serving.
//!    The successor builds everything that does not need the store and answers `ready`.
//!    A successor that fails, or goes away, up to here has changed nothing.
//! 3. On `ready` the holder stands down, in this order ([`outgoing::Stand`]): it hands
//!    over ([`crate::Drain::hand_over`]: it stays ready, and its readiness stops asking
//!    the stores it is giving up), waits, for a bounded time, for the calls between their
//!    request and their queueing and the results being taken; fences its farm (from then
//!    on an `Execute` that reaches it is forwarded to the successor,
//!    [`kbf_front::Relay`]); ends the worker streams and stops the worker listener, so
//!    daemons dial the successor; and gives up the state store
//!    ([`kbf_state::Store::demote`]: the writer stops and the file lock is released, the
//!    reader stays). It answers `released`. Its REAPI and operator API listeners keep
//!    serving: Envoy still sends it streams until it sees the successor.
//! 4. The successor takes the next generation of the store, expecting exactly the one it
//!    was told ([`crate::StateStoreUri::open_after_handover`]), keeps the epoch (so the
//!    daemons that reconnect keep the runs of their leases), serves, and, once its
//!    probes pass, tells systemd it is ready ([`notify`]) and the holder it is `serving`.
//! 5. The holder, on `serving`, waits the settle (Envoy's check interval, threshold and
//!    timeout: [`EnvoyCheck::settle`]) so Envoy has seen the successor, and goes not
//!    ready ([`crate::Drain::unready`]). Envoy then sends it no new stream. It answers
//!    the callers whose streams it kept open from the `done/` rows the successor writes
//!    ([`crate::farm::Farm::follow`]) and the relays it holds, and exits when it holds
//!    neither and has been out of Envoy's rotation for the settle.
//!
//! The one window where a fault is not harmless is between `released` and the successor
//! serving: if the successor dies there the holder cannot take the store back. It goes
//! not ready itself, at once when the control connection closes and after
//! `--handover-watchdog-secs` when the successor is alive but never serves, so Envoy
//! answers cleanly instead of choosing a holder that can answer nothing new. The
//! successor is started again as any start is (a cold start bumps the epoch, so daemons
//! rerun their leases), and the holder still answers its callers from the rows.

use std::path::PathBuf;
use std::str::FromStr;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use futures::future::BoxFuture;
use tokio::sync::watch;

use crate::listen::Latch;

mod incoming;
mod notify;
mod outgoing;
mod ready;
pub(crate) mod wire;

pub(crate) use incoming::{Released, knock};
pub(crate) use outgoing::{Dial, Facts, Holder, Leave, Peer, Stand};
pub(crate) use ready::Ready;
pub use wire::Message;

use crate::farm::StateError;

/// Which of the two instances of a frontend this process is: the two run side by side
/// during an update, on port blocks [`Instance::SECOND_OFFSET`] apart.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Instance {
    A,
    B,
}

impl Instance {
    /// How far instance b's ports are above instance a's.
    pub const SECOND_OFFSET: u16 = 10;

    /// The letter that names it, on the command line and on the wire.
    #[must_use]
    pub const fn letter(self) -> char {
        match self {
            Self::A => 'a',
            Self::B => 'b',
        }
    }

    /// The port this instance listens on where the flags say `port`: instance a's block is
    /// the flags', instance b's is [`Instance::SECOND_OFFSET`] above. Port 0 (the system
    /// chooses) stays 0. `None` when the port runs past 65535.
    #[must_use]
    pub fn port(self, port: u16) -> Option<u16> {
        match self {
            Self::A => Some(port),
            Self::B if port == 0 => Some(0),
            Self::B => port.checked_add(Self::SECOND_OFFSET),
        }
    }
}

impl FromStr for Instance {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, String> {
        match text {
            "a" => Ok(Self::A),
            "b" => Ok(Self::B),
            other => Err(format!("{other:?} is not an instance: it is a or b")),
        }
    }
}

/// Envoy's health-check settings for the frontend's cluster, which decide how long Envoy
/// takes to see a host healthy or to stop choosing one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EnvoyCheck {
    /// The time between two checks of a host.
    pub interval: Duration,
    /// How long a check waits for an answer.
    pub timeout: Duration,
    /// How many checks in a row fail before the host is not chosen.
    pub unhealthy_threshold: u32,
}

impl EnvoyCheck {
    /// The time that passes between a host changing its answer and Envoy having acted on
    /// it: `interval x unhealthy_threshold + timeout`, plus one interval for the check
    /// that has to come first. A holder waits it before it goes not ready (Envoy has then
    /// seen the successor) and again before it exits (Envoy then sends it nothing).
    #[must_use]
    pub fn settle(&self) -> Duration {
        self.interval
            .saturating_mul(self.unhealthy_threshold)
            .saturating_add(self.timeout)
            .saturating_add(self.interval)
    }
}

/// `kbf frontend`'s flags for handing over.
#[derive(Clone, Debug)]
pub struct Handover {
    /// The control socket. A frontend serving binds it; a starting one asks it.
    pub socket: PathBuf,
    /// Which instance this is: a holder hands over only to the other one. Without it this
    /// frontend refuses every handover (and the unit cannot update).
    pub instance: Option<Instance>,
    /// The longest a holder waits for calls between their request and their queueing, and
    /// for results being taken, before it stands down anyway.
    pub quiesce: Duration,
    /// How often a holder that handed over looks in the state store for the results of
    /// the calls it kept open.
    pub follow_interval: Duration,
    /// How long after `released` a holder waits for its successor to say it serves before
    /// it goes not ready itself.
    pub watchdog: Duration,
    /// Envoy's health-check settings, which the settle comes from.
    pub envoy: EnvoyCheck,
    /// The DNS name the successor's REAPI certificate carries, for a holder that forwards
    /// calls to it over TLS (the REAPI listener has a certificate).
    pub peer_name: Option<String>,
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
    /// The frontend serving answered with a message this step does not take (its line).
    #[error("unexpected handover message: {0}")]
    Unexpected(String),
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
