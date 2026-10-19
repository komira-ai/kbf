//! [`Released`]: a watch on a [`FsStore`](crate::FsStore)'s close.
//!
//! A store's directory lock is held by the store's shared state, which lives until the
//! last handle (a clone of the `FsStore`) is dropped. Whoever stops a node and starts
//! another over the same directory has to wait for that, and the handles are not all
//! theirs to drop: they sit in tasks and threads of the cache above the store. In the
//! node those are the touch flusher of `kbf-front` (`cache/touches.rs`: aborted when the
//! cache drops, gone only when the runtime next polls it) and the write-pool threads
//! (`cache/pool.rs`: they end after their queue closes). A watch that is itself a handle
//! would keep the store open; this one holds a weak reference, and a flag the store's
//! `Drop` sets after it unlocks.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Weak;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tokio::sync::Notify;

/// Set, then woken, when the store's directory has been unlocked.
///
/// The flag is what a waiter trusts. The strong count of the handles reaches zero
/// before the shared state's `Drop` runs, so a count of zero says the unlock is about to
/// happen, not that it has.
#[derive(Default)]
pub(crate) struct Gate {
    open: AtomicBool,
    notify: Notify,
    /// Runs inside the unlock step, just before the unlock itself: the place a test looks
    /// at the store while the directory is still locked (`testkit`).
    #[cfg(feature = "testkit")]
    before_unlock: std::sync::OnceLock<Box<dyn Fn() + Send + Sync>>,
}

impl Gate {
    /// Runs `unlock`, then opens the gate and wakes the waiters: called by the store's
    /// `Drop`. Never the other way round: a waiter that wakes restarts over the
    /// directory at once.
    pub(crate) fn release(&self, unlock: impl FnOnce()) {
        #[cfg(feature = "testkit")]
        let unlock = || {
            if let Some(hook) = self.before_unlock.get() {
                hook();
            }
            unlock();
        };
        unlock();
        self.open.store(true, Ordering::Release);
        self.notify.notify_waiters();
    }

    fn is_open(&self) -> bool {
        self.open.load(Ordering::Acquire)
    }
}

/// A store still open when the wait for its close ended.
#[derive(Debug, thiserror::Error)]
#[error("{}: still open after {limit:?}, held by {clones} live handle(s)", .dir.display())]
pub struct StillHeld {
    /// The store's directory.
    pub dir: PathBuf,
    /// How many handles on the store were alive when the wait ended.
    pub clones: usize,
    /// How long the wait was.
    pub limit: Duration,
}

/// Completes when every handle on a store is gone and its directory is unlocked.
/// Holds no handle itself ([`FsStore::released`](crate::FsStore::released)).
pub struct Released {
    holders: Weak<dyn Send + Sync>,
    gate: Arc<Gate>,
    dir: PathBuf,
}

impl Released {
    pub(crate) fn new(holders: Weak<dyn Send + Sync>, gate: Arc<Gate>, dir: PathBuf) -> Self {
        Self { holders, gate, dir }
    }

    /// Runs `hook` on the thread that drops the store's last handle, after the store has
    /// decided to release and before it unlocks: what a test uses to observe the watch
    /// while the directory is still locked. Set once; a second call is ignored.
    #[cfg(feature = "testkit")]
    pub fn before_unlock(&self, hook: impl Fn() + Send + Sync + 'static) {
        let _ = self.gate.before_unlock.set(Box::new(hook));
    }

    /// Waits at most `limit` until the store's last handle is dropped and its directory
    /// unlocked. Returns at once for a store that is already closed. The limit is on the
    /// runtime's clock, so a test with paused time decides it.
    ///
    /// # Errors
    /// [`StillHeld`] when a handle is alive after `limit`.
    pub async fn wait_within(self, limit: Duration) -> Result<(), StillHeld> {
        self.wait_hooked(limit, || {}).await
    }

    /// [`Released::wait_within`], running `between` after the check finds the store open
    /// and before the wait starts: the place a release must not be lost in.
    async fn wait_hooked(self, limit: Duration, between: impl FnOnce()) -> Result<(), StillHeld> {
        let mut between = Some(between);
        let wait = async {
            loop {
                let woken = self.gate.notify.notified();
                tokio::pin!(woken);
                // Registered before the check: a release between the two wakes this future.
                woken.as_mut().enable();
                if self.gate.is_open() {
                    return;
                }
                if let Some(between) = between.take() {
                    between();
                }
                // A wake that finds the gate still shut is not a release: look again.
                woken.await;
            }
        };
        tokio::time::timeout(limit, wait)
            .await
            .map_err(|_| StillHeld {
                dir: self.dir.clone(),
                clones: self.holders.strong_count(),
                limit,
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A watch over `holder`, with the gate its drop would open.
    fn watch(holder: &Arc<()>) -> (Released, Arc<Gate>) {
        let gate = Arc::new(Gate::default());
        let weak: Weak<()> = Arc::downgrade(holder);
        let holders: Weak<dyn Send + Sync> = weak;
        (
            Released::new(holders, Arc::clone(&gate), PathBuf::from("/store")),
            gate,
        )
    }

    /// Catches a gate that opens, or wakes a waiter, before the unlock has run: at the
    /// moment `unlock` runs, the gate is shut and a registered waiter is not woken; after
    /// `release` both have happened. Mutant: `release` opens and notifies before `unlock`.
    #[tokio::test(start_paused = true)]
    async fn the_gate_opens_after_the_unlock() {
        let holder = Arc::new(());
        let (released, gate) = watch(&holder);
        let mut wait = Box::pin(released.wait_within(Duration::from_secs(5)));
        assert!(futures::poll!(wait.as_mut()).is_pending());
        let mut unlocked = false;
        gate.release(|| {
            assert!(!gate.is_open(), "open before the unlock");
            assert!(
                futures::FutureExt::now_or_never(wait.as_mut()).is_none(),
                "a waiter woken before the unlock"
            );
            unlocked = true;
        });
        assert!(unlocked);
        wait.await.expect("released");
    }

    /// Catches a wait that registers after its check: a release that lands between the
    /// two is then never seen and the wait runs to its limit. The seam opens the gate
    /// exactly there, so the answer does not depend on a race. Mutant: `enable()` and the
    /// `notified()` it enables moved below `between()`.
    #[tokio::test(start_paused = true)]
    async fn a_release_between_the_check_and_the_wait_is_not_lost() {
        let holder = Arc::new(());
        let (released, gate) = watch(&holder);
        let start = tokio::time::Instant::now();
        released
            .wait_hooked(Duration::from_secs(5), || gate.release(|| {}))
            .await
            .expect("woken by the release, not timed out");
        assert_eq!(start.elapsed(), Duration::ZERO);
    }

    /// The same, over a real store: the last handle is dropped by the seam between the
    /// check and the wait, so the release (flag, wake) lands exactly there. A wait that
    /// registered after its check hangs, and the limit turns that into an error.
    #[tokio::test(start_paused = true)]
    async fn the_last_handle_dropped_between_the_check_and_the_wait_is_seen() {
        let dir = std::env::temp_dir().join(format!("kbf-released-r3-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = crate::FsStore::open(&dir, crate::Floor::Bytes(1)).expect("open");
        let released = store.released();
        released
            .wait_hooked(Duration::from_secs(5), move || drop(store))
            .await
            .expect("woken by the drop, not timed out");
    }

    /// Catches a wait that returns on any wake: one that finds the gate shut (a wake sent
    /// before the unlock, or by anything else) goes back to sleep, and the release that
    /// follows ends it. Mutant: the loop is gone and the first wake returns.
    #[tokio::test(start_paused = true)]
    async fn a_wake_with_the_gate_shut_is_not_a_release() {
        let holder = Arc::new(());
        let (released, gate) = watch(&holder);
        let mut wait = Box::pin(released.wait_within(Duration::from_secs(5)));
        assert!(futures::poll!(wait.as_mut()).is_pending());
        gate.notify.notify_waiters();
        assert!(
            futures::poll!(wait.as_mut()).is_pending(),
            "returned on a wake with the gate shut"
        );
        gate.release(|| {});
        wait.await.expect("released");
    }

    /// Catches a wait that trusts the handle count: it reaches zero before the store's
    /// `Drop` has unlocked. With every handle gone and the gate shut, the wait is not over.
    #[tokio::test(start_paused = true)]
    async fn no_handles_left_but_the_gate_shut_is_not_released() {
        let holder = Arc::new(());
        let (released, _gate) = watch(&holder);
        drop(holder);
        let held = released
            .wait_within(Duration::from_secs(3))
            .await
            .expect_err("the unlock has not happened");
        assert_eq!(held.clones, 0);
    }
}
