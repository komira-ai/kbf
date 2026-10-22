//! Which slots can run a lease: the bookkeeping under [`super::macos::Slots`], with no
//! Mac in it so that it runs under test on every platform.
//!
//! A slot is free, held by one lease, or dirty. A slot is **dirty** when its clean
//! failed ([`Outcome::Dirty`]), or never finished (the clean panicked, or its thread or
//! task never ran): it is not offered to a lease, the node reports one slot fewer
//! ([`Book::count`]), and nothing is written to disk about it.
//!
//! A dirty slot is cleaned again, and offered when that clean passes, by the **recovery
//! thread** ([`Book::start`]): one thread of its own, with no runtime and no lease
//! needed, so a node whose every slot is dirty (it takes no lease, and its report does
//! not change, so nothing else would clean them) comes back by itself. While any slot is
//! dirty it cleans the dirty ones, one at a time, after a wait that starts at
//! [`Backoff::first`], doubles after a pass that frees nothing up to [`Backoff::max`], and
//! starts again after a pass that frees a slot. Whenever the number of free slots changes
//! the book calls the hook ([`Book::on_count`]), which is how the node's report reaches
//! the daemon without waiting for anything else to change.
//!
//! A slot that stays dirty for the same reason logs it at `ERROR` once, then at `DEBUG`
//! until the reason changes.

use std::collections::{BTreeMap, BTreeSet};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError, Weak};
use std::time::{Duration, Instant};

use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use super::clean::Outcome;

/// How long the recovery thread waits between passes, and how that grows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Backoff {
    /// The wait before the first pass, and after a pass that freed a slot.
    pub first: Duration,
    /// The longest wait.
    pub max: Duration,
}

impl Backoff {
    /// The daemon's: 5 s, doubling to 5 min.
    pub(crate) const DAEMON: Self = Self {
        first: Duration::from_secs(5),
        max: Duration::from_secs(300),
    };

    /// The wait after a pass that waited `waited`: `first` when the pass freed a slot
    /// (`freed`), else double, up to `max`.
    pub(crate) fn next(self, waited: Duration, freed: bool) -> Duration {
        if freed {
            self.first
        } else {
            waited.saturating_mul(2).min(self.max)
        }
    }
}

/// How long the idle recovery thread sleeps before it looks whether the book is gone.
const IDLE: Duration = Duration::from_secs(60);

/// A slot, as far as its bookkeeping and its log lines go.
pub(crate) trait Slotted {
    /// The pool user's uid.
    fn uid(&self) -> u32;
}

/// Cleans one slot.
pub(crate) type Cleaner<R> = Arc<dyn Fn(&R) -> Outcome + Send + Sync>;

/// Called with the number of free slots whenever it changes.
pub(crate) type CountHook = Arc<dyn Fn(usize) + Send + Sync>;

/// What wakes the recovery thread: a slot went dirty, or the book is gone.
#[derive(Default)]
struct Wake {
    state: Mutex<WakeState>,
    cv: Condvar,
}

#[derive(Default)]
struct WakeState {
    dirty: bool,
    closed: bool,
}

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
    wake: Arc<Wake>,
    hook: Mutex<Option<CountHook>>,
    /// Why each dirty slot last failed, to log a repeat quietly.
    reasons: Mutex<BTreeMap<usize, String>>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    // Each update is one set operation, so the set is consistent after a panic.
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl<R: Slotted + Send + Sync + 'static> Book<R> {
    /// The slots over `ready`, each cleaned by `cleaner` first (a slot whose clean leaves
    /// it dirty is not free), and the recovery thread started with `backoff`.
    pub(crate) fn start(ready: Vec<R>, cleaner: Cleaner<R>, backoff: Backoff) -> Arc<Self> {
        let book = Arc::new(Self {
            permits: Arc::new(Semaphore::new(0)),
            free: Mutex::default(),
            dirty: Mutex::default(),
            retrying: Mutex::default(),
            wake: Arc::default(),
            hook: Mutex::default(),
            reasons: Mutex::default(),
            ready,
            cleaner,
        });
        for index in 0..book.ready.len() {
            if book.settle(index) {
                lock(&book.free).insert(index);
                book.permits.add_permits(1);
            } else {
                lock(&book.dirty).insert(index);
            }
        }
        let weak = Arc::downgrade(&book);
        let wake = Arc::clone(&book.wake);
        // A thread that cannot be made (out of memory) leaves dirty slots dirty, as the
        // clean leaves them.
        drop(
            std::thread::Builder::new()
                .name("kbf-slot-recovery".to_owned())
                .spawn(move || recover(&weak, &wake, backoff)),
        );
        book.wake_up();
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

    /// Calls `hook` with the number of free slots now, and again whenever it changes.
    pub(crate) fn on_count(&self, hook: CountHook) {
        *lock(&self.hook) = Some(Arc::clone(&hook));
        hook(self.count());
    }

    /// Tells the hook, if any, how many slots are free now.
    fn changed(&self) {
        let hook = lock(&self.hook).clone();
        if let Some(hook) = hook {
            hook(self.count());
        }
    }

    /// The thread's side of the wake-up, to see it end.
    #[cfg(test)]
    fn wake_handle(&self) -> Arc<Wake> {
        Arc::clone(&self.wake)
    }

    /// Wakes the recovery thread: a slot is dirty.
    fn wake_up(&self) {
        lock(&self.wake.state).dirty = true;
        self.wake.cv.notify_all();
    }

    /// Records that slot `index` failed for `why`; whether that is news (a first failure,
    /// or another reason than the last).
    pub(crate) fn note_failure(&self, index: usize, why: &str) -> bool {
        let mut reasons = lock(&self.reasons);
        if reasons.get(&index).is_some_and(|last| last == why) {
            return false;
        }
        reasons.insert(index, why.to_owned());
        true
    }

    /// Cleans slot `index`, logs how that went, and says whether it is clean.
    fn settle(&self, index: usize) -> bool {
        let slot = &self.ready[index];
        let started = Instant::now();
        let outcome = (self.cleaner)(slot);
        let (uid, took) = (slot.uid(), started.elapsed());
        match &outcome {
            Outcome::Clean => {
                lock(&self.reasons).remove(&index);
                tracing::debug!(index, uid, ?took, "slot cleaned");
            }
            Outcome::Dirty { why } if self.note_failure(index, why) => {
                tracing::error!(
                    index,
                    uid,
                    ?took,
                    %why,
                    "slot dirty: its clean failed; it is not offered until a clean passes"
                );
            }
            Outcome::Dirty { why } => {
                tracing::debug!(index, uid, ?took, %why, "slot still dirty for the same reason");
            }
        }
        outcome == Outcome::Clean
    }

    /// Cleans every dirty slot again, one at a time; a slot that passes is free (and its
    /// permit back) at once. Does nothing while another call is cleaning. How many slots
    /// it freed.
    pub(crate) fn retry(&self) -> usize {
        // Nothing panics while it is held (the clean is called under `catch_unwind`), so it
        // is never poisoned: a failed `try_lock` is another re-clean running.
        let Ok(_one_at_a_time) = self.retrying.try_lock() else {
            return 0;
        };
        let again: Vec<usize> = lock(&self.dirty).iter().copied().collect();
        let mut freed = 0;
        for index in again {
            // A clean that panics leaves the slot dirty, as at the first clean.
            let clean = catch_unwind(AssertUnwindSafe(|| self.settle(index)));
            if clean.unwrap_or(false) {
                lock(&self.free).insert(index);
                lock(&self.dirty).remove(&index);
                self.permits.add_permits(1);
                tracing::warn!(index, uid = self.ready[index].uid(), "slot clean again");
                freed += 1;
                self.changed();
            }
        }
        freed
    }

    /// A free slot, waiting for one while every slot is held or dirty. The dirty slots
    /// are cleaned again meanwhile by the recovery thread, not by this call.
    ///
    /// # Errors
    /// None in practice: the semaphore is never closed, and a permit always finds a
    /// free slot.
    pub(crate) async fn take(self: &Arc<Self>) -> Result<(usize, OwnedSemaphorePermit), String> {
        let permit = Arc::clone(&self.permits).acquire_owned().await;
        let permit = permit.map_err(|e| format!("slots: {e}"))?;
        let index = lock(&self.free).pop_first();
        let index = index.ok_or_else(|| "slots: none free".to_owned())?;
        Ok((index, permit))
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

impl<R> Drop for Book<R> {
    fn drop(&mut self) {
        lock(&self.wake.state).closed = true;
        self.wake.cv.notify_all();
    }
}

/// The recovery thread: while a slot is dirty, wait, clean the dirty ones again, and
/// wait longer if none came back. Ends once the book is gone (dropping it wakes the
/// thread, whose next look finds nothing to upgrade).
fn recover<R: Slotted + Send + Sync + 'static>(
    book: &Weak<Book<R>>,
    wake: &Arc<Wake>,
    backoff: Backoff,
) {
    let mut wait = backoff.first;
    // Whether the wait before the next pass is over.
    let mut due = false;
    while let Some(strong) = book.upgrade() {
        let idle = lock(&strong.dirty).is_empty();
        if idle || !due {
            // Not held while asleep: the book must be able to go.
            drop(strong);
            if idle {
                wait = backoff.first;
                due = false;
                sleep_until_dirty(wake);
            } else {
                sleep(wake, wait);
                due = true;
            }
            continue;
        }
        let freed = strong.retry();
        drop(strong);
        wait = backoff.next(wait, freed > 0);
        due = false;
    }
}

/// Sleeps until a slot is dirty or the book is gone (or [`IDLE`] has passed).
fn sleep_until_dirty(wake: &Wake) {
    let state = lock(&wake.state);
    let mut state = wake
        .cv
        .wait_timeout_while(state, IDLE, |s| !s.dirty && !s.closed)
        .unwrap_or_else(PoisonError::into_inner)
        .0;
    state.dirty = false;
}

/// Sleeps `time`, or until the book is gone.
fn sleep(wake: &Wake, time: Duration) {
    let state = lock(&wake.state);
    drop(
        wake.cv
            .wait_timeout_while(state, time, |s| !s.closed)
            .unwrap_or_else(PoisonError::into_inner),
    );
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
    /// Cleans the slot; frees it, or leaves it dirty (on drop).
    pub(crate) fn give(mut self) {
        if self.book.settle(self.index) {
            lock(&self.book.free).insert(self.index);
            drop(self.permit.take());
        }
    }
}

impl<R: Slotted + Send + Sync + 'static> Drop for Held<R> {
    /// Marks the slot dirty unless [`Held::give`] freed it: its permit is forgotten, so
    /// the pool has one permit fewer until a clean passes, and the recovery thread and the
    /// hook are told.
    fn drop(&mut self) {
        if let Some(permit) = self.permit.take() {
            lock(&self.book.dirty).insert(self.index);
            permit.forget();
            self.book.changed();
            self.book.wake_up();
        }
    }
}

#[cfg(test)]
mod tests;
