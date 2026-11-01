//! Two arms side by side: the four numbers of the scheduler paper, from two exports.
//!
//! # Definition
//!
//! An arm is one export (a directory of `runs-*.jsonl` files, or one file); each row is one
//! attempt. The report is read-only and uses only fields the row records.
//!
//! * **Attempts**: rows. **Started**: rows with a start.
//! * **Queue wait**: `started - queued` of each attempt that started, whatever its outcome;
//!   p50 and p90. An attempt that never started has no wait; it is counted by outcome
//!   ([`ArmStats::never_started`]) and is not in the percentiles.
//! * **Wall**: `ended - started` of each attempt whose outcome is `ok` and that has both
//!   times; p50 and p90. Failed, killed and lost attempts are not walls.
//! * **Makespan**: the latest recorded end of the arm less its earliest queue time.
//! * **Kills per 1,000 attempts**: attempts whose `memory_kill` is not `none`, split into
//!   `own_limit` and `node_pressure`, each times 1,000 over all attempts. A kind this tool
//!   does not know is counted apart ([`Kills::other`]) and left out of the split.
//! * **Percentiles** are exact nearest rank over the rows: the value at rank
//!   `ceil(p * n / 100)` of the sorted values (never the histogram's approximate ones).
//! * **Absent is not zero**: an arm with no sample of a number prints `-` for it, and so
//!   does the difference when either arm lacks it. The difference is arm B less arm A.
//!
//! A timestamp pair that is out of order, and an `ok` attempt with no end, are counted
//! ([`ArmStats::out_of_order`], [`ArmStats::ok_without_end`]) and not used. An
//! out-of-order row is left out of wall and wait but its end still sets the makespan's end;
//! a latest end before the earliest queue time gives no makespan (absent, counted once as
//! out of order).
//!
//! B - A is a difference of two single numbers, not a significance test; the paper's rule
//! needs the 95% Welch CI over runs to exclude 0.
//! What the export
//! cannot say is printed with every report ([`NOT_RECORDED`]); nothing is estimated.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write;

use serde_json::{Value, json};

use crate::build_report;
use crate::builds::{BuildStats, GroupBy, RunLabels};
use crate::row::Row;

/// What the numbers cannot see. Printed with every report.
pub const NOT_RECORDED: [&str; 5] = [
    "Per-build wall and queue wait: a row carries no invocation id, so only per-attempt numbers exist.",
    "Attempts the history does not hold (no invocation id, aged out of retention) are in no number.",
    "An attempt with no end (lost, or in flight at the export) has no wall and cannot move the makespan's end; the makespan is the export's window of recorded times, not a build's.",
    "The queue wait of an attempt that never started is not recorded; it is counted by outcome, never given a wait.",
    "The row does not say which scheduler core or flags ran it: an arm is whatever export was given.",
];

/// What the numbers of `a` and `b` cannot see: [`NOT_RECORDED`], less the per-build entry
/// when both arms name their builds.
pub fn not_recorded(a: &ArmStats, b: &ArmStats) -> Vec<&'static str> {
    // The per-build entry is the first of the list.
    let named = a.builds.recorded() && b.builds.recorded();
    NOT_RECORDED.into_iter().skip(usize::from(named)).collect()
}

/// A median and a 90th percentile, in microseconds; `None` when there is no sample.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Percentiles {
    /// Median (nearest rank).
    pub p50: Option<u64>,
    /// 90th percentile (nearest rank).
    pub p90: Option<u64>,
}

/// Attempts killed for memory, by kind.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Kills {
    /// `memory_kill` = `own_limit`.
    pub own_limit: u64,
    /// `memory_kill` = `node_pressure`.
    pub node_pressure: u64,
    /// Any other value but `none`.
    pub other: u64,
}

/// The numbers of one arm.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ArmStats {
    /// Rows read.
    pub attempts: u64,
    /// Rows with a start.
    pub started: u64,
    /// Rows with no start, by outcome.
    pub never_started: BTreeMap<String, u64>,
    /// Attempts that gave a queue wait.
    pub queue_wait_samples: u64,
    /// Queue wait percentiles.
    pub queue_wait: Percentiles,
    /// `ok` attempts that gave a wall.
    pub wall_samples: u64,
    /// Wall percentiles.
    pub wall: Percentiles,
    /// Latest end less earliest queue, when any attempt has an end.
    pub makespan_us: Option<u64>,
    /// Memory kills.
    pub kills: Kills,
    /// `ok` attempts with a start and no end.
    pub ok_without_end: u64,
    /// Timestamp pairs (queue/start, start/end) where the later is before the earlier,
    /// plus one when the latest end is before the earliest queue time. Such rows are
    /// excluded from wall and wait but still set the makespan end.
    pub out_of_order: u64,
    /// The per-build numbers of rows that name their invocation or build.
    pub builds: BuildStats,
}

impl ArmStats {
    /// `kills` per 1,000 attempts of this arm; `None` for an arm with no attempt.
    pub fn per_thousand(&self, kills: u64) -> Option<f64> {
        (self.attempts > 0).then(|| kills as f64 * 1000.0 / self.attempts as f64)
    }
}

/// The value at percentile `p` (1 to 100) of `sorted`, by nearest rank.
fn nearest(sorted: &[u64], p: u64) -> Option<u64> {
    let rank = (p * sorted.len() as u64).div_ceil(100).max(1);
    sorted.get((rank - 1) as usize).copied()
}

fn percentiles(mut values: Vec<u64>) -> Percentiles {
    values.sort_unstable();
    Percentiles {
        p50: nearest(&values, 50),
        p90: nearest(&values, 90),
    }
}

/// The numbers of `rows`.
pub fn analyze(rows: &[Row]) -> ArmStats {
    analyze_grouped(rows, GroupBy::default())
}

/// The numbers of `rows`, their builds being what `group` says.
pub fn analyze_grouped(rows: &[Row], group: GroupBy) -> ArmStats {
    let mut stats = ArmStats::default();
    let (mut waits, mut walls) = (Vec::new(), Vec::new());
    let mut first_queued: Option<u64> = None;
    let mut last_ended: Option<u64> = None;
    for row in rows {
        stats.attempts += 1;
        match row.memory_kill.as_str() {
            "none" => {}
            "own_limit" => stats.kills.own_limit += 1,
            "node_pressure" => stats.kills.node_pressure += 1,
            _ => stats.kills.other += 1,
        }
        first_queued = first_queued
            .min(Some(row.queued_us))
            .or(Some(row.queued_us));
        last_ended = last_ended.max(row.ended_us);
        let Some(started) = row.started_us else {
            *stats.never_started.entry(row.outcome.clone()).or_default() += 1;
            continue;
        };
        stats.started += 1;
        match started.checked_sub(row.queued_us) {
            Some(wait) => waits.push(wait),
            None => stats.out_of_order += 1,
        }
        if row.outcome != "ok" {
            continue;
        }
        match row.ended_us.map(|ended| ended.checked_sub(started)) {
            None => stats.ok_without_end += 1,
            Some(None) => stats.out_of_order += 1,
            Some(Some(wall)) => walls.push(wall),
        }
    }
    stats.makespan_us = last_ended
        .zip(first_queued)
        .and_then(|(e, q)| e.checked_sub(q));
    if last_ended.is_some() && stats.makespan_us.is_none() {
        stats.out_of_order += 1;
    }
    stats.queue_wait_samples = waits.len() as u64;
    stats.queue_wait = percentiles(waits);
    stats.wall_samples = walls.len() as u64;
    stats.wall = percentiles(walls);
    let _ = group;
    stats.builds = BuildStats::default();
    stats
}

const LABEL_WIDTH: usize = 34;
const CELL_WIDTH: usize = 14;

/// One table row: the label, then A, B and B less A, right-aligned.
pub(crate) fn line(out: &mut String, label: &str, cells: [String; 3]) {
    let _ = write!(out, "{label:<LABEL_WIDTH$}");
    for cell in cells {
        let _ = write!(out, "{cell:>CELL_WIDTH$}");
    }
    out.push('\n');
}

/// Seconds with three decimals (half up) of a signed microsecond value.
pub(crate) fn secs(us: i128) -> String {
    let ms = (us.abs() + 500) / 1000;
    let sign = if us < 0 && ms > 0 { "-" } else { "" };
    format!("{sign}{}.{:03}", ms / 1000, ms % 1000)
}

pub(crate) fn count_row(out: &mut String, label: &str, a: u64, b: u64) {
    let diff = i128::from(b) - i128::from(a);
    line(out, label, [a.to_string(), b.to_string(), diff.to_string()]);
}

pub(crate) fn duration_row(out: &mut String, label: &str, a: Option<u64>, b: Option<u64>) {
    let cell = |v: Option<u64>| v.map_or_else(|| "-".to_owned(), |v| secs(i128::from(v)));
    let diff = a
        .zip(b)
        .map(|(a, b)| secs(i128::from(b) - i128::from(a)))
        .unwrap_or_else(|| "-".to_owned());
    line(out, label, [cell(a), cell(b), diff]);
}

fn rate_row(out: &mut String, label: &str, a: Option<f64>, b: Option<f64>) {
    let cell = |v: Option<f64>| v.map_or_else(|| "-".to_owned(), |v| format!("{v:.2}"));
    let diff = a.zip(b).map(|(a, b)| b - a);
    line(out, label, [cell(a), cell(b), cell(diff)]);
}

/// The two arms as a table: A, B and B less A.
pub fn render_text(a: &ArmStats, b: &ArmStats) -> String {
    let mut out = String::new();
    out.push_str("kbf estimator replay: two arms\n");
    for (name, arm) in [("A", a), ("B", b)] {
        if arm.attempts == 0 {
            let _ = writeln!(
                out,
                "arm {name} has no attempts: its numbers are absent (-), not zero"
            );
        }
    }
    line(
        &mut out,
        "",
        ["A".to_owned(), "B".to_owned(), "B - A".to_owned()],
    );
    count_row(&mut out, "attempts", a.attempts, b.attempts);
    count_row(&mut out, "started", a.started, b.started);
    let outcomes: BTreeSet<&String> = a
        .never_started
        .keys()
        .chain(b.never_started.keys())
        .collect();
    for outcome in outcomes {
        let n = |arm: &ArmStats| arm.never_started.get(outcome).copied().unwrap_or(0);
        count_row(&mut out, &format!("never started: {outcome}"), n(a), n(b));
    }
    count_row(
        &mut out,
        "queue wait samples",
        a.queue_wait_samples,
        b.queue_wait_samples,
    );
    duration_row(
        &mut out,
        "queue wait p50 (s)",
        a.queue_wait.p50,
        b.queue_wait.p50,
    );
    duration_row(
        &mut out,
        "queue wait p90 (s)",
        a.queue_wait.p90,
        b.queue_wait.p90,
    );
    count_row(
        &mut out,
        "wall samples (ok runs)",
        a.wall_samples,
        b.wall_samples,
    );
    duration_row(&mut out, "wall p50 (s)", a.wall.p50, b.wall.p50);
    duration_row(&mut out, "wall p90 (s)", a.wall.p90, b.wall.p90);
    duration_row(&mut out, "makespan (s)", a.makespan_us, b.makespan_us);
    rate_row(
        &mut out,
        "kills/1000 own_limit",
        a.per_thousand(a.kills.own_limit),
        b.per_thousand(b.kills.own_limit),
    );
    rate_row(
        &mut out,
        "kills/1000 node_pressure",
        a.per_thousand(a.kills.node_pressure),
        b.per_thousand(b.kills.node_pressure),
    );
    count_row(
        &mut out,
        "kills of another kind",
        a.kills.other,
        b.kills.other,
    );
    count_row(
        &mut out,
        "ok without an end",
        a.ok_without_end,
        b.ok_without_end,
    );
    count_row(
        &mut out,
        "times out of order",
        a.out_of_order,
        b.out_of_order,
    );
    out.push_str(
        "B - A is a difference of two single numbers, not a significance test; the paper's rule needs the 95% Welch CI over runs to exclude 0.\n",
    );
    build_report::rows(&mut out, &a.builds, &b.builds);
    out.push_str("Not recorded (not estimated):\n");
    for gap in not_recorded(a, b) {
        let _ = writeln!(out, "  - {gap}");
    }
    out
}

fn arm_json(arm: &ArmStats) -> Value {
    json!({
        "attempts": arm.attempts,
        "started": arm.started,
        "never_started": arm.never_started,
        "queue_wait_samples": arm.queue_wait_samples,
        "queue_wait_us": {"p50": arm.queue_wait.p50, "p90": arm.queue_wait.p90},
        "wall_samples": arm.wall_samples,
        "wall_us": {"p50": arm.wall.p50, "p90": arm.wall.p90},
        "makespan_us": arm.makespan_us,
        "kills": {
            "own_limit": arm.kills.own_limit,
            "node_pressure": arm.kills.node_pressure,
            "other": arm.kills.other,
        },
        "kills_per_1000": {
            "own_limit": arm.per_thousand(arm.kills.own_limit),
            "node_pressure": arm.per_thousand(arm.kills.node_pressure),
        },
        "ok_without_end": arm.ok_without_end,
        "out_of_order": arm.out_of_order,
        "builds": build_report::arm_json(&arm.builds),
    })
}

/// `b - a` as a number, or `null` when either is absent.
pub(crate) fn delta(a: Option<u64>, b: Option<u64>) -> Value {
    a.zip(b)
        .map_or(Value::Null, |(a, b)| json!(i128::from(b) - i128::from(a)))
}

fn rate_delta(a: Option<f64>, b: Option<f64>) -> Value {
    a.zip(b).map_or(Value::Null, |(a, b)| json!(b - a))
}

/// The two arms as one JSON document (times in microseconds, `null` for absent).
pub fn render_json(a: &ArmStats, b: &ArmStats) -> String {
    render_json_runs(a, b, None)
}

/// [`render_json`], with one `RunRecord`-shaped object per build under `runs` when `labels`
/// (arm A's and arm B's) are given.
pub fn render_json_runs(
    a: &ArmStats,
    b: &ArmStats,
    labels: Option<(&RunLabels, &RunLabels)>,
) -> String {
    let _ = labels;
    let doc = json!({
        "arms": {"a": arm_json(a), "b": arm_json(b)},
        "difference": {
            "attempts": delta(Some(a.attempts), Some(b.attempts)),
            "queue_wait_us": {
                "p50": delta(a.queue_wait.p50, b.queue_wait.p50),
                "p90": delta(a.queue_wait.p90, b.queue_wait.p90),
            },
            "wall_us": {
                "p50": delta(a.wall.p50, b.wall.p50),
                "p90": delta(a.wall.p90, b.wall.p90),
            },
            "makespan_us": delta(a.makespan_us, b.makespan_us),
            "builds": build_report::difference_json(&a.builds, &b.builds),
            "kills_per_1000": {
                "own_limit": rate_delta(
                    a.per_thousand(a.kills.own_limit),
                    b.per_thousand(b.kills.own_limit),
                ),
                "node_pressure": rate_delta(
                    a.per_thousand(a.kills.node_pressure),
                    b.per_thousand(b.kills.node_pressure),
                ),
            },
        },
        "not_recorded": not_recorded(a, b),
    });
    format!("{doc:#}\n")
}
