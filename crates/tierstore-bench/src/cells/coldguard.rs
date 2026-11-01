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
use crate::coldstate::ColdReading;
use crate::result::{ColdTrip, ColdWindow};

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
    /// Most requests from elsewhere that a pass may have seen at the cold store.
    pub max_foreign_requests: u64,
}

impl Default for ColdGuardLimits {
    fn default() -> Self {
        Self {
            window: Duration::from_secs(60),
            max_p99_ms: 2_000,
            max_error_share: 0.05,
            max_foreign_requests: 10,
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

    /// Whether a first reading has been taken: [`ColdGuard::open`] has been called, or
    /// [`ColdGuard::close`] took its place.
    #[must_use]
    pub fn has_baseline(&self) -> bool {
        todo!("whether a first reading is held")
    }

    /// Takes the first reading of the chain of windows (the counters at its start), and refuses a
    /// store that is not healthy by it.
    ///
    /// # Errors
    /// [`ColdTrip`] if the reading failed or the store's drives are not all `ok`, or it is healing.
    pub fn open(
        &mut self,
        _reading: Result<ColdReading, String>,
        _own_requests: u64,
    ) -> Result<(), ColdTrip> {
        todo!("the first reading")
    }

    /// Ends the window that began at the last reading and begins the next at this one.
    /// `own_requests` is the harness's count of requests to the cold role at the reading;
    /// `judgeable` is whether the traffic of the window was the harness's alone.
    ///
    /// Gives what the window read (none if it could not be worked out) and the trip, if the store
    /// is not healthy or the requests from elsewhere are over the limit.
    pub fn close(
        &mut self,
        _reading: Result<ColdReading, String>,
        _own_requests: u64,
        _judgeable: bool,
    ) -> (Option<ColdWindow>, Option<ColdTrip>) {
        todo!("the end of a window")
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
            detail: None,
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
    use crate::result::{ColdSource, ColdState, DriveState};

    /// The limits of the p99 and error share tests: a 2 s p99 keeps their numbers small. The
    /// real default is the 30 s backstop, pinned by its own test.
    const LIMITS: ColdGuardLimits = ColdGuardLimits {
        window: Duration::from_secs(60),
        max_p99_ms: 2_000,
        max_error_share: 0.05,
        max_foreign_requests: 10,
    };

    /// A store whose drives are in the states `drives`, healing or not.
    fn state(drives: &[&str], healing: bool) -> ColdState {
        ColdState {
            healing,
            percent_done: None,
            drives: drives
                .iter()
                .enumerate()
                .map(|(i, state)| DriveState {
                    id: format!("d-{i:08}"),
                    state: (*state).to_owned(),
                    healing: false,
                })
                .collect(),
            read_unix_ms: 1,
            source: Some(ColdSource::Mc),
        }
    }

    /// A reading of a healthy store of 12 drives whose counter says `s3_requests`.
    fn read(s3_requests: u64) -> Result<ColdReading, String> {
        Ok(ColdReading {
            s3_requests,
            state: state(&["ok"; 12], false),
        })
    }

    /// A reading of a store in `state`.
    fn read_in(s3_requests: u64, state: ColdState) -> Result<ColdReading, String> {
        Ok(ColdReading { s3_requests, state })
    }

    /// A guard whose chain of windows began with the counter at 1,000 and the harness's own count
    /// at 500.
    fn opened(limits: ColdGuardLimits) -> ColdGuard {
        let mut guard = ColdGuard::new(limits);
        guard.open(read(1_000), 500).unwrap();
        guard
    }

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

    /// Catches limits other than the stated defaults: a window of 60 s, a hung-store backstop of
    /// 30,000 ms, an error share of 0.05 and 10 requests from elsewhere.
    #[test]
    fn the_defaults_are_a_minute_a_thirty_second_backstop_five_percent_and_ten_foreign() {
        assert_eq!(
            ColdGuardLimits::default(),
            ColdGuardLimits {
                max_p99_ms: 30_000,
                ..LIMITS
            }
        );
    }

    /// Catches a guard that trips on the bench's own latency: the circular case. A store whose
    /// small writes take 2.5 s at the p99 (an erasure-coded set of hard disks does) is not in
    /// trouble when nothing but the bench is sending: the p99 is under the backstop, and the
    /// window has no request from elsewhere.
    #[test]
    fn a_quiet_window_with_a_high_p99_of_its_own_does_not_trip() {
        let clock = clock();
        let mut guard = opened(ColdGuardLimits::default());
        healthy(&mut guard, &clock, 0, 98);
        guard.record(clock.tick(98), 2_500_000, false);
        guard.record(clock.tick(99), 2_500_000, false);
        assert_eq!(guard.check(clock.tick(100)), Ok(()));
        let (window, trip) = guard.close(read(1_100), 600, true);
        assert_eq!(trip, None);
        let window = window.unwrap();
        assert_eq!((window.minio_requests, window.own_requests), (100, 100));
        assert_eq!((window.foreign_requests, window.judged), (0, true));
    }

    /// Catches a backstop that is not looked at, or one that fires early: a p99 of 31 s is a hung
    /// store, and exactly 30 s is within the limit.
    #[test]
    fn a_hung_store_trips_the_backstop_and_the_message_says_so() {
        let clock = clock();
        let mut guard = ColdGuard::new(ColdGuardLimits::default());
        healthy(&mut guard, &clock, 0, 98);
        guard.record(clock.tick(98), 30_000_000, false);
        guard.record(clock.tick(99), 30_000_000, false);
        assert_eq!(guard.check(clock.tick(100)), Ok(()));
        guard.record(clock.tick(100), 31_000_000, false);
        guard.record(clock.tick(101), 31_000_000, false);
        let trip = guard.check(clock.tick(102)).unwrap_err();
        assert_eq!(trip.metric, "p99_ms");
        assert!((trip.value - 31_000.0).abs() < 1e-9, "{trip}");
        assert!((trip.limit - 30_000.0).abs() < 1e-9);
        let text = trip.to_string();
        assert!(text.contains("hung-store backstop"), "{text}");
        assert!(text.contains("31000"), "{text}");
    }

    /// Catches foreign requests that are not held to the limit, and a message that does not name
    /// them: 71 requests at the store, 60 of them the bench's, 11 are over the 10 allowed.
    #[test]
    fn foreign_requests_over_the_limit_trip_and_the_message_names_them() {
        let mut guard = opened(ColdGuardLimits::default());
        let (window, trip) = guard.close(read(1_071), 560, true);
        let trip = trip.unwrap();
        assert_eq!(trip.metric, "foreign_requests");
        assert!((trip.value - 11.0).abs() < 1e-9, "{trip}");
        assert!((trip.limit - 10.0).abs() < 1e-9);
        let text = trip.to_string();
        assert!(text.contains("foreign_requests is 11"), "{text}");
        assert!(text.contains("limit 10"), "{text}");
        assert!(text.contains("71") && text.contains("60"), "{text}");
        // The window it read is given with the trip, for the result.
        let window = window.unwrap();
        assert_eq!(
            (
                window.minio_requests,
                window.own_requests,
                window.foreign_requests
            ),
            (71, 60, 11)
        );
    }

    /// Pins the boundary: exactly the limit is within it, as everywhere else in the guard.
    #[test]
    fn foreign_requests_at_the_limit_do_not_trip() {
        let mut guard = opened(ColdGuardLimits::default());
        let (window, trip) = guard.close(read(1_070), 560, true);
        assert_eq!(trip, None);
        assert_eq!(window.unwrap().foreign_requests, 10);
        // A limit of 0 is "no request from elsewhere at all".
        let strict = ColdGuardLimits {
            max_foreign_requests: 0,
            ..ColdGuardLimits::default()
        };
        let mut guard = opened(strict);
        assert_eq!(guard.close(read(1_060), 560, true).1, None);
        assert_eq!(
            guard.close(read(1_061 + 60), 620, true).1.unwrap().metric,
            "foreign_requests"
        );
    }

    /// Catches a store that failed the bench's requests before they were counted (a refused
    /// connection is in the bench's count and not the store's) being read as negative foreign
    /// load: it is none, and the numbers are in the window.
    #[test]
    fn fewer_requests_at_the_store_than_sent_is_no_foreign_load() {
        let mut guard = opened(ColdGuardLimits::default());
        let (window, trip) = guard.close(read(1_005), 509, true);
        assert_eq!(trip, None);
        let window = window.unwrap();
        assert_eq!(
            (
                window.minio_requests,
                window.own_requests,
                window.foreign_requests
            ),
            (5, 9, 0)
        );
    }

    /// Catches windows that are not chained: the second window starts where the first ended, so
    /// the requests of the first are not counted again.
    #[test]
    fn each_window_begins_where_the_last_ended() {
        let mut guard = opened(ColdGuardLimits::default());
        assert!(guard.has_baseline());
        let (first, _) = guard.close(read(1_050), 550, true);
        assert_eq!(first.unwrap().minio_requests, 50);
        let (second, trip) = guard.close(read(1_080), 580, true);
        assert_eq!(trip, None);
        assert_eq!(second.unwrap().minio_requests, 30);
    }

    /// Catches a guard that says it has a reading before it has: only a first reading makes the
    /// baseline; a window closed with none begins the chain and judges the store's health, not
    /// the requests (there is nothing to take them from).
    #[test]
    fn a_window_closed_before_any_reading_begins_the_chain() {
        let mut guard = ColdGuard::new(ColdGuardLimits::default());
        assert!(!guard.has_baseline());
        assert_eq!(guard.close(read(1_000), 500, true), (None, None));
        assert!(guard.has_baseline());
        let (window, _) = guard.close(read(1_020), 520, true);
        assert_eq!(window.unwrap().minio_requests, 20);
        let mut sick = ColdGuard::new(ColdGuardLimits::default());
        let offline = state(&["ok", "offline"], false);
        let (window, trip) = sick.close(read_in(1_000, offline), 500, true);
        assert_eq!((window, trip.unwrap().metric.as_str()), (None, "drives"));
    }

    /// Catches a drive that is not `ok` being let by, in the window and at the start of the
    /// chain: the numbers are the drives not ok and the drives listed.
    #[test]
    fn a_drive_that_is_not_ok_trips() {
        let mut offline = vec!["ok"; 12];
        offline[3] = "offline";
        let mut guard = opened(ColdGuardLimits::default());
        let (window, trip) = guard.close(read_in(1_000, state(&offline, false)), 500, true);
        let trip = trip.unwrap();
        assert_eq!(trip.metric, "drives");
        assert!((trip.value - 1.0).abs() < 1e-9, "{trip}");
        assert!(trip.limit.abs() < 1e-9);
        let text = trip.to_string();
        assert!(text.contains("1 of 12"), "{text}");
        assert!(text.contains("offline"), "{text}");
        let window = window.unwrap();
        assert_eq!((window.drives, window.drives_not_ok), (12, 1));
        // Starting on such a store is refused as well.
        let mut fresh = ColdGuard::new(ColdGuardLimits::default());
        let refused = fresh.open(read_in(1_000, state(&offline, false)), 500);
        assert_eq!(refused.unwrap_err().metric, "drives");
        // And a store that lists no drive at all cannot be shown to have them all ok.
        let mut none = ColdGuard::new(ColdGuardLimits::default());
        let trip = none.open(read_in(1_000, state(&[], false)), 500);
        assert!(trip.unwrap_err().to_string().contains("no drive"));
    }

    /// Catches a store that is healing being let by.
    #[test]
    fn a_store_that_is_healing_trips() {
        let mut guard = opened(ColdGuardLimits::default());
        let healing = state(&["ok"; 12], true);
        let (window, trip) = guard.close(read_in(1_000, healing.clone()), 500, true);
        let trip = trip.unwrap();
        assert_eq!(trip.metric, "healing");
        assert!((trip.value - 1.0).abs() < 1e-9, "{trip}");
        assert!(trip.to_string().contains("healing"));
        assert!(window.unwrap().healing);
        let mut fresh = ColdGuard::new(ColdGuardLimits::default());
        assert_eq!(
            fresh.open(read_in(1_000, healing), 500).unwrap_err().metric,
            "healing"
        );
    }

    /// Catches an order that hides the cause: a store with a drive offline AND foreign load
    /// names the drive (the more specific fault, which also explains the load's effects).
    #[test]
    fn a_drive_fault_is_named_before_foreign_load() {
        let mut guard = opened(ColdGuardLimits::default());
        let offline = state(&["ok", "offline"], false);
        let (_, trip) = guard.close(read_in(1_500, offline), 500, true);
        assert_eq!(trip.unwrap().metric, "drives");
    }

    /// Decision, pinned: a counter that cannot be read is a trip, not zero foreign load. A window
    /// that cannot be shown to be quiet is not quiet; and a trip, unlike an error, still writes
    /// the results of the passes so far.
    #[test]
    fn a_counter_that_cannot_be_read_trips_and_is_not_zero_load() {
        let mut guard = opened(ColdGuardLimits::default());
        let (window, trip) = guard.close(Err("no series".to_owned()), 600, true);
        assert_eq!(window, None);
        let trip = trip.unwrap();
        assert_eq!(trip.metric, "unreadable");
        let text = trip.to_string();
        assert!(
            text.contains("unreadable") && text.contains("no series"),
            "{text}"
        );
        let mut fresh = ColdGuard::new(ColdGuardLimits::default());
        let refused = fresh.open(Err("timed out".to_owned()), 500).unwrap_err();
        assert_eq!(refused.metric, "unreadable");
        assert!(refused.to_string().contains("timed out"));
        assert!(!fresh.has_baseline());
    }

    /// Decision, pinned: a counter that went backwards (the store restarted, its counter reset) is
    /// a trip naming both readings; the same value is a window with no request.
    #[test]
    fn a_counter_that_goes_backwards_trips_and_the_same_value_is_a_quiet_window() {
        let mut guard = opened(ColdGuardLimits::default());
        let (window, trip) = guard.close(read(3), 520, true);
        assert_eq!(window, None);
        let trip = trip.unwrap();
        assert_eq!(trip.metric, "counter");
        let text = trip.to_string();
        assert!(text.contains("1000") && text.contains('3'), "{text}");
        let mut same = opened(ColdGuardLimits::default());
        let (window, trip) = same.close(read(1_000), 500, true);
        assert_eq!(trip, None);
        let window = window.unwrap();
        assert_eq!((window.minio_requests, window.foreign_requests), (0, 0));
    }

    /// Catches foreign load judged where the harness cannot tell it from the instance's own
    /// copy traffic: the window is recorded with `judged` false and trips nothing, and so is the
    /// window after it (the copy goes on); the next one is judged again.
    #[test]
    fn a_window_that_is_not_judgeable_is_recorded_and_not_judged() {
        let mut guard = opened(ColdGuardLimits::default());
        let (first, trip) = guard.close(read(1_500), 500, false);
        assert_eq!(trip, None);
        let first = first.unwrap();
        assert_eq!((first.foreign_requests, first.judged), (500, false));
        let (second, trip) = guard.close(read(2_000), 500, true);
        assert_eq!(trip, None);
        assert!(!second.unwrap().judged);
        let (third, trip) = guard.close(read(2_500), 500, true);
        assert!(third.unwrap().judged);
        assert_eq!(trip.unwrap().metric, "foreign_requests");
    }
}
