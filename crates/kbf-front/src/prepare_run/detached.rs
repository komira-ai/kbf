//! The writes of a closure walk that the run does not wait for.
//!
//! A walk records the `Directory` bodies it read from blobs as rows of the pack index, and keeps
//! them on the disk half of the [`crate::prepare_run::DirectoryCache`]. Both only make a later
//! walk faster and never decide an answer, so a preparation hands them to a task of its own once
//! the walk is over and returns without them.
//!
//! [`DetachedWrites`] bounds those tasks. Each takes one permit when it is started and gives it
//! back when it ends; a preparation that finds no permit free skips its write and counts the
//! skip ([`DetachedWrites::skipped`]), so a burst of preparations never queues more tasks than
//! there are permits. A skipped write is made by a later walk of the same tree.

use std::sync::Arc;
use std::sync::LazyLock;
use std::sync::atomic::{AtomicU64, Ordering};

use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// The most detached writes of one [`DetachedWrites::shared`] running at once.
pub const DETACHED_WRITES: u16 = 32;

static SHARED: LazyLock<DetachedWrites> = LazyLock::new(|| DetachedWrites::new(DETACHED_WRITES));

/// The bound on the writes that closure walks leave running after they return (the module docs).
#[derive(Clone, Debug)]
pub struct DetachedWrites {
    limit: u16,
    permits: Arc<Semaphore>,
    skipped: Arc<AtomicU64>,
}

impl DetachedWrites {
    /// A bound of `limit` writes at once. With a `limit` of 0 every write is skipped.
    #[must_use]
    pub fn new(limit: u16) -> Self {
        Self {
            limit,
            permits: Arc::new(Semaphore::new(usize::from(limit))),
            skipped: Arc::new(AtomicU64::new(0)),
        }
    }

    /// The bound every walk that is not given one of its own shares: [`DETACHED_WRITES`] writes.
    #[must_use]
    pub fn shared() -> Self {
        SHARED.clone()
    }

    /// How many writes were skipped because every permit was taken, since this bound was made.
    #[must_use]
    pub fn skipped(&self) -> u64 {
        self.skipped.load(Ordering::Relaxed)
    }

    /// Waits until no write of this bound is running. A write started after this returns is not
    /// waited for.
    pub async fn settled(&self) {
        // Every permit is free only when every write has ended. The semaphore is never closed,
        // so the wait ends in permits.
        let _all = self.permits.acquire_many(u32::from(self.limit)).await;
    }

    /// A permit for one write, or `None` (counted as a skip) when every permit is taken.
    pub(crate) fn take(&self) -> Option<OwnedSemaphorePermit> {
        let permit = Arc::clone(&self.permits).try_acquire_owned().ok();
        if permit.is_none() {
            self.skipped.fetch_add(1, Ordering::Relaxed);
        }
        permit
    }
}
