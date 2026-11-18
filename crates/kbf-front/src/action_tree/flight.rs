//! Manifest writes in flight, shared by every preparation: a preparation that needs a manifest
//! another preparation is writing waits for that write instead of writing it again.
//!
//! A manifest is named by its content, so two writes of one digest write the same bytes to the
//! same key; one is enough. Many preparations of one build share subtrees (a toolchain), and
//! until the first of them has stored a subtree no memo names it, so without this each would
//! write it, the store taking one create-only write per preparation of every shared manifest.
//!
//! A write is entered under its manifest's digest when the first preparation asks for it, and
//! taken out by whichever of its waiters sees it end, stored or failed: a write that failed is
//! made anew by the next preparation that asks. A waiter dropped part way (its preparation ran
//! out of time) leaves the write to the others; the last one dropped takes the write out and
//! drops it, its request with it, so nothing is held for a preparation that is gone, and the
//! next preparation that asks makes it anew.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use bytes::Bytes;
use futures::FutureExt as _;
use futures::future::{BoxFuture, Shared};
use kbf_types::Digest;

use super::NoTree;
use super::place::store_object;
use super::retry::PutRetry;
use crate::large_backfill::{LargeStore, large_key};

/// One write in flight: its end, the store's answer (`why` it failed) for every waiter.
type Flight = Shared<BoxFuture<'static, Result<(), String>>>;

/// The writes in flight, by manifest digest.
#[derive(Default)]
pub(super) struct Flights {
    writes: Mutex<BTreeMap<Digest, Flight>>,
}

impl Flights {
    /// Writes the manifest `digest` (`bytes`) to `cas/l/` through `store`, repeated by `retry`,
    /// or waits for the write of it already in flight.
    ///
    /// # Errors
    /// [`NoTree::Put`]: the store refused the write, this waiter's or the one it waited for.
    pub(super) async fn write(
        &self,
        store: &Arc<dyn LargeStore>,
        retry: &PutRetry,
        digest: Digest,
        bytes: Bytes,
    ) -> Result<(), NoTree> {
        let flight = self
            .lock()
            .entry(digest)
            .or_insert_with(|| {
                let (store, retry) = (Arc::clone(store), retry.clone());
                async move {
                    let key = large_key(&digest);
                    let stored = store_object(&*store, &retry, &key, bytes).await;
                    stored.map_err(|error| error.to_string())
                }
                .boxed()
                .shared()
            })
            .clone();
        let mut waiter = Waiter {
            flights: self,
            digest,
            flight,
            ended: false,
        };
        // A clone is awaited: the waiter's own is never polled, so it still names the write
        // when the waiter is dropped. Dropped part way, the clone goes first (it was made last).
        let answer = waiter.flight.clone().await;
        waiter.ended = true;
        answer.map_err(|why| NoTree::Put {
            key: large_key(&digest).to_string(),
            why,
        })
    }

    fn lock(&self) -> MutexGuard<'_, BTreeMap<Digest, Flight>> {
        self.writes.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// One preparation's wait for a write. Dropped, it takes the write out of [`Flights`] if the
/// write ended (whichever waiter sees the end first takes it out) or if no other waiter is left
/// (the entry and this waiter's own, unpolled copy are its only holders).
struct Waiter<'a> {
    flights: &'a Flights,
    digest: Digest,
    flight: Flight,
    ended: bool,
}

impl Drop for Waiter<'_> {
    fn drop(&mut self) {
        let mut writes = self.flights.lock();
        let ours = writes
            .get(&self.digest)
            .is_some_and(|entered| entered.ptr_eq(&self.flight));
        if ours && (self.ended || self.flight.strong_count() == Some(2)) {
            writes.remove(&self.digest);
        }
    }
}
