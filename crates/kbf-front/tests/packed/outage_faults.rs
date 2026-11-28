//! A window of a store's outage on the runtime's clock, and the ways a store fails in one: the
//! faults of the outage tests, applied by [`Spy`] to every call that is made inside the window.

use std::io;
use std::sync::{Arc, PoisonError};
use std::time::Duration;

use kbf_objstore::{ObjectKey, ObjectStoreError};
use tokio::time::Instant;

use super::{Spy, injected_with};

/// How the store fails while a window lasts.
#[derive(Clone, Copy, Debug)]
pub enum Fault {
    /// Every call is answered at once by a reset connection.
    Reset,
    /// A call is sent and then the store sends nothing: it ends at the earlier of the window's
    /// end (and then goes on and is answered) and `client_timeout` after it began (and then fails
    /// as a request the client cut does).
    Stall { client_timeout: Duration },
    /// Every call is answered at once 503 `ServiceUnavailable`.
    Unavailable,
    /// A streamed write takes the first piece of its body and then the connection is lost,
    /// storing nothing; every other call is answered as in a store that is up.
    DieMidStream,
    /// A ranged read of an object whose key ends with this is answered at once 503
    /// `ServiceUnavailable`; every other call is answered as in a store that is up.
    UnavailableFor(&'static str),
}

/// A fault, until an instant.
#[derive(Clone, Copy, Debug)]
pub struct Outage {
    until: Instant,
    fault: Fault,
}

/// The failure of a request that got no answer, as the store client reports it.
pub fn transport(kind: io::ErrorKind, what: &str) -> ObjectStoreError {
    ObjectStoreError::Transport(Arc::new(io::Error::new(kind, what.to_owned())))
}

impl Spy {
    /// From now until `window` has passed on the runtime's clock, every call of the store fails
    /// as `fault` says. A window of zero ends the outage.
    pub fn fault_for(&self, window: Duration, fault: Fault) {
        let until = Instant::now() + window;
        *self.outage.lock().unwrap_or_else(PoisonError::into_inner) = Some(Outage { until, fault });
    }

    /// The outage that is on at this instant, if any.
    fn outage_now(&self) -> Option<Outage> {
        let outage = *self.outage.lock().unwrap_or_else(PoisonError::into_inner);
        outage.filter(|o| Instant::now() < o.until)
    }

    /// Whether a streamed write that begins now loses its connection.
    pub(super) fn dies_mid_stream(&self) -> bool {
        matches!(
            self.outage_now(),
            Some(Outage {
                fault: Fault::DieMidStream,
                ..
            })
        )
    }

    /// What an outage does to a ranged read of `key` that begins now: as [`Spy::in_outage`], and
    /// the failure of [`Fault::UnavailableFor`] if the key is the one named.
    pub(super) async fn in_outage_of(&self, key: &ObjectKey) -> Result<(), ObjectStoreError> {
        match self.outage_now() {
            Some(Outage {
                fault: Fault::UnavailableFor(suffix),
                ..
            }) if key.as_str().ends_with(suffix) => Err(injected_with(503)),
            _ => self.in_outage().await,
        }
    }

    /// What an outage does to a call that begins now: the failure it ends in, after the time it
    /// takes on the runtime's clock, or none.
    pub(super) async fn in_outage(&self) -> Result<(), ObjectStoreError> {
        let Some(Outage { until, fault }) = self.outage_now() else {
            return Ok(());
        };
        match fault {
            Fault::Reset => Err(transport(
                io::ErrorKind::ConnectionReset,
                "connection reset",
            )),
            Fault::Unavailable => Err(injected_with(503)),
            Fault::DieMidStream | Fault::UnavailableFor(_) => Ok(()),
            Fault::Stall { client_timeout } => {
                let began = Instant::now();
                if until - began > client_timeout {
                    tokio::time::sleep(client_timeout).await;
                    Err(transport(
                        io::ErrorKind::TimedOut,
                        "the store stopped sending",
                    ))
                } else {
                    tokio::time::sleep_until(until).await;
                    Ok(())
                }
            }
        }
    }
}
