//! The cold store guard: a run stops when the cold store itself is in trouble.
//!
//! The load gate ([`crate::gate`]) sees this machine only. The guard sees the store: it keeps the
//! harness's own requests to the cold role over a sliding window (60 s unless set) and trips when
//!
//! - the p99 latency of the window is over `--cold-abort-p99-ms` (2,000 ms), or
//! - the share of the window's requests that failed or timed out is over
//!   `--cold-abort-error-share` (0.05).
//!
//! A trip ends the pass and the run, the way a trip of the load gate does (the passes so far are
//! written first), and is recorded in the result ([`crate::result::PassResult::cold_guard`]).
//!
//! Rules:
//! - Equal to a limit is within it, as in the load gate.
//! - A window with fewer than [`MIN_REQUESTS`] requests trips nothing: one failure in two
//!   requests is not a share, and the p99 of a handful is the slowest of them.
//! - A request that failed counts in the share and, with the time it took to fail, in the p99: a
//!   store that hangs until the timeout is slow as well as failing.
//! - The window counts one entry per request: the step of the whole request. The step that
//!   carries a second quantity of a request (the `.ttfb` of a streamed read, which only a request
//!   that succeeded has) is not an entry, or a read that succeeded would count twice and one that
//!   failed once.
//! - Only requests the harness sent to the cold role count; the arms that put the cold store in a
//!   fault on purpose (through the fault proxy) are not shown to the guard.
//! - The guard is pure: it is given the instant of every request and of every check, so a test
//!   moves time by hand.

use std::collections::VecDeque;
use std::time::Duration;

use tokio::time::Instant;

use crate::cells::stats;
use crate::result::ColdTrip;

/// Fewer requests than this in the window and the guard does not judge.
pub const MIN_REQUESTS: usize = 20;

/// The limits of the guard.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ColdGuardLimits {
    /// The length of the sliding window.
    pub window: Duration,
    /// Highest p99 latency of the window, milliseconds.
    pub max_p99_ms: u64,
    /// Highest share of failed requests in the window, 0 to 1.
    pub max_error_share: f64,
}

impl Default for ColdGuardLimits {
    fn default() -> Self {
        Self {
            window: Duration::from_secs(60),
            max_p99_ms: 2_000,
            max_error_share: 0.05,
        }
    }
}

/// One request of the harness to the cold role.
#[derive(Clone, Copy)]
struct Seen {
    at: Instant,
    latency_us: u64,
    failed: bool,
}

/// The window of cold requests.
pub struct ColdGuard {
    limits: ColdGuardLimits,
    window: VecDeque<Seen>,
}

impl ColdGuard {
    /// A guard with an empty window.
    #[must_use]
    pub fn new(limits: ColdGuardLimits) -> Self {
        Self {
            limits,
            window: VecDeque::new(),
        }
    }

    /// Notes a request that finished at `at`.
    pub fn record(&mut self, at: Instant, latency_us: u64, failed: bool) {
        self.window.push_back(Seen {
            at,
            latency_us,
            failed,
        });
    }

    /// Judges the window as it stands at `now`: the requests older than the window are dropped,
    /// then the error share and the p99 of what is left are held to the limits (the share is
    /// looked at first).
    ///
    /// # Errors
    /// [`ColdTrip`] naming the measure that went over.
    pub fn check(&mut self, now: Instant) -> Result<(), ColdTrip> {
        let window = self.limits.window;
        while self
            .window
            .front()
            .is_some_and(|seen| now.saturating_duration_since(seen.at) > window)
        {
            self.window.pop_front();
        }
        let requests = self.window.len();
        if requests < MIN_REQUESTS {
            return Ok(());
        }
        let trip = |metric: &str, value: f64, limit: f64| ColdTrip {
            metric: metric.to_owned(),
            value,
            limit,
            requests: requests as u64,
            window_secs: window.as_secs(),
        };
        let failed = self.window.iter().filter(|seen| seen.failed).count();
        let share = failed as f64 / requests as f64;
        if share > self.limits.max_error_share {
            return Err(trip("error_share", share, self.limits.max_error_share));
        }
        let mut latencies: Vec<u64> = self.window.iter().map(|seen| seen.latency_us).collect();
        let p99_us = stats::summarise(&mut latencies).map_or(0, |s| s.p99);
        let limit_us = self.limits.max_p99_ms.saturating_mul(1_000);
        if p99_us > limit_us {
            return Err(trip(
                "p99_ms",
                p99_us as f64 / 1_000.0,
                self.limits.max_p99_ms as f64,
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIMITS: ColdGuardLimits = ColdGuardLimits {
        window: Duration::from_secs(60),
        max_p99_ms: 2_000,
        max_error_share: 0.05,
    };

    /// The clock of these tests, moved by hand: `at` is `s` seconds after a fixed start, `tick`
    /// is `n` tenths of a second.
    struct Clock(Instant);

    impl Clock {
        fn at(&self, s: u64) -> Instant {
            self.0 + Duration::from_secs(s)
        }

        fn tick(&self, n: u64) -> Instant {
            self.0 + Duration::from_millis(100 * n)
        }
    }

    fn clock() -> Clock {
        Clock(Instant::now())
    }

    /// `n` healthy requests, one every tenth of a second from tick `from`.
    fn healthy(guard: &mut ColdGuard, clock: &Clock, from: u64, n: u64) {
        for i in 0..n {
            guard.record(clock.tick(from + i), 50_000, false);
        }
    }

    /// Catches a guard that trips on a store that is fine, or on one exactly at a limit.
    #[test]
    fn a_healthy_window_and_a_window_at_the_limits_pass() {
        let clock = clock();
        let mut guard = ColdGuard::new(LIMITS);
        healthy(&mut guard, &clock, 0, 98);
        // Nearest rank: the p99 of 100 is the 99th smallest, here exactly the limit.
        guard.record(clock.tick(98), 2_000_000, false);
        guard.record(clock.tick(99), 2_000_000, false);
        assert_eq!(guard.check(clock.tick(100)), Ok(()));
        // One microsecond more is over it.
        let mut over = ColdGuard::new(LIMITS);
        healthy(&mut over, &clock, 0, 98);
        over.record(clock.tick(98), 2_000_001, false);
        over.record(clock.tick(99), 2_000_001, false);
        assert_eq!(over.check(clock.tick(100)).unwrap_err().metric, "p99_ms");
        // 5 of 100 failed is the limit itself.
        let mut at_share = ColdGuard::new(LIMITS);
        healthy(&mut at_share, &clock, 0, 95);
        for i in 0..5 {
            at_share.record(clock.tick(95 + i), 1_000, true);
        }
        assert_eq!(at_share.check(clock.tick(100)), Ok(()));
    }

    /// Catches an error share that is not looked at, or looked at with the wrong sign: one more
    /// failure than the limit allows trips it, by name, with the numbers.
    #[test]
    fn an_error_share_over_the_limit_trips_by_name() {
        let clock = clock();
        let mut guard = ColdGuard::new(LIMITS);
        healthy(&mut guard, &clock, 0, 94);
        for i in 0..6 {
            guard.record(clock.tick(94 + i), 1_000, true);
        }
        let trip = guard.check(clock.tick(100)).unwrap_err();
        assert_eq!(trip.metric, "error_share");
        assert!((trip.value - 0.06).abs() < 1e-9, "{trip}");
        assert!((trip.limit - 0.05).abs() < 1e-9);
        assert_eq!((trip.requests, trip.window_secs), (100, 60));
        assert!(trip.to_string().contains("error_share"));
    }

    /// Catches a p99 that is not looked at, one taken over milliseconds as microseconds, and a
    /// guard that ignores a store that answers every request, only slowly.
    #[test]
    fn a_p99_over_the_limit_trips_by_name_even_when_every_request_succeeded() {
        let clock = clock();
        let mut guard = ColdGuard::new(LIMITS);
        healthy(&mut guard, &clock, 0, 98);
        guard.record(clock.tick(98), 2_500_000, false);
        guard.record(clock.tick(99), 2_500_000, false);
        let trip = guard.check(clock.tick(100)).unwrap_err();
        assert_eq!(trip.metric, "p99_ms");
        assert!((trip.value - 2_500.0).abs() < 1e-9, "{trip}");
        assert!((trip.limit - 2_000.0).abs() < 1e-9);
    }

    /// Catches a window that never slides: slow requests older than the window are forgotten, and
    /// the store that recovered is not held to its past.
    #[test]
    fn requests_older_than_the_window_are_forgotten() {
        let clock = clock();
        let mut guard = ColdGuard::new(LIMITS);
        for i in 0..30 {
            guard.record(clock.at(i), 9_000_000, true);
        }
        assert!(guard.check(clock.at(30)).is_err());
        // 61 s after the last of them, all are out of the window.
        for i in 0..25 {
            guard.record(clock.at(91 + i), 50_000, false);
        }
        assert_eq!(guard.check(clock.at(116)), Ok(()));
        assert_eq!(guard.window.len(), 25);
        // The edge: a request exactly one window old is still in it.
        let mut edge = ColdGuard::new(LIMITS);
        for _ in 0..MIN_REQUESTS {
            edge.record(clock.at(0), 1_000, true);
        }
        assert!(edge.check(clock.at(60)).is_err());
        assert_eq!(edge.check(clock.at(61)), Ok(()));
    }

    /// Catches a guard that trips on a handful of requests: one failure of one is a share of 1.
    #[test]
    fn too_few_requests_are_not_judged() {
        let clock = clock();
        let mut guard = ColdGuard::new(LIMITS);
        for i in 0..(MIN_REQUESTS as u64 - 1) {
            guard.record(clock.at(i), 60_000_000, true);
        }
        assert_eq!(guard.check(clock.at(30)), Ok(()));
        guard.record(clock.at(30), 60_000_000, true);
        assert!(guard.check(clock.at(31)).is_err());
    }

    /// Catches limits other than the stated defaults: a window of 60 s, a p99 of 2,000 ms and an
    /// error share of 0.05.
    #[test]
    fn the_defaults_are_sixty_seconds_two_seconds_and_five_percent() {
        assert_eq!(ColdGuardLimits::default(), LIMITS);
    }
}
