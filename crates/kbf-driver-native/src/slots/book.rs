//! Which slots can run a lease: the bookkeeping under [`super::macos::Slots`], with no
//! Mac in it so that it runs under test on every platform.
//!
//! A slot is free, held by one lease, or dirty. A slot is **dirty** when its clean
//! failed ([`Outcome::Dirty`]), or never finished (the clean panicked, or its thread or
//! task never ran): it is not offered to a lease, the node reports one slot fewer
//! ([`Book::count`]), and nothing is written to disk about it. It is cleaned again, and
//! offered again when that clean passes, at the next of these:
//!
//! - a lease gives its slot back ([`Held::give`]), after that slot's own clean, on the
//!   thread that cleans it, so no lease waits for it;
//! - a lease waits for a slot while a dirty one exists ([`Book::take`]), at once and then
//!   every [`RECHECK_EVERY`] while it waits;
//! - the node reports its slots ([`Book::recheck`]), which is the only trigger on a node
//!   whose slots are all dirty: it takes no lease, so nothing else would clean them.
//!
//! One re-clean runs at a time (a second trigger while one runs does nothing).

use std::collections::BTreeSet;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, TryLockError};
use std::time::{Duration, Instant};

use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use super::clean::Outcome;

/// How often a lease that waits for a slot has the dirty ones cleaned again.
pub(crate) const RECHECK_EVERY: Duration = Duration::from_secs(5);

/// A slot, as far as its bookkeeping and its log lines go.
pub(crate) trait Slotted {
    /// The pool user's uid.
    fn uid(&self) -> u32;
}

/// Cleans one slot.
pub(crate) type Cleaner<R> = Arc<dyn Fn(&R) -> Outcome + Send + Sync>;

/// The slots of one pool: `ready[i]` is slot `i`.
pub(crate) struct Book<R> {
    ready: Vec<R>,
    free: Mutex<BTreeSet<usize>>,
    dirty: Mutex<BTreeSet<usize>>,
    /// One permit per free slot.
    permits: Arc<Semaphore>,
    cleaner: Cleaner<R>,
    /// Held while a dirty slot is cleaned again.
    retrying: Mutex<()>,
    /// How often a waiting lease has them cleaned again.
    every: Duration,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    // Each update is one set operation, so the set is consistent after a panic.
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl<R: Slotted + Send + Sync + 'static> Book<R> {
    /// The slots over `ready`, each cleaned by `cleaner` first: a slot whose clean leaves
    /// it dirty is not free.
    pub(crate) fn new(ready: Vec<R>, cleaner: Cleaner<R>) -> Self {
        let book = Self {
            permits: Arc::new(Semaphore::new(0)),
            free: Mutex::default(),
            dirty: Mutex::default(),
            retrying: Mutex::default(),
            every: RECHECK_EVERY,
            ready,
            cleaner,
        };
        for index in 0..book.ready.len() {
            if book.settle(index) {
                lock(&book.free).insert(index);
                book.permits.add_permits(1);
            } else {
                lock(&book.dirty).insert(index);
            }
        }
        book
    }

    /// Slot `index`.
    pub(crate) fn ready(&self, index: usize) -> &R {
        &self.ready[index]
    }

    /// The slots as given.
    pub(crate) fn all(&self) -> &[R] {
        &self.ready
    }

    /// How many slots can run a lease: all but the dirty.
    pub(crate) fn count(&self) -> usize {
        self.ready.len().saturating_sub(lock(&self.dirty).len())
    }

    /// The dirty slots, by index.
    pub(crate) fn dirty(&self) -> BTreeSet<usize> {
        lock(&self.dirty).clone()
    }

    /// Cleans slot `index`, logs how that went, and says whether it is clean.
    fn settle(&self, index: usize) -> bool {
        let slot = &self.ready[index];
        let started = Instant::now();
        let outcome = (self.cleaner)(slot);
        let (uid, took) = (slot.uid(), started.elapsed());
        match &outcome {
            Outcome::Clean => tracing::debug!(index, uid, ?took, "slot cleaned"),
            Outcome::Dirty { why } => {
                tracing::error!(
                    index,
                    uid,
                    ?took,
                    %why,
                    "slot dirty: its clean failed; it is not offered until a clean passes"
                );
            }
        }
        outcome == Outcome::Clean
    }

    /// Cleans every dirty slot but `skip` again; a slot that passes is free (and its
    /// permit back) at once. Does nothing while another call is cleaning.
    pub(crate) fn retry(&self, skip: Option<usize>) {
        let _one_at_a_time = match self.retrying.try_lock() {
            Ok(guard) => guard,
            Err(TryLockError::Poisoned(guard)) => guard.into_inner(),
            Err(TryLockError::WouldBlock) => return,
        };
        let again: Vec<usize> = lock(&self.dirty)
            .iter()
            .copied()
            .filter(|index| Some(*index) != skip)
            .collect();
        for index in again {
            // A clean that panics leaves the slot dirty, as at the first clean.
            let clean = catch_unwind(AssertUnwindSafe(|| self.settle(index)));
            if clean.unwrap_or(false) {
                lock(&self.free).insert(index);
                lock(&self.dirty).remove(&index);
                self.permits.add_permits(1);
                tracing::warn!(index, uid = self.ready[index].uid(), "slot clean again");
            }
        }
    }

    /// Cleans the dirty slots again in the background, if any and if a runtime is there
    /// to run it on: what the node's report triggers.
    pub(crate) fn recheck(self: &Arc<Self>) {
        if lock(&self.dirty).is_empty() {
            return;
        }
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            let book = Arc::clone(self);
            drop(runtime.spawn_blocking(move || book.retry(None)));
        }
    }

    /// A free slot, waiting for one while every slot is held or dirty. While it waits the
    /// dirty slots are cleaned again, now and then every [`RECHECK_EVERY`], so a lease
    /// that needs a slot never waits on a clean nobody is running.
    ///
    /// # Errors
    /// None in practice: the semaphore is never closed, and a permit always finds a
    /// free slot.
    pub(crate) async fn take(self: &Arc<Self>) -> Result<(usize, OwnedSemaphorePermit), String> {
        let permit = loop {
            if self.permits.available_permits() == 0 {
                self.recheck();
            }
            let wait = Arc::clone(&self.permits).acquire_owned();
            if let Ok(permit) = tokio::time::timeout(self.every, wait).await {
                break permit.map_err(|e| format!("slots: {e}"))?;
            }
        };
        let index = lock(&self.free).pop_first();
        let index = index.ok_or_else(|| "slots: none free".to_owned())?;
        Ok((index, permit))
    }

    /// The same book, its waiting leases having the dirty slots cleaned again every
    /// `every`.
    #[cfg(test)]
    pub(crate) fn rechecking_every(mut self, every: Duration) -> Self {
        self.every = every;
        self
    }

    /// Slot `index` and its `permit`, to clean and give back.
    pub(crate) fn held(self: &Arc<Self>, index: usize, permit: OwnedSemaphorePermit) -> Held<R> {
        Held {
            book: Arc::clone(self),
            index,
            permit: Some(permit),
        }
    }
}

/// A held slot, to clean and give back. Dropped before its clean passed (the clean
/// failed, its thread could not start, its task never ran, or the clean panicked), it is
/// dirty: not free again until a clean passes.
pub(crate) struct Held<R: Slotted + Send + Sync + 'static> {
    book: Arc<Book<R>>,
    index: usize,
    /// `None` once the slot is free again.
    permit: Option<OwnedSemaphorePermit>,
}

impl<R: Slotted + Send + Sync + 'static> std::fmt::Debug for Held<R> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Held")
            .field("index", &self.index)
            .finish_non_exhaustive()
    }
}

impl<R: Slotted + Send + Sync + 'static> Held<R> {
    /// Cleans the slot; frees it, or leaves it dirty (on drop). Then cleans the other
    /// dirty slots again, which no lease waits for.
    pub(crate) fn give(mut self) {
        if self.book.settle(self.index) {
            lock(&self.book.free).insert(self.index);
            drop(self.permit.take());
        }
        self.book.retry(Some(self.index));
    }
}

impl<R: Slotted + Send + Sync + 'static> Drop for Held<R> {
    /// Marks the slot dirty unless [`Held::give`] freed it: its permit is forgotten, so
    /// the pool has one permit fewer until a clean passes.
    fn drop(&mut self) {
        if let Some(permit) = self.permit.take() {
            lock(&self.book.dirty).insert(self.index);
            permit.forget();
        }
    }
}

#[cfg(test)]
mod tests;
