//! Which slots can run a lease: the bookkeeping under [`super::macos::Slots`], with no
//! Mac in it so that it runs under test on every platform.
//!
//! A slot is free, held by one lease, or dirty. A slot is **dirty** when its clean
//! failed ([`Outcome::Dirty`]), its slot-ready check (the assertion or the profile
//! canaries) failed ([`Book::set_aside`]), or it never finished (the clean panicked, or
//! its thread or task never ran): it is not offered to a lease, the node reports one slot
//! fewer ([`Book::count`]), and nothing is written to disk about it.
//!
//! A dirty slot is cleaned again, and offered when that clean passes, by the **recovery
//! thread** ([`Book::start`]): one thread of its own, with no runtime and no lease
//! needed, so a node whose every slot is dirty (it takes no lease, and its report does
//! not change, so nothing else would clean them) comes back by itself. It is the only
//! thing that cleans a dirty slot again. While any slot is dirty it cleans the dirty ones,
//! one at a time, after a wait that starts at [`Backoff::first`], doubles after a pass
//! that frees nothing up to [`Backoff::max`], and starts again from [`Backoff::first`]
//! after a pass that frees a slot and whenever another slot goes dirty. Whenever the
//! number of slots that can run a lease changes the book calls the hook
//! ([`Book::on_count`]) with it, which is how the node's report reaches the daemon without
//! waiting for anything else to change; the count and the call are made under one lock,
//! so the last call always carries the newest count.
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

/// Called with the number of slots that can run a lease whenever it changes.
pub(crate) type CountHook = Arc<dyn Fn(usize) + Send + Sync>;

/// How the recovery thread waits `time` between passes, and whether a slot went dirty
/// meanwhile. The daemon's is [`sleep`]: it waits until `time` has passed or the wake is
/// signalled. A test's records `time` and returns when the test steps it.
type Nap = Box<dyn FnMut(&Wake, Duration) -> bool + Send>;

/// What wakes the recovery thread: a slot went dirty, or the book is gone.
#[derive(Default)]
struct Wake {
    state: Mutex<WakeState>,
    cv: Condvar,
}

#[derive(Default)]
struct WakeState {
    /// A slot went dirty since the thread last looked.
    news: bool,
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

/// How many slots the first clean of the book works on at once.
const START_WORKERS: usize = 8;

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    // Each update is one set operation, so the set is consistent after a panic.
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl<R: Slotted + Send + Sync + 'static> Book<R> {
    /// The slots over `ready`, each cleaned by `cleaner` first (a slot whose clean leaves
    /// it dirty is not free), and the recovery thread started with `backoff`.
    pub(crate) fn start(ready: Vec<R>, cleaner: Cleaner<R>, backoff: Backoff) -> Arc<Self> {
        Self::start_with(ready, cleaner, backoff, None, Box::new(sleep))
    }

    /// [`Book::start`], the recovery thread given a stack of `stack` bytes if any (a test
    /// asks for one that cannot be made) and waiting by `nap` (a test asks for the waits
    /// instead of sitting through them).
    fn start_with(
        ready: Vec<R>,
        cleaner: Cleaner<R>,
        backoff: Backoff,
        stack: Option<usize>,
        nap: Nap,
    ) -> Arc<Self> {
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
        book.settle_all();
        let weak = Arc::downgrade(&book);
        let wake = Arc::clone(&book.wake);
        // A thread that cannot be made (out of memory) leaves dirty slots dirty, as the
        // clean leaves them, and is logged.
        let mut thread = std::thread::Builder::new().name("kbf-slot-recovery".to_owned());
        if let Some(bytes) = stack {
            thread = thread.stack_size(bytes);
        }
        if let Err(why) = thread.spawn(move || recover(&weak, &wake, backoff, nap)) {
            tracing::error!(%why, "no thread to clean dirty slots again: they stay dirty");
        }
        book.wake_up();
        book
    }

    /// Cleans every slot once, up to [`START_WORKERS`] at a time (the first clean of a slot
    /// also records its baseline, which launches a command as its user, so one after the
    /// other would take as many such launches as there are slots), and logs how long all
    /// took.
    fn settle_all(&self) {
        let started = std::time::Instant::now();
        let next = std::sync::atomic::AtomicUsize::new(0);
        let work = || {
            loop {
                let index = next.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if index >= self.ready.len() {
                    break;
                }
                // A clean that panics leaves the slot dirty, as at every later clean.
                let clean = catch_unwind(AssertUnwindSafe(|| self.settle(index)));
                if clean.unwrap_or(false) {
                    lock(&self.free).insert(index);
                    self.permits.add_permits(1);
                } else {
                    lock(&self.dirty).insert(index);
                }
            }
        };
        std::thread::scope(|scope| {
            for _ in 1..START_WORKERS.min(self.ready.len()) {
                scope.spawn(work);
            }
            work();
        });
        tracing::info!(
            slots = self.ready.len(),
            dirty = lock(&self.dirty).len(),
            took = ?started.elapsed(),
            "the slots are cleaned"
        );
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

    /// Calls `hook` with the number of slots that can run a lease now, and again whenever
    /// it changes.
    pub(crate) fn on_count(&self, hook: CountHook) {
        let mut slot = lock(&self.hook);
        hook(self.count());
        *slot = Some(hook);
    }

    /// Tells the hook, if any, how many slots can run a lease now. The count is read and
    /// the hook called under the hook's lock: of two changes at once, the one that takes
    /// the lock second reads the newer count and calls last, so an older count never
    /// overwrites a newer one at the receiver.
    fn changed(&self) {
        if let Some(hook) = lock(&self.hook).as_ref() {
            // A hook that panics must not end the recovery thread, or the lease that
            // called this: it is logged, and the next change calls it again.
            let called = catch_unwind(AssertUnwindSafe(|| hook(self.count())));
            if called.is_err() {
                tracing::error!("the hook told of the slot count panicked");
            }
        }
    }

    /// The thread's side of the wake-up, to see it end.
    #[cfg(test)]
    fn wake_handle(&self) -> Arc<Wake> {
        Arc::clone(&self.wake)
    }

    /// Wakes the recovery thread: a slot went dirty.
    fn wake_up(&self) {
        lock(&self.wake.state).news = true;
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
            Outcome::Dirty { why } => self.log_dirty(index, uid, "clean", why, Some(took)),
        }
        outcome == Outcome::Clean
    }

    /// Logs that slot `index` is dirty because `failed` ("clean" or "slot-ready check")
    /// failed for `why`: at `ERROR` the first time for that reason, then at `DEBUG`. One
    /// shape for both ways a slot goes dirty.
    fn log_dirty(&self, index: usize, uid: u32, failed: &str, why: &str, took: Option<Duration>) {
        if self.note_failure(index, &format!("{failed}: {why}")) {
            tracing::error!(
                index,
                uid,
                ?took,
                failed,
                %why,
                "slot dirty: it is not offered until a clean passes"
            );
        } else {
            tracing::debug!(index, uid, ?took, failed, %why, "slot still dirty for the same reason");
        }
    }

    /// Marks the taken slot `index` (its `permit`) dirty because its check at slot-ready
    /// (the assertion, or the profile canaries) failed for `why`, as a failed clean does:
    /// its permit is forgotten (one slot fewer is offered and counted), the hook is told
    /// and the recovery thread cleans it again. Its checks run again at the lease it is
    /// next handed to.
    pub(crate) fn set_aside(
        self: &Arc<Self>,
        index: usize,
        permit: OwnedSemaphorePermit,
        why: &str,
    ) {
        self.log_dirty(
            index,
            self.ready[index].uid(),
            "slot-ready check",
            why,
            None,
        );
        drop(self.held(index, permit));
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
    mut nap: Nap,
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
                sleep_until_news(wake);
            } else if nap(wake, wait) {
                // Another slot went dirty: its wait starts now, from the first.
                wait = backoff.first;
            } else {
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

/// Sleeps until a slot goes dirty or the book is gone (or [`IDLE`] has passed).
fn sleep_until_news(wake: &Wake) {
    let state = lock(&wake.state);
    let mut state = wake
        .cv
        .wait_timeout_while(state, IDLE, |s| !s.news && !s.closed)
        .unwrap_or_else(PoisonError::into_inner)
        .0;
    state.news = false;
}

/// Sleeps `time`, or until a slot goes dirty or the book is gone; whether a slot went dirty.
fn sleep(wake: &Wake, time: Duration) -> bool {
    let state = lock(&wake.state);
    let (mut state, _) = wake
        .cv
        .wait_timeout_while(state, time, |s| !s.news && !s.closed)
        .unwrap_or_else(PoisonError::into_inner);
    std::mem::take(&mut state.news)
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
