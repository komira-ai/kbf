//! The rate limit: the clock and the sleeper a run is given, and the spacing of its trees.

use std::num::NonZeroU32;
use std::time::{Duration, Instant};

use futures::future::BoxFuture;
use kbf_front::action_tree::Sleeper;

/// A monotonic clock: the time since some fixed moment.
pub trait Clock: Send + Sync {
    /// The time since the clock's start; never less than an earlier answer.
    fn now(&self) -> Duration;
}

/// The [`Clock`] of a run: the system's monotonic clock, from the moment it was made.
pub struct SystemClock {
    start: Instant,
}

impl SystemClock {
    /// A clock that starts now.
    #[must_use]
    pub fn new() -> Self {
        Self {
            start: Instant::now(),
        }
    }
}

impl Default for SystemClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for SystemClock {
    fn now(&self) -> Duration {
        self.start.elapsed()
    }
}

/// The [`Sleeper`] of a run: the tokio timer.
pub struct TokioSleeper;

impl Sleeper for TokioSleeper {
    fn sleep(&self, wait: Duration) -> BoxFuture<'_, ()> {
        Box::pin(tokio::time::sleep(wait))
    }
}

/// Spaces the trees of a run: each starts at least `every` after the one before.
pub struct Pace {
    every: Option<Duration>,
    next: Option<Duration>,
}

impl Pace {
    /// A pace of `rate` trees a second, or none.
    #[must_use]
    pub fn new(rate: Option<NonZeroU32>) -> Self {
        Self {
            every: rate.map(|rate| Duration::from_secs(1) / rate.get()),
            next: None,
        }
    }

    /// Returns when the next tree may start: at once for the first, and for every tree when
    /// there is no rate; otherwise after sleeping what is left of the spacing.
    pub async fn wait(&mut self, clock: &dyn Clock, sleeper: &dyn Sleeper) {
        let Some(every) = self.every else { return };
        if let Some(next) = self.next {
            let left = next.saturating_sub(clock.now());
            if !left.is_zero() {
                sleeper.sleep(left).await;
            }
        }
        self.next = Some(clock.now() + every);
    }
}
