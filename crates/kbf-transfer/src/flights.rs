//! In-memory fetches shared by every caller of one digest
//! (`docs/design/worker-cache.md#43-api`, "Duplicates are merged in memory").
//!
//! The first caller of a digest starts a **flight**: a task that waits for the byte
//! budget and a request permit, reads the blob and publishes the outcome. Every caller
//! that asks for the digest while the flight is in the map waits on it instead of
//! fetching again, from any lease. Only the flight's own task takes it out of the map:
//! when it lands (publishes), or when it gives up.
//!
//! A caller that stops waiting (its lease dropped) leaves the flight. When the last
//! waiter leaves, the task is told, and gives up unless more than half of the blob has
//! arrived: then it finishes, for a caller that joins before it lands (a flight that
//! lands with no waiter keeps nothing)
//! (`docs/design/worker-cache.md#44-limits-retries-cancellation`). Giving up drops the
//! read where it is, so a request that was only queued never starts. Joining and giving
//! up both happen under the map's lock, so no caller joins a flight that has given up.

use std::collections::HashMap;
use std::future::Future;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use kbf_proto::reapi::Digest;
use tokio::sync::{Notify, watch};

use crate::cas::{CasError, label};

/// What a flight publishes: the blob's verified bytes, shared by its waiters.
pub(crate) type Outcome = Result<Arc<Vec<u8>>, CasError>;

/// The flights in progress, by digest.
#[derive(Debug, Default)]
pub(crate) struct Flights {
    map: Mutex<HashMap<String, Arc<Flight>>>,
}

#[derive(Debug)]
pub(crate) struct Flight {
    /// Callers waiting on it; changed only under the map's lock.
    waiters: AtomicUsize,
    done: watch::Receiver<Option<Outcome>>,
    /// Notified when the last waiter leaves.
    deserted: Notify,
    /// Bytes of the blob received so far, of `size`.
    pub arrived: AtomicU64,
    size: u64,
}

/// A flight as its task holds it: where to count what arrives, and where to publish.
pub(crate) struct Publish {
    key: String,
    pub flight: Arc<Flight>,
    done: watch::Sender<Option<Outcome>>,
}

impl Flight {
    /// Returns once the last waiter has left (perhaps before this was called).
    pub async fn deserted(&self) {
        self.deserted.notified().await;
    }
}

impl Flights {
    /// Waits on the flight of `digest`, starting one with `start` when there is none.
    /// Also returns whether the wait joined a flight that was already in progress.
    pub fn join<F>(&self, digest: &Digest, start: impl FnOnce(Publish) -> F) -> (Waiter<'_>, bool)
    where
        F: Future<Output = ()> + Send + 'static,
    {
        let key = label(digest);
        let mut map = self.lock();
        if let Some(flight) = map.get(&key) {
            flight.waiters.fetch_add(1, Ordering::SeqCst);
            let flight = Arc::clone(flight);
            return (self.waiter(flight), true);
        }
        let (done, published) = watch::channel(None);
        let flight = Arc::new(Flight {
            waiters: AtomicUsize::new(1),
            done: published,
            deserted: Notify::new(),
            arrived: AtomicU64::new(0),
            size: u64::try_from(digest.size_bytes).unwrap_or(0),
        });
        map.insert(key.clone(), Arc::clone(&flight));
        let publish = Publish {
            key,
            flight: Arc::clone(&flight),
            done,
        };
        // The task cannot touch the map before this returns: that takes the lock held
        // here.
        tokio::spawn(start(publish));
        (self.waiter(flight), false)
    }

    /// Publishes `outcome` and takes the flight out of the map: a caller from now on
    /// starts a new one.
    pub fn land(&self, publish: Publish, outcome: Outcome) {
        let mut map = self.lock();
        publish.done.send_replace(Some(outcome));
        map.remove(&publish.key);
    }

    /// Takes the flight out of the map if it has no waiters and no more than half of
    /// its blob has arrived; returns whether it did, so the task stops.
    pub fn give_up(&self, publish: &Publish) -> bool {
        let mut map = self.lock();
        let flight = &publish.flight;
        let idle = flight.waiters.load(Ordering::SeqCst) == 0;
        let half_done = flight.arrived.load(Ordering::SeqCst).saturating_mul(2) > flight.size;
        // Both read under the lock; neither has a side effect.
        if idle & !half_done {
            map.remove(&publish.key);
            return true;
        }
        false
    }

    /// Flights in progress.
    pub fn len(&self) -> usize {
        self.lock().len()
    }

    fn waiter(&self, flight: Arc<Flight>) -> Waiter<'_> {
        Waiter {
            flights: self,
            flight,
        }
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<String, Arc<Flight>>> {
        // Every change is one map operation or one counter, consistent after a panic.
        self.map.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// One caller's wait on a flight. Dropped, it leaves it.
pub(crate) struct Waiter<'a> {
    flights: &'a Flights,
    flight: Arc<Flight>,
}

impl Waiter<'_> {
    /// The flight's outcome. A flight that ended without one (its task dropped by a
    /// runtime shutting down, or panicked) is [`CasError::Unavailable`].
    pub async fn outcome(&self, digest: &Digest) -> Outcome {
        let mut done = self.flight.done.clone();
        let landed = match done.wait_for(Option::is_some).await {
            Ok(outcome) => outcome.clone(),
            Err(_) => None,
        };
        match landed {
            Some(outcome) => outcome,
            None => Err(CasError::Unavailable(
                label(digest),
                "the fetch ended without an answer".to_owned(),
            )),
        }
    }
}

impl Drop for Waiter<'_> {
    fn drop(&mut self) {
        let _map = self.flights.lock();
        if self.flight.waiters.fetch_sub(1, Ordering::SeqCst) == 1 {
            self.flight.deserted.notify_one();
        }
    }
}
