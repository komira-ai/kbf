//! Per-build numbers from fixed rows with explicit timestamps (seconds after
//! 2026-10-01), and the per-run JSON shape. No clock is read.

mod common;

use common::*;
use kbf_estimator_replay::builds::{BuildRun, Spread, analyze};
use kbf_estimator_replay::row::Row;
use serde_json::{Value, json};

const S: u64 = 1_000_000;

/// An attempt of `inv` (build `corr`, when the client sent one) queued, started and ended
/// at the given seconds.
fn row(
    id: &str,
    inv: Option<&str>,
    corr: Option<&str>,
    times: (u64, Option<u64>, Option<u64>),
) -> Row {
    Row {
        run_id: id.into(),
        queued_us: OCT_1 + times.0 * S,
        started_us: times.1.map(|s| OCT_1 + s * S),
        ended_us: times.2.map(|s| OCT_1 + s * S),
        invocation_id: inv.map(str::to_owned),
        correlated_invocations_id: corr.map(str::to_owned),
        ..attempt(id, 0, 1, "ok", None)
    }
}

/// Four builds and a row that names none:
/// - `corr-1`: three attempts over two invocations (a, b), wall 30 s, queue wait 2+4+1 s;
/// - `inv-c`: no correlated id, so the invocation is the build: wall 11 s, wait 1 s;
/// - `inv-d`: a second attempt never started: wall 20 s, wait 3 s;
/// - `inv-e`: its attempt has no end: no wall, wait 1 s.
fn fixture() -> Vec<Row> {
    vec![
        row("r1", Some("a"), Some("corr-1"), (0, Some(2), Some(12))),
        row("r2", Some("a"), Some("corr-1"), (1, Some(5), Some(20))),
        row("r3", Some("b"), Some("corr-1"), (3, Some(4), Some(30))),
        row("r4", Some("inv-c"), None, (100, Some(101), Some(111))),
        row("r5", Some("inv-d"), None, (200, Some(203), Some(220))),
        row("r6", Some("inv-d"), None, (201, None, Some(205))),
        row("r7", Some("inv-e"), None, (300, Some(301), None)),
        row("r8", None, None, (400, Some(401), Some(402))),
    ]
}

fn us(s: u64) -> Option<u64> {
    Some(s * S)
}

/// Catches: a build keyed by the invocation when a correlated id is present (corr-1 would
/// be two builds), a wall that is not first queue to last end (the sum of walls, or the
/// last attempt's own), a queue wait that counts an attempt that never started or one that
/// has no id, an unfinished build given a wall, and rows without an id put in a build.
#[test]
fn a_build_is_its_correlated_id_and_gives_wall_and_queue_wait() {
    let got = analyze(&fixture());
    let run = |build: &str, attempts, wall, wait| BuildRun {
        build: build.into(),
        attempts,
        wall_us: wall,
        queue_wait_us: wait,
    };
    assert_eq!(
        got.runs,
        [
            run("corr-1", 3, us(30), us(7)),
            run("inv-c", 1, us(11), us(1)),
            run("inv-d", 2, us(20), us(3)),
            run("inv-e", 1, None, us(1)),
        ]
    );
    assert_eq!(got.rows_without_id, 1);
    assert_eq!(got.unfinished, 1);
    assert_eq!(got.out_of_order, 0);
}

/// Catches: percentiles taken over per-attempt values, rank rounding other than nearest
/// rank (walls 11, 20, 30 give p50 20 and p90 30; waits 1, 1, 3, 7 give p50 1 and p90 7),
/// a build without a value counted as a sample, and one that gives none.
#[test]
fn per_build_percentiles_are_nearest_rank_over_builds_with_the_value() {
    let got = analyze(&fixture());
    assert_eq!(got.wall_samples, 3);
    assert_eq!(
        got.wall,
        Spread {
            p50: us(20),
            p90: us(30)
        }
    );
    assert_eq!(got.queue_wait_samples, 4);
    assert_eq!(
        got.queue_wait,
        Spread {
            p50: us(1),
            p90: us(7)
        }
    );
}

/// Catches: rows from a server that does not record the ids turned into builds (one
/// per row, or one for all), and a zero printed where nothing is recorded.
#[test]
fn rows_with_no_ids_make_no_builds_and_no_numbers() {
    let rows = [
        row("r1", None, None, (0, Some(1), Some(2))),
        row("r2", None, None, (1, Some(2), Some(3))),
    ];
    let got = analyze(&rows);
    assert!(!got.recorded());
    assert_eq!(got.rows_without_id, 2);
    assert_eq!(
        (got.wall, got.queue_wait),
        (Spread::default(), Spread::default())
    );
    assert_eq!((got.wall_samples, got.queue_wait_samples), (0, 0));
    assert!(!analyze(&fixture()).runs.is_empty());
}

/// Catches: a start before its queue time wrapping into a huge wait, an end before the
/// build's first queue wrapping into a huge wall, and either being used silently.
#[test]
fn times_out_of_order_are_counted_and_not_used() {
    let rows = [
        // Started before it was queued: no wait from this attempt.
        row("r1", Some("a"), None, (10, Some(5), Some(20))),
        // Ended before the build's earliest queue: no wall for the build.
        row("r2", Some("b"), None, (50, Some(51), Some(40))),
    ];
    let got = analyze(&rows);
    assert_eq!(got.out_of_order, 2);
    assert_eq!(
        got.runs,
        [
            BuildRun {
                build: "a".into(),
                attempts: 1,
                wall_us: us(10),
                queue_wait_us: None,
            },
            BuildRun {
                build: "b".into(),
                attempts: 1,
                wall_us: None,
                queue_wait_us: us(1),
            },
        ]
    );
}

/// Catches: a JSON run whose keys differ from `js-analysis`'s `RunRecord` (which refuses
/// an unknown field and needs `setting`, `arm`, `run_index`, `seed`, `commit`, `label` and
/// `metrics`), a metric that is zero where the build has no value, a run index that is not
/// the build's place in queue order, and an arm name dropped.
#[test]
fn per_run_json_has_the_fields_of_a_run_record() {
    let records = analyze(&fixture()).run_records("a");
    assert_eq!(records.len(), 4);
    for (i, r) in records.iter().enumerate() {
        let obj = r.as_object().unwrap();
        let mut keys: Vec<&str> = obj.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "arm",
                "commit",
                "label",
                "metrics",
                "run_index",
                "seed",
                "setting"
            ]
        );
        assert_eq!(r["arm"], "a");
        assert_eq!(r["run_index"], i);
        assert_eq!(r["seed"], 0);
        assert_eq!(r["label"], "");
        assert_eq!(r["commit"], "");
        assert!(r["setting"].is_string());
        let metrics = r["metrics"].as_object().unwrap();
        let mut names: Vec<&str> = metrics.keys().map(String::as_str).collect();
        names.sort_unstable();
        assert_eq!(
            names,
            ["build_attempts", "build_queue_wait_us", "build_wall_us"]
        );
    }
    assert_eq!(
        records[0]["metrics"],
        json!({"build_attempts": 3.0, "build_queue_wait_us": 7e6, "build_wall_us": 30e6})
    );
    assert_eq!(
        records[3]["metrics"]["build_wall_us"],
        Value::Null,
        "no wall is not zero"
    );
    assert_eq!(records[3]["metrics"]["build_queue_wait_us"], json!(1e6));
}
