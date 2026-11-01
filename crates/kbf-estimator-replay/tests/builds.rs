//! Per-build numbers from fixed rows with explicit timestamps (seconds after
//! 2026-10-01), and the per-run JSON shape. No clock is read.

mod common;

use common::*;
use kbf_estimator_replay::builds::{BuildRun, GroupBy, RunLabels, Spread, analyze};
use kbf_estimator_replay::row::Row;
use serde_json::{Value, json};

const S: u64 = 1_000_000;

/// An attempt of invocation `inv` (in CI job `corr`, when the client sent one) queued,
/// started and ended at the given seconds.
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

/// Five invocations (builds) in two views, and a row that names none:
/// - `a`: two attempts, waits 2 and 4 s, span 20 s;
/// - `b`: one attempt in the same CI job as `a`: wait 1 s, span 27 s;
/// - `c`: wait 1 s, span 11 s;
/// - `d`: a second attempt never started: wait 3 s, span 20 s;
/// - `e`: its attempt has no end: no span, wait 1 s.
///
/// `a` and `b` carry the correlated id `corr-1`; the others have none.
fn fixture() -> Vec<Row> {
    vec![
        row("r1", Some("a"), Some("corr-1"), (0, Some(2), Some(12))),
        row("r2", Some("a"), Some("corr-1"), (1, Some(5), Some(20))),
        row("r3", Some("b"), Some("corr-1"), (3, Some(4), Some(30))),
        row("r4", Some("c"), None, (100, Some(101), Some(111))),
        row("r5", Some("d"), None, (200, Some(203), Some(220))),
        row("r6", Some("d"), None, (201, None, Some(205))),
        row("r7", Some("e"), None, (300, Some(301), None)),
        row("r8", None, None, (400, Some(401), Some(402))),
    ]
}

fn us(s: u64) -> Option<u64> {
    Some(s * S)
}

fn spread(p50: u64, p90: u64) -> Spread {
    Spread {
        p50: us(p50),
        p90: us(p90),
    }
}

fn labels(arm: &str) -> RunLabels {
    RunLabels {
        setting: "k7".into(),
        arm: arm.into(),
        seed: 7,
        commit: "abc123".into(),
    }
}

/// Catches: a build keyed by the correlated id by default (`a` and `b` would be one build,
/// and buck2's Build ID would not join), a span that is not first queue to last end (the
/// sum of attempt walls, or the last attempt's own), a queue time total that counts an
/// attempt that never started, an unfinished build given a span, a within-build wait
/// percentile taken over another build's attempts, and rows without an id put in a build.
#[test]
fn a_build_is_an_invocation_with_its_span_total_and_attempt_waits() {
    let got = analyze(&fixture(), GroupBy::Invocation);
    let run = |build: &str, attempts, span, total, wait| BuildRun {
        build: build.into(),
        attempts,
        action_span_us: span,
        queue_time_total_us: total,
        attempt_queue_wait: wait,
    };
    assert_eq!(
        got.runs,
        [
            run("a", 2, us(20), us(6), spread(2, 4)),
            run("b", 1, us(27), us(1), spread(1, 1)),
            run("c", 1, us(11), us(1), spread(1, 1)),
            run("d", 2, us(20), us(3), spread(3, 3)),
            run("e", 1, None, us(1), spread(1, 1)),
        ]
    );
    assert_eq!(got.rows_without_id, 1);
    assert_eq!(got.unfinished, 1);
    assert_eq!(got.out_of_order, 0);
}

/// Catches: the CI-job view not merging the invocations of one job (`a` and `b` are one
/// build of three attempts, span 30 s from queue 0 to end 30), and rows with no correlated
/// id put in a build (they are counted as rows with no id instead).
#[test]
fn grouping_by_correlated_id_merges_the_invocations_of_a_job() {
    let got = analyze(&fixture(), GroupBy::Correlated);
    assert_eq!(
        got.runs,
        [BuildRun {
            build: "corr-1".into(),
            attempts: 3,
            action_span_us: us(30),
            queue_time_total_us: us(7),
            attempt_queue_wait: spread(2, 4),
        }]
    );
    assert_eq!(got.rows_without_id, 5);
    assert_eq!(got.unfinished, 0);
}

/// Catches: percentiles taken over per-attempt values, rank rounding other than nearest
/// rank, a build without a value counted as a sample, and the distribution of the
/// within-build waits taken as one number. Spans 11, 20, 20, 27 give p50 20 and p90 27;
/// totals 1, 1, 1, 3, 6 give p50 1 and p90 6; the builds' own p50 waits 1, 1, 1, 2, 3 give p50
/// 1 and p90 3, and their p90 waits 1, 1, 1, 3, 4 give p50 1 and p90 4.
#[test]
fn per_arm_numbers_are_nearest_rank_over_builds_with_the_value() {
    let got = analyze(&fixture(), GroupBy::Invocation);
    assert_eq!(got.span_samples, 4);
    assert_eq!(got.action_span, spread(20, 27));
    assert_eq!(got.queue_total_samples, 5);
    assert_eq!(got.queue_time_total, spread(1, 6));
    assert_eq!(got.attempt_wait_p50, spread(1, 3));
    assert_eq!(got.attempt_wait_p90, spread(1, 4));
}

/// Catches: rows from a server that does not record the ids turned into builds (one per
/// row, or one for all), and a zero printed where nothing is recorded.
#[test]
fn rows_with_no_ids_make_no_builds_and_no_numbers() {
    let rows = [
        row("r1", None, None, (0, Some(1), Some(2))),
        row("r2", None, Some("corr"), (1, Some(2), Some(3))),
    ];
    let got = analyze(&rows, GroupBy::Invocation);
    assert!(!got.recorded());
    assert_eq!(got.rows_without_id, 2);
    assert_eq!(
        (got.action_span, got.queue_time_total, got.attempt_wait_p50),
        (Spread::default(), Spread::default(), Spread::default())
    );
    assert_eq!((got.span_samples, got.queue_total_samples), (0, 0));
    assert!(analyze(&rows, GroupBy::Correlated).recorded());
}

/// Catches: a start before its queue time wrapping into a huge wait, an end before the
/// build's first queue wrapping into a huge span, and either being used silently.
#[test]
fn times_out_of_order_are_counted_and_not_used() {
    let rows = [
        // Started before it was queued: no wait from this attempt.
        row("r1", Some("a"), None, (10, Some(5), Some(20))),
        // Ended before the build's earliest queue: no span for the build.
        row("r2", Some("b"), None, (50, Some(51), Some(40))),
    ];
    let got = analyze(&rows, GroupBy::Invocation);
    assert_eq!(got.out_of_order, 2);
    assert_eq!(
        got.runs,
        [
            BuildRun {
                build: "a".into(),
                attempts: 1,
                action_span_us: us(10),
                queue_time_total_us: None,
                attempt_queue_wait: Spread::default(),
            },
            BuildRun {
                build: "b".into(),
                attempts: 1,
                action_span_us: None,
                queue_time_total_us: us(1),
                attempt_queue_wait: spread(1, 1),
            },
        ]
    );
}

/// Catches: a JSON run whose keys differ from `js-analysis`'s `RunRecord` (which refuses
/// an unknown field and needs `setting`, `arm`, `run_index`, `seed`, `commit`, `label` and
/// `metrics`), a label that is not the caller's (a made-up setting, seed or commit), a
/// metric that is zero where the build has no value, a metric under another name, and a
/// run index that is not the build's place in queue order.
#[test]
fn per_run_json_has_the_fields_of_a_run_record_and_the_callers_labels() {
    let records = analyze(&fixture(), GroupBy::Invocation).run_records(&labels("arm-x"));
    assert_eq!(records.len(), 5);
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
        assert_eq!(r["setting"], "k7");
        assert_eq!(r["arm"], "arm-x");
        assert_eq!(r["run_index"], i);
        assert_eq!(r["seed"], 7);
        assert_eq!(r["commit"], "abc123");
        assert_eq!(r["label"], "");
        let mut names: Vec<&str> = r["metrics"]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        names.sort_unstable();
        assert_eq!(
            names,
            [
                "build_action_span_us",
                "build_attempt_queue_wait_p50_us",
                "build_attempt_queue_wait_p90_us",
                "build_attempts",
                "build_queue_time_total_us"
            ]
        );
    }
    assert_eq!(
        records[0]["metrics"],
        json!({
            "build_action_span_us": 20e6,
            "build_queue_time_total_us": 6e6,
            "build_attempt_queue_wait_p50_us": 2e6,
            "build_attempt_queue_wait_p90_us": 4e6,
            "build_attempts": 2.0,
        })
    );
    assert_eq!(
        records[4]["metrics"]["build_action_span_us"],
        Value::Null,
        "no span is not zero"
    );
    let rows = [row("r1", Some("a"), None, (10, Some(5), Some(20)))];
    let none = analyze(&rows, GroupBy::Invocation).run_records(&labels("a"));
    assert_eq!(
        none[0]["metrics"]["build_attempt_queue_wait_p50_us"],
        Value::Null,
        "no wait is not zero"
    );
}

/// Catches: runs numbered in the order of their ids or of the input rows (a build's index
/// would move with its name or with which row came first), a build's first queue taken from
/// its first row instead of the earliest of its rows, and a tie broken by anything but the
/// id. Builds: `z` queued 0; `b` has a first row queued 9 and a later one queued 1, so its
/// earliest queue is 1; `a` and `m` both queued 5, so `a` comes first.
#[test]
fn run_index_follows_the_earliest_queue_then_the_id() {
    let rows = [
        row("r1", Some("b"), None, (9, Some(10), Some(11))),
        row("r2", Some("m"), None, (5, Some(6), Some(7))),
        row("r3", Some("a"), None, (5, Some(6), Some(7))),
        row("r4", Some("z"), None, (0, Some(1), Some(2))),
        row("r5", Some("b"), None, (1, Some(2), Some(3))),
    ];
    let got = analyze(&rows, GroupBy::Invocation);
    let order: Vec<&str> = got.runs.iter().map(|r| r.build.as_str()).collect();
    assert_eq!(order, ["z", "b", "a", "m"]);
    let records = got.run_records(&labels("x"));
    let attempts: Vec<&Value> = records
        .iter()
        .map(|r| &r["metrics"]["build_attempts"])
        .collect();
    assert_eq!(attempts, [1.0, 2.0, 1.0, 1.0]);
    for (i, r) in records.iter().enumerate() {
        assert_eq!(r["run_index"], i);
    }
}
