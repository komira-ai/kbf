//! The evaluator's own earlier scrapes of the latency histogram, and the p99 baseline taken from
//! them.

use kbf_bake::ledger::Ledger;
use kbf_bake::table::{BASELINE_SECS, BASELINE_SLACK_SECS};

const AT: u64 = 1_800_000_000;
const H: u64 = 3600;

/// Buckets 0.5, 2, 8, +Inf with `a`, `b`, `c`, `d` cumulative counts.
fn b(a: f64, b: f64, c: f64, d: f64) -> Vec<(f64, f64)> {
    vec![(0.5, a), (2.0, b), (8.0, c), (f64::INFINITY, d)]
}

/// Readings each hour across the window, `per_hour[i]` observations an hour in the i-th bucket
/// (not cumulative); the file holds them cumulative, as the farm does.
fn week(per_hour: [f64; 4]) -> Ledger {
    let mut l = Ledger::default();
    let mut c = [0.0; 4];
    let mut t = AT - BASELINE_SECS;
    while t <= AT {
        // Cumulative histogram: each bucket holds every observation at or below its bound.
        let cum: Vec<f64> = (0..4).map(|i| c[..=i].iter().sum::<f64>()).collect();
        l.push(t, &b(cum[0], cum[1], cum[2], cum[3])).unwrap();
        for (x, p) in c.iter_mut().zip(per_hour) {
            *x += p;
        }
        t += H;
    }
    l
}

/// Catches a p99 taken over the wrong thing: the p99 is the bound of the bucket that holds 99% of
/// the week's increase (99.5% in the first bucket is 0.5; 90% puts it in the second).
#[test]
fn the_p99_is_the_bucket_holding_ninety_nine_percent_of_the_weeks_increase() {
    assert_eq!(week([99.5, 0.5, 0.0, 0.0]).queue_p99(AT).unwrap(), 0.5);
    assert_eq!(week([90.0, 10.0, 0.0, 0.0]).queue_p99(AT).unwrap(), 2.0);
    assert_eq!(week([90.0, 5.0, 5.0, 0.0]).queue_p99(AT).unwrap(), 8.0);
}

/// Catches a short history standing in for a week: a ledger that starts later than the slack
/// allows after the window's start is an error naming how far it falls short; one that starts
/// exactly at the slack covers it.
#[test]
fn a_ledger_that_does_not_reach_back_seven_days_is_no_baseline() {
    let from = AT - BASELINE_SECS;
    let reading = |start: u64| {
        let mut l = Ledger::default();
        l.push(start, &b(0.0, 0.0, 0.0, 0.0)).unwrap();
        l.push(AT, &b(100.0, 100.0, 100.0, 100.0)).unwrap();
        l
    };
    assert_eq!(
        reading(from + BASELINE_SLACK_SECS).queue_p99(AT).unwrap(),
        0.5
    );
    let why = reading(from + BASELINE_SLACK_SECS + 1)
        .queue_p99(AT)
        .unwrap_err();
    assert!(why.contains("does not cover seven days"), "{why}");
    let why = Ledger::default().queue_p99(AT).unwrap_err();
    assert!(why.contains("no reading"), "{why}");
}

/// Catches a stale ledger: a sampler that stopped days ago leaves a week that ends before the
/// deploy. The newest reading may be the slack old, not one second more.
#[test]
fn a_ledger_that_stopped_sampling_is_no_baseline() {
    let from = AT - BASELINE_SECS;
    let ending = |last: u64| {
        let mut l = Ledger::default();
        l.push(from, &b(0.0, 0.0, 0.0, 0.0)).unwrap();
        l.push(last, &b(100.0, 100.0, 100.0, 100.0)).unwrap();
        l
    };
    assert_eq!(ending(AT - BASELINE_SLACK_SECS).queue_p99(AT).unwrap(), 0.5);
    let why = ending(AT - BASELINE_SLACK_SECS - 1)
        .queue_p99(AT)
        .unwrap_err();
    assert!(why.contains("newest reading"), "{why}");
}

/// Catches a restart of the farm (counters falling to zero) read as negative traffic: the fall is
/// a reset and adds the new value, as for any counter.
#[test]
fn a_counter_reset_in_the_week_is_a_restart_not_a_drop() {
    let from = AT - BASELINE_SECS;
    let mut l = Ledger::default();
    l.push(from, &b(1000.0, 1000.0, 1000.0, 1000.0)).unwrap();
    l.push(from + 3 * H, &b(1100.0, 1100.0, 1100.0, 1100.0))
        .unwrap();
    // The farm restarted: 90 fast and 10 slower observations since.
    l.push(AT, &b(90.0, 100.0, 100.0, 100.0)).unwrap();
    // The increase is 190 in the first bucket and 200 in the rest: the p99 is 2.
    assert_eq!(l.queue_p99(AT).unwrap(), 2.0);
}

/// Catches a window of nothing passing as a quiet baseline, and a p99 beyond the last bucket
/// passing as a number.
#[test]
fn nothing_observed_and_an_unbounded_p99_are_no_baseline() {
    let flat = week([0.0, 0.0, 0.0, 0.0]);
    assert!(
        flat.queue_p99(AT)
            .unwrap_err()
            .contains("nothing was observed")
    );
    let slow = week([0.0, 0.0, 0.0, 100.0]);
    assert!(slow.queue_p99(AT).unwrap_err().contains("largest bucket"));
}

/// Catches readings outside the week counting: a burst of slow calls before the window is not in the
/// baseline, and one after the deploy is not either.
#[test]
fn readings_outside_the_window_do_not_count() {
    let from = AT - BASELINE_SECS;
    let mut l = Ledger::default();
    l.push(from - 10 * H, &b(0.0, 0.0, 0.0, 0.0)).unwrap();
    l.push(from - 5 * H, &b(0.0, 0.0, 10_000.0, 10_000.0))
        .unwrap();
    l.push(from, &b(0.0, 0.0, 10_000.0, 10_000.0)).unwrap();
    l.push(AT, &b(100.0, 100.0, 10_100.0, 10_100.0)).unwrap();
    l.push(AT + 5 * H, &b(100.0, 100.0, 90_100.0, 90_100.0))
        .unwrap();
    assert_eq!(l.queue_p99(AT).unwrap(), 0.5);
}

/// Catches a clock that went backwards being appended, and an unbounded file: readings older than
/// twice the window are dropped on a push.
#[test]
fn pushes_stay_in_order_and_old_readings_are_dropped() {
    let mut l = Ledger::default();
    l.push(100, &b(0.0, 0.0, 0.0, 0.0)).unwrap();
    assert!(
        l.push(100, &b(0.0, 0.0, 0.0, 0.0))
            .unwrap_err()
            .contains("not after")
    );
    assert!(l.push(99, &b(0.0, 0.0, 0.0, 0.0)).is_err());
    l.push(100 + 2 * BASELINE_SECS, &b(0.0, 0.0, 0.0, 0.0))
        .unwrap();
    assert_eq!(l.snapshots.len(), 2, "exactly twice the window is kept");
    l.push(101 + 2 * BASELINE_SECS, &b(0.0, 0.0, 0.0, 0.0))
        .unwrap();
    assert_eq!(
        l.snapshots.len(),
        2,
        "the reading at 100 is now older than twice the window"
    );
}

/// A file round-trips, a missing file is an empty ledger, and a file that is not a ledger is an
/// error (a history is never silently started over).
#[test]
fn the_ledger_file_round_trips_and_is_never_silently_replaced() {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("ledger-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("ledger.json");
    assert_eq!(Ledger::load(&path).unwrap(), Ledger::default());
    let l = week([99.0, 1.0, 0.0, 0.0]);
    l.save(&path).unwrap();
    assert_eq!(Ledger::load(&path).unwrap(), l);
    std::fs::write(&path, "{\"schema\":2,\"snapshots\":[]}").unwrap();
    assert!(Ledger::load(&path).unwrap_err().contains("schema 2"));
    std::fs::write(&path, "garbage").unwrap();
    assert!(Ledger::load(&path).unwrap_err().contains("not a ledger"));
    // A path that cannot be read for another reason than being absent is an error, not an empty ledger.
    assert!(Ledger::load(&dir).unwrap_err().starts_with("read "));
    // A directory that is not there cannot take the file.
    let why = l
        .save(&dir.join("no-such-dir").join("l.json"))
        .unwrap_err()
        .to_string();
    assert!(why.starts_with("write "), "{why}");
}

use std::path::Path;
