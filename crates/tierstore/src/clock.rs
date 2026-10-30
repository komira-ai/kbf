//! Where the proxy and the reconciler read the time from.

use std::time::{Instant, SystemTime, UNIX_EPOCH};

/// A source of the time, in milliseconds since the Unix epoch.
///
/// The proxy stamps its journal entries with it and judges the age of the usage object by it;
/// the reconciler judges the age of a lease and of a journal object by it. Those ages are
/// differences from the times a backend stamps on its objects, so a clock and the backends it
/// is used with must agree to within the shortest of the ages that are set.
pub trait Clock: Send + Sync {
    /// The time, in milliseconds.
    fn now(&self) -> u64;
}

/// The clock of the machine.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> u64 {
        let since_epoch = SystemTime::now().duration_since(UNIX_EPOCH);
        since_epoch
            .map(|elapsed| elapsed.as_millis() as u64)
            .unwrap_or_default()
    }
}

/// A clock that only goes forward and does not follow the clock of the machine when it is set:
/// the milliseconds since it was made. Its times mean nothing outside the process, so it serves
/// to measure how long something took (the rate of a request body), not to stamp objects.
#[derive(Clone, Copy, Debug)]
pub struct MonotonicClock {
    start: Instant,
}

impl MonotonicClock {
    /// A clock that reads zero now.
    #[must_use]
    pub fn new() -> Self {
        Self {
            start: Instant::now(),
        }
    }
}

impl Default for MonotonicClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for MonotonicClock {
    fn now(&self) -> u64 {
        u64::try_from(self.start.elapsed().as_millis()).unwrap_or(u64::MAX)
    }
}
