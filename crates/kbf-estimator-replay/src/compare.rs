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
//! ([`ArmStats::out_of_order`], [`ArmStats::ok_without_end`]) and not used. What the export
//! cannot say is printed with every report ([`NOT_RECORDED`]); nothing is estimated.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Value, json};

use crate::row::Row;

/// What the numbers cannot see. Printed with every report.
pub const NOT_RECORDED: [&str; 5] = [
    "Per-build wall and queue wait: a row carries no invocation id, so only per-attempt numbers exist.",
    "Attempts the history does not hold (no invocation id, aged out of retention) are in no number.",
    "An attempt with no end (lost, or in flight at the export) has no wall and cannot move the makespan's end; the makespan is the export's window of recorded times, not a build's.",
    "The queue wait of an attempt that never started is not recorded; it is counted by outcome, never given a wait.",
    "The row does not say which scheduler core or flags ran it: an arm is whatever export was given.",
];

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
    /// Timestamp pairs (queue/start, start/end) where the later is before the earlier.
    pub out_of_order: u64,
}

impl ArmStats {
    /// Kills of `kind` per 1,000 attempts; `None` for an arm with no attempt.
    pub fn per_thousand(&self, kills: u64) -> Option<f64> {
        let _ = kills;
        None
    }
}

/// The numbers of `rows`.
pub fn analyze(rows: &[Row]) -> ArmStats {
    let _ = rows;
    ArmStats::default()
}

/// The two arms as a table: A, B and B less A.
pub fn render_text(a: &ArmStats, b: &ArmStats) -> String {
    let _ = (a, b, BTreeSet::<u8>::new());
    String::new()
}

/// The two arms as one JSON document (times in microseconds, `null` for absent).
pub fn render_json(a: &ArmStats, b: &ArmStats) -> String {
    let _: Value = json!({});
    let _ = (a, b);
    String::new()
}
