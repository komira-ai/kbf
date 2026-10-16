//! The store's clock. Expiry is judged by the store, never by a caller, so a backend owns
//! a [`Clock`] and tests inject a [`ManualClock`] they advance by hand.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// A reading of a store's clock: the time since that clock's origin.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct StoreTime(Duration);

impl StoreTime {
    /// The origin of a clock.
    pub const ZERO: StoreTime = StoreTime(Duration::ZERO);

    /// This reading moved `by` later, saturating at the largest representable time.
    #[must_use]
    pub fn saturating_add(self, by: Duration) -> StoreTime {
        StoreTime(self.0.saturating_add(by))
    }

    /// How long after `earlier` this reading is; zero when `earlier` is not earlier.
    #[must_use]
    pub fn since(self, earlier: StoreTime) -> Duration {
        self.0.saturating_sub(earlier.0)
    }
}

/// A source of [`StoreTime`]. Never goes backwards.
pub trait Clock: Send + Sync {
    /// The current reading.
    fn now(&self) -> StoreTime;
}

/// A clock that moves only when told to. Shared by `Arc` between a test and the store
/// it is injected into.
#[derive(Debug, Default)]
pub struct ManualClock {
    nanos: AtomicU64,
}

impl ManualClock {
    /// Moves the clock forward by `by` (saturating).
    pub fn advance(&self, by: Duration) {
        let by = u64::try_from(by.as_nanos()).unwrap_or(u64::MAX);
        // `fetch_update` with a saturating add; the closure always returns `Some`.
        let _ = self
            .nanos
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
                Some(n.saturating_add(by))
            });
    }
}

impl Clock for ManualClock {
    fn now(&self) -> StoreTime {
        StoreTime(Duration::from_nanos(self.nanos.load(Ordering::SeqCst)))
    }
}
