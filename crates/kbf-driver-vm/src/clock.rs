//! [`Clock`]: the time the driver's own waits read and let pass, so that a test of a
//! deadline moves time itself instead of waiting for it.
//!
//! Two of the driver's waits are the driver's own: the poll for a booting guest's
//! `Ready` (a deadline read with [`Clock::now`], a pause with [`Clock::pause`] between
//! looks) and the watchdog on the guest's report ([`Clock::sleep`]). [`Wall`] is the
//! monotonic clock, a thread sleep and a timer of the runtime; the tests' clock moves on
//! when the test says so. The guest's own timeout is the guest's, and a read bound on a
//! stream is the stream's ([`kbf_guest::server::Stream::set_read_timeout`]).

use std::future::Future;
use std::pin::Pin;
use std::time::{Duration, Instant};

/// A wait that ends when its time has passed on the clock it was asked of.
pub type Sleep = Pin<Box<dyn Future<Output = ()> + Send>>;

/// A clock and a way to let time pass on it.
pub trait Clock: std::fmt::Debug + Send + Sync + 'static {
    /// Time since an origin fixed when the clock was made; never goes back.
    fn now(&self) -> Duration;
    /// Blocks the calling thread while `d` passes.
    fn pause(&self, d: Duration);
    /// A wait that ends once `d` has passed from now.
    fn sleep(&self, d: Duration) -> Sleep;
}

/// The monotonic clock, a thread sleep and the runtime's timer.
#[derive(Debug)]
pub struct Wall(Instant);

impl Wall {
    /// A clock whose origin is now.
    #[must_use]
    pub fn new() -> Self {
        Self(Instant::now())
    }
}

impl Default for Wall {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for Wall {
    fn now(&self) -> Duration {
        self.0.elapsed()
    }

    fn pause(&self, d: Duration) {
        std::thread::sleep(d);
    }

    fn sleep(&self, d: Duration) -> Sleep {
        Box::pin(tokio::time::sleep(d))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Catches a wall clock that does not wait (a pause or a sleep that returns at
    /// once would make every poll a busy loop and the watchdog fire early) or does not
    /// move: both only ever lengthen what has passed, so the bound is a floor.
    #[tokio::test]
    async fn the_wall_clock_moves_on_when_it_pauses_and_sleeps() {
        let wall = Wall::default();
        let before = wall.now();

        wall.pause(Duration::from_millis(2));
        let paused = wall.now();
        wall.sleep(Duration::from_millis(2)).await;

        assert!(paused >= before + Duration::from_millis(2));
        assert!(wall.now() >= paused + Duration::from_millis(2));
    }
}
