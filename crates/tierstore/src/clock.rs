//! Where the proxy and the reconciler read the time from.

use std::time::{SystemTime, UNIX_EPOCH};

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
