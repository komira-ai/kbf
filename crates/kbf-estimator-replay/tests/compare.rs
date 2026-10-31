//! The two-arm comparison: nearest-rank percentiles, the funnel of attempts that never
//! started, the kill split, the makespan, absent against zero, the table and JSON, the verb
//! and its read-only guard. Every time is a number written into a row.

mod common;

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use common::*;
use kbf_estimator_replay::cli::{Failure, run, write_days};
use kbf_estimator_replay::compare::{ArmStats, analyze, render_json, render_text};
use kbf_estimator_replay::row::Row;
use kbf_testkit::dir::TestDir;

const S: u64 = 1_000_000;

/// An attempt queued at `queued_s`, started at `started_s` and ended at `ended_s` (seconds
/// from the fixture epoch).
fn row(
    id: &str,
    queued_s: u64,
    started_s: Option<u64>,
    ended_s: Option<u64>,
    outcome: &str,
    kill: &str,
) -> Row {
    let mut r = attempt(id, 0, 1, outcome, None);
    r.run_id = id.to_owned();
    r.queued_us = OCT_1 + queued_s * S;
    r.started_us = started_s.map(|s| OCT_1 + s * S);
    r.ended_us = ended_s.map(|s| OCT_1 + s * S);
    r.memory_kill = kill.to_owned();
    r
}

/// An `ok` attempt queued at 0 that waited `wait_s` and ran `wall_s`.
fn ok(id: &str, wait_s: u64, wall_s: u64) -> Row {
    row(id, 0, Some(wait_s), Some(wait_s + wall_s), "ok", "none")
}

/// The `label` row of the table: its A, B and difference cells.
fn cells<'a>(text: &'a str, label: &str) -> Vec<&'a str> {
    let line = text
        .lines()
        .find(|l| l.len() > 34 && l[..34].trim() == label)
        .unwrap_or_else(|| panic!("no row {label:?} in\n{text}"));
    line[34..].split_whitespace().collect()
}

/// Catches percentiles by interpolation, by the histogram, or over a different rank:
/// ten waits of 1..=10 s put p50 at rank 5 and p90 at rank 9 whatever the row order.
#[test]
fn queue_wait_percentiles_are_nearest_rank_over_started_attempts() {
    let waits = [7, 1, 10, 3, 5, 2, 9, 4, 8, 6];
    let a: Vec<Row> = waits
        .iter()
        .enumerate()
        .map(|(i, w)| ok(&format!("a{i}"), *w, 1))
        .collect();
    let stats = analyze(&a);
    assert_eq!(stats.queue_wait_samples, 10);
    assert_eq!(stats.queue_wait.p50, Some(5 * S));
    assert_eq!(stats.queue_wait.p90, Some(9 * S));
    // Five waits: rank ceil(2.5) = 3 and ceil(4.5) = 5.
    let five = analyze(&a[..5]);
    let mut sorted: Vec<u64> = waits[..5].to_vec();
    sorted.sort_unstable();
    assert_eq!(five.queue_wait.p50, Some(sorted[2] * S));
    assert_eq!(five.queue_wait.p90, Some(sorted[4] * S));
    // Arm B waited one second longer on each attempt.
    let b: Vec<Row> = waits
        .iter()
        .enumerate()
        .map(|(i, w)| ok(&format!("b{i}"), *w + 1, 1))
        .collect();
    let text = render_text(&stats, &analyze(&b));
    assert_eq!(
        cells(&text, "queue wait p50 (s)"),
        ["5.000", "6.000", "1.000"]
    );
    assert_eq!(
        cells(&text, "queue wait p90 (s)"),
        ["9.000", "10.000", "1.000"]
    );
}

/// Catches a wall taken from a failed or killed attempt, and one taken from an attempt
/// with no end: ten ok walls of 1..=10 s, plus a failed 1000 s attempt.
#[test]
fn wall_percentiles_are_over_ok_attempts_with_both_times() {
    let mut rows: Vec<Row> = (1..=10).map(|w| ok(&format!("w{w}"), 0, w)).collect();
    rows.push(row("f", 0, Some(0), Some(1000), "failed", "none"));
    rows.push(row("noend", 0, Some(0), None, "ok", "none"));
    let stats = analyze(&rows);
    assert_eq!(stats.wall_samples, 10);
    assert_eq!(stats.wall.p50, Some(5 * S));
    assert_eq!(stats.wall.p90, Some(9 * S));
    assert_eq!(stats.ok_without_end, 1);
}

/// Catches a one-row arm reporting nothing, or two different values: every percentile of
/// one sample is that sample.
#[test]
fn a_one_row_arm_has_that_row_at_every_percentile() {
    let stats = analyze(&[ok("only", 4, 30)]);
    assert_eq!(stats.attempts, 1);
    assert_eq!(stats.queue_wait.p50, Some(4 * S));
    assert_eq!(stats.queue_wait.p90, Some(4 * S));
    assert_eq!(stats.wall.p50, Some(30 * S));
    assert_eq!(stats.wall.p90, Some(30 * S));
    assert_eq!(stats.makespan_us, Some(34 * S));
}

/// Catches a never-started attempt given a wait of zero, left out of the attempt count, or
/// not counted by its outcome.
#[test]
fn an_attempt_that_never_started_is_counted_and_has_no_wait() {
    let rows = vec![
        ok("a", 2, 5),
        row("l", 0, None, None, "lost", "none"),
        row("c1", 0, None, None, "cancelled", "none"),
        row("c2", 0, None, None, "cancelled", "none"),
    ];
    let stats = analyze(&rows);
    assert_eq!(stats.attempts, 4);
    assert_eq!(stats.started, 1);
    assert_eq!(stats.queue_wait_samples, 1);
    assert_eq!(stats.queue_wait.p50, Some(2 * S));
    let want: BTreeMap<String, u64> = [("cancelled".to_owned(), 2), ("lost".to_owned(), 1)].into();
    assert_eq!(stats.never_started, want);
    let text = render_text(&stats, &ArmStats::default());
    assert_eq!(cells(&text, "never started: cancelled"), ["2", "0", "-2"]);
    assert_eq!(cells(&text, "never started: lost"), ["1", "0", "-1"]);
}

/// Catches the kill split merged, a node-pressure kill read as the arm's own, and the rate
/// taken over kills instead of attempts: 8 attempts, one kill of each kind, 125 per 1,000.
#[test]
fn kills_per_thousand_split_own_limit_from_node_pressure() {
    let mut rows: Vec<Row> = (0..6).map(|i| ok(&format!("k{i}"), 0, 1)).collect();
    rows.push(row(
        "own",
        0,
        Some(0),
        Some(1),
        "memory_killed",
        "own_limit",
    ));
    rows.push(row(
        "np",
        0,
        Some(0),
        Some(1),
        "memory_killed",
        "node_pressure",
    ));
    let stats = analyze(&rows);
    assert_eq!(stats.kills.own_limit, 1);
    assert_eq!(stats.kills.node_pressure, 1);
    assert_eq!(stats.kills.other, 0);
    assert_eq!(stats.per_thousand(stats.kills.own_limit), Some(125.0));
    assert_eq!(stats.per_thousand(stats.kills.node_pressure), Some(125.0));
    // Arm B: 8 attempts, two own-limit kills and no pressure kill.
    let mut b: Vec<Row> = (0..6).map(|i| ok(&format!("j{i}"), 0, 1)).collect();
    b.push(row("o1", 0, Some(0), Some(1), "memory_killed", "own_limit"));
    b.push(row("o2", 0, Some(0), Some(1), "memory_killed", "own_limit"));
    let text = render_text(&stats, &analyze(&b));
    assert_eq!(
        cells(&text, "kills/1000 own_limit"),
        ["125.00", "250.00", "125.00"]
    );
    assert_eq!(
        cells(&text, "kills/1000 node_pressure"),
        ["125.00", "0.00", "-125.00"]
    );
}

/// Catches a kill kind the tool does not know vanishing, or being put in a split bucket.
#[test]
fn an_unknown_kill_kind_is_counted_apart() {
    let rows = vec![
        ok("a", 0, 1),
        row("x", 0, Some(0), Some(1), "memory_killed", "cgroup_oom"),
    ];
    let stats = analyze(&rows);
    assert_eq!(stats.kills.other, 1);
    assert_eq!(stats.kills.own_limit, 0);
    assert_eq!(stats.kills.node_pressure, 0);
    let text = render_text(&stats, &stats);
    assert_eq!(cells(&text, "kills of another kind"), ["1", "1", "0"]);
}

/// Catches a makespan from the first start, one that ignores an attempt that never ended,
/// or one that takes the earliest queue time of only the attempts that ended: queued 50
/// (never ended), 100 and 200, ends 300 and 250, so 300 - 50.
#[test]
fn makespan_runs_from_first_queued_to_last_ended() {
    let rows = vec![
        row("a", 100, Some(110), Some(300), "ok", "none"),
        row("b", 50, Some(60), None, "lost", "none"),
        row("c", 200, Some(210), Some(250), "ok", "none"),
    ];
    let stats = analyze(&rows);
    assert_eq!(stats.makespan_us, Some(250 * S));
    let text = render_text(&stats, &stats);
    assert_eq!(
        cells(&text, "makespan (s)"),
        ["250.000", "250.000", "0.000"]
    );
    // No attempt ended: there is no makespan.
    let lost = analyze(&[row("b", 50, Some(60), None, "lost", "none")]);
    assert_eq!(lost.makespan_us, None);
}

/// Catches impossible or missing times used as numbers: a start before its queue time, an
/// end before its start, an ok attempt with no end. Each is counted and none is a sample.
#[test]
fn times_out_of_order_are_counted_and_not_used() {
    let rows = vec![
        row("early", 100, Some(90), Some(120), "ok", "none"),
        row("back", 100, Some(110), Some(105), "ok", "none"),
    ];
    let stats = analyze(&rows);
    assert_eq!(stats.out_of_order, 2);
    assert_eq!(stats.queue_wait_samples, 1);
    assert_eq!(stats.queue_wait.p50, Some(10 * S));
    assert_eq!(stats.wall_samples, 1);
    assert_eq!(stats.wall.p50, Some(30 * S));
    // The end before the start still ends the arm at its recorded time.
    assert_eq!(stats.makespan_us, Some(20 * S));
}

/// Catches an empty arm printed as zeros, a difference computed against an absent number,
/// and an empty arm that stops the report: absent is `-`, in text and `null` in JSON.
#[test]
fn an_empty_arm_reports_no_numbers_not_zero() {
    let empty = analyze(&[]);
    assert_eq!(empty, ArmStats::default());
    assert_eq!(empty.queue_wait, Default::default());
    assert_eq!(empty.makespan_us, None);
    assert_eq!(empty.per_thousand(0), None);
    let full = analyze(&[ok("a", 2, 5)]);
    let text = render_text(&empty, &full);
    assert_eq!(cells(&text, "attempts"), ["0", "1", "1"]);
    for label in [
        "queue wait p50 (s)",
        "queue wait p90 (s)",
        "wall p50 (s)",
        "wall p90 (s)",
        "makespan (s)",
        "kills/1000 own_limit",
        "kills/1000 node_pressure",
    ] {
        let c = cells(&text, label);
        assert_eq!(c[0], "-", "{label}");
        assert_ne!(c[1], "-", "{label}");
        assert_eq!(c[2], "-", "{label}");
    }
    assert!(text.contains("arm A has no attempts"), "{text}");
    assert!(!text.contains("arm B has no attempts"), "{text}");
    let both = render_text(&empty, &empty);
    assert!(both.contains("arm A has no attempts"), "{both}");
    assert!(both.contains("arm B has no attempts"), "{both}");
    let json: serde_json::Value = serde_json::from_str(&render_json(&empty, &full)).unwrap();
    assert_eq!(json["arms"]["a"]["attempts"], 0);
    assert_eq!(
        json["arms"]["a"]["queue_wait_us"]["p50"],
        serde_json::Value::Null
    );
    assert_eq!(json["arms"]["a"]["makespan_us"], serde_json::Value::Null);
    assert_eq!(
        json["arms"]["a"]["kills_per_1000"]["own_limit"],
        serde_json::Value::Null
    );
    assert_eq!(json["difference"]["makespan_us"], serde_json::Value::Null);
    assert_eq!(json["arms"]["b"]["queue_wait_us"]["p50"], 2 * S);
}

/// Catches a rate over a shared denominator, or counts compared raw: arm A has 4 attempts
/// and one kill, arm B 2,000 attempts and ten kills.
#[test]
fn arms_with_different_attempt_counts_are_each_normalised() {
    let mut a: Vec<Row> = (0..3).map(|i| ok(&format!("a{i}"), 0, 1)).collect();
    a.push(row("ak", 0, Some(0), Some(1), "memory_killed", "own_limit"));
    let mut b: Vec<Row> = (0..1990).map(|i| ok(&format!("b{i}"), 0, 1)).collect();
    for i in 0..10 {
        b.push(row(
            &format!("bk{i}"),
            0,
            Some(0),
            Some(1),
            "memory_killed",
            "own_limit",
        ));
    }
    let (sa, sb) = (analyze(&a), analyze(&b));
    assert_eq!(sa.attempts, 4);
    assert_eq!(sb.attempts, 2000);
    let text = render_text(&sa, &sb);
    assert_eq!(cells(&text, "attempts"), ["4", "2000", "1996"]);
    assert_eq!(
        cells(&text, "kills/1000 own_limit"),
        ["250.00", "5.00", "-245.00"]
    );
}

/// Catches a table that lost a column or a limit, and a JSON document that is not the
/// table's numbers: the whole fixture, text and JSON.
#[test]
fn the_table_and_the_json_carry_the_same_numbers() {
    let a = vec![
        ok("a1", 1, 10),
        ok("a2", 3, 20),
        row("al", 0, None, None, "lost", "none"),
    ];
    let b = vec![
        ok("b1", 2, 8),
        row("bk", 0, Some(1), Some(2), "memory_killed", "node_pressure"),
    ];
    let (sa, sb) = (analyze(&a), analyze(&b));
    let text = render_text(&sa, &sb);
    let header = text
        .lines()
        .find(|l| l.contains("B - A"))
        .expect("a header");
    assert_eq!(
        header.split_whitespace().collect::<Vec<_>>(),
        ["A", "B", "B", "-", "A"]
    );
    assert_eq!(cells(&text, "started"), ["2", "2", "0"]);
    assert_eq!(cells(&text, "queue wait samples"), ["2", "2", "0"]);
    assert_eq!(cells(&text, "wall samples (ok runs)"), ["2", "1", "-1"]);
    assert_eq!(cells(&text, "wall p50 (s)"), ["10.000", "8.000", "-2.000"]);
    assert_eq!(cells(&text, "ok without an end"), ["0", "0", "0"]);
    assert_eq!(cells(&text, "times out of order"), ["0", "0", "0"]);
    assert!(text.contains("Not recorded"), "{text}");
    for line in kbf_estimator_replay::compare::NOT_RECORDED {
        assert!(text.contains(line), "{line}");
    }
    let json: serde_json::Value = serde_json::from_str(&render_json(&sa, &sb)).unwrap();
    assert_eq!(json["arms"]["a"]["attempts"], 3);
    assert_eq!(json["arms"]["a"]["never_started"]["lost"], 1);
    assert_eq!(json["arms"]["a"]["wall_us"]["p50"], 10 * S);
    assert_eq!(json["arms"]["b"]["kills"]["node_pressure"], 1);
    assert_eq!(json["arms"]["b"]["kills_per_1000"]["node_pressure"], 500.0);
    assert_eq!(json["difference"]["wall_us"]["p50"], -2 * (S as i64));
    assert_eq!(json["difference"]["attempts"], -1);
    assert_eq!(json["not_recorded"].as_array().unwrap().len(), 5);
}

fn arm_dirs(a: &[Row], b: &[Row], name: &str) -> TestDir {
    let tmp = TestDir::under(env!("CARGO_TARGET_TMPDIR"), name);
    write_days(a, &tmp.join("a")).unwrap();
    write_days(b, &tmp.join("b")).unwrap();
    tmp
}

fn verb(dir: &Path, extra: &[&str]) -> Result<String, Failure> {
    let mut args = vec![
        "kbf-estimator-replay".to_owned(),
        "compare".into(),
        "--arm-a".into(),
        dir.join("a").to_string_lossy().into_owned(),
        "--arm-b".into(),
        dir.join("b").to_string_lossy().into_owned(),
    ];
    args.extend(extra.iter().map(|s| (*s).to_owned()));
    let mut out = Vec::new();
    run(args, &mut out)?;
    Ok(String::from_utf8(out).unwrap())
}

/// Catches arms swapped, a verb that ignores `--json`, and a missing arm reported as
/// an empty one.
#[test]
fn the_verb_reads_two_exports_and_prints_text_or_json() {
    let dir = arm_dirs(&[ok("a", 1, 10)], &[ok("b", 5, 10)], "compare-verb");
    let text = verb(&dir, &[]).unwrap();
    assert!(text.contains("A: "), "{text}");
    assert!(
        text.contains(&dir.join("b").to_string_lossy().into_owned()),
        "{text}"
    );
    assert_eq!(
        cells(&text, "queue wait p50 (s)"),
        ["1.000", "5.000", "4.000"]
    );
    let json: serde_json::Value = serde_json::from_str(&verb(&dir, &["--json"]).unwrap()).unwrap();
    assert_eq!(json["arms"]["a"]["queue_wait_us"]["p50"], S);
    assert_eq!(json["arms"]["b"]["queue_wait_us"]["p50"], 5 * S);
    let missing = dir.join("none");
    let err = run(
        [
            "kbf-estimator-replay",
            "compare",
            "--arm-a",
            &dir.join("a").to_string_lossy(),
            "--arm-b",
            &missing.to_string_lossy(),
        ],
        &mut Vec::new(),
    )
    .unwrap_err();
    assert!(
        matches!(err, Failure::Run(ref t) if t.contains("none")),
        "{err:?}"
    );
    let usage = run(
        ["kbf-estimator-replay", "compare", "--arm-a", "x"],
        &mut Vec::new(),
    );
    assert!(matches!(usage, Err(Failure::Usage(_))), "{usage:?}");
}

/// Everything under `dir`: path to bytes.
fn snapshot(dir: &Path, into: &mut BTreeMap<String, Vec<u8>>) {
    for entry in fs::read_dir(dir).unwrap().flatten() {
        let path = entry.path();
        if path.is_dir() {
            snapshot(&path, into);
        } else {
            into.insert(
                path.to_string_lossy().into_owned(),
                fs::read(&path).unwrap(),
            );
        }
    }
}

/// Catches a verb that writes a file, a cache or a report beside the arms: the tree around
/// both arms is the same bytes before and after, text and JSON, and the output is on the
/// writer it was given.
#[test]
fn compare_never_writes_outside_stdout() {
    let dir = arm_dirs(&[ok("a", 1, 10)], &[ok("b", 5, 10)], "compare-readonly");
    let mut before = BTreeMap::new();
    snapshot(&dir, &mut before);
    assert_eq!(before.len(), 2);
    let text = verb(&dir, &[]).unwrap();
    let json = verb(&dir, &["--json"]).unwrap();
    assert!(!text.is_empty());
    assert!(!json.is_empty());
    let mut after = BTreeMap::new();
    snapshot(&dir, &mut after);
    assert_eq!(before, after);
}

/// Catches a kill rate taken over the attempts that started instead of all attempts: a
/// cancelled attempt that never started is in the denominator. Nine attempts, eight
/// started, one own-limit kill: 1000/9 per 1,000, where a denominator of 8 gives 125.
#[test]
fn the_kill_rate_counts_attempts_that_never_started() {
    let mut rows: Vec<Row> = (0..7).map(|i| ok(&format!("s{i}"), 0, 1)).collect();
    rows.push(row(
        "own",
        0,
        Some(0),
        Some(1),
        "memory_killed",
        "own_limit",
    ));
    rows.push(row("c", 0, None, None, "cancelled", "none"));
    let stats = analyze(&rows);
    assert_eq!(stats.attempts, 9);
    assert_eq!(stats.started, 8);
    assert_eq!(
        stats.per_thousand(stats.kills.own_limit),
        Some(1000.0 / 9.0)
    );
    let text = render_text(&stats, &stats);
    assert_eq!(
        cells(&text, "kills/1000 own_limit"),
        ["111.11", "111.11", "0.00"]
    );
}
