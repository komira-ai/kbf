//! The clock the polling waits of this crate read and pause on, so that a test of a
//! deadline moves time itself instead of waiting for it.
//!
//! A wait that polls (a child's exit, a pipe's end) asks [`Pace::now`] for the time since
//! the pace was made and [`Pace::pause`]s between looks. [`Wall`] is the monotonic clock
//! and a thread sleep; the test pace ([`Manual`]) advances a counter on a pause.

use std::time::{Duration, Instant};

/// A clock and a way to let time pass on it.
pub(crate) trait Pace {
    /// Time since an origin fixed when the pace was made; never goes back.
    fn now(&self) -> Duration;
    /// Lets `d` pass.
    fn pause(&self, d: Duration);
}

/// The monotonic clock and a thread sleep.
pub(crate) struct Wall(Instant);

impl Wall {
    pub(crate) fn new() -> Self {
        Self(Instant::now())
    }
}

impl Pace for Wall {
    fn now(&self) -> Duration {
        self.0.elapsed()
    }
    fn pause(&self, d: Duration) {
        std::thread::sleep(d);
    }
}

#[cfg(all(test, target_os = "linux"))]
pub(crate) use manual::Hooked;
#[cfg(test)]
pub(crate) use manual::Manual;

#[cfg(test)]
mod manual {
    use std::cell::Cell;
    use std::time::Duration;

    use super::Pace;

    /// A pace whose time is a counter: a pause moves it on by the pause, at once. It is
    /// for waits on stand-ins the test controls; a wait on a real process or thread would
    /// run out its deadline before the process had run.
    #[derive(Default)]
    pub(crate) struct Manual {
        at: Cell<Duration>,
        pauses: Cell<u32>,
    }

    /// The monotonic clock, with `hook` told the number of each pause just before it is
    /// made: a test lets go of what a wait is waiting for at a pause it names, and the
    /// wait goes on in real time after it (a child it started needs real time to run).
    #[cfg(target_os = "linux")]
    pub(crate) struct Hooked<F: Fn(u32)> {
        wall: super::Wall,
        hook: F,
        pauses: Cell<u32>,
    }

    #[cfg(target_os = "linux")]
    impl<F: Fn(u32)> Hooked<F> {
        pub(crate) fn new(hook: F) -> Self {
            Self {
                wall: super::Wall::new(),
                hook,
                pauses: Cell::new(0),
            }
        }
    }

    #[cfg(target_os = "linux")]
    impl<F: Fn(u32)> Pace for Hooked<F> {
        fn now(&self) -> Duration {
            self.wall.now()
        }
        fn pause(&self, d: Duration) {
            self.pauses.set(self.pauses.get() + 1);
            (self.hook)(self.pauses.get());
            self.wall.pause(d);
        }
    }

    impl Manual {
        pub(crate) fn pauses(&self) -> u32 {
            self.pauses.get()
        }
    }

    impl Pace for Manual {
        fn now(&self) -> Duration {
            self.at.get()
        }
        fn pause(&self, d: Duration) {
            self.at.set(self.at.get() + d);
            self.pauses.set(self.pauses.get() + 1);
        }
    }
}
