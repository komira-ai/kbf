//! Per-build numbers from an export: one build's wall time and queue wait, and the
//! per-run values of the scheduler paper's arms.
//!
//! # Definition
//!
//! A row names its build by the server's record of the Execute request's `RequestMetadata`:
//! the `correlated_invocations_id` when the client sent one, else the `tool_invocation_id`.
//! A row with neither is in no build and is counted ([`BuildStats::rows_without_id`]); an
//! export from a server that does not record the ids has no builds at all, and nothing is
//! estimated for it.
//!
//! * **Wall**: the latest end of the build's attempts less the earliest queue time of its
//!   attempts. A build with an attempt that has no end (lost, or in flight at the export) has
//!   no wall ([`BuildStats::unfinished`]): its end is not known.
//! * **Queue wait**: the sum of `started - queued` over the build's attempts that started.
//!   An attempt that never started adds nothing, so the sum is the build's recorded
//!   queueing, not its latency to first work.
//! * **Percentiles** are exact nearest rank over the builds that have the value, as the
//!   per-attempt ones are.
//! * A timestamp pair out of order is counted ([`BuildStats::out_of_order`]) and not used.
//!
//! # Per-run JSON
//!
//! [`BuildStats::run_records`] writes one object per build with the fields of
//! `js-analysis`'s `RunRecord` (komira-ai/jobscheduler, `crates/js-analysis/src/types.rs`),
//! which refuses an unknown field: `setting`, `arm`, `run_index`, `seed`, `commit`, `label`
//! and `metrics`. A build is one run. What an export cannot say has a neutral value: `seed`
//! is 0, `commit` and `label` are empty, `setting` is [`SETTING`]. `run_index` is the
//! build's place in queue order. `metrics` holds `build_wall_us`, `build_queue_wait_us` and
//! `build_attempts`; a metric the build has no value for is `null`, which `RunRecord` reads
//! as not available.

use std::collections::BTreeMap;

use serde_json::{Value, json};

use crate::row::Row;

/// The `setting` of every run record: an export has no varied parameter.
pub const SETTING: &str = "build";

/// A median and a 90th percentile, in microseconds; `None` when there is no sample.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Spread {
    /// Median (nearest rank).
    pub p50: Option<u64>,
    /// 90th percentile (nearest rank).
    pub p90: Option<u64>,
}

/// One build's values.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BuildRun {
    /// The build: its correlated id, else its invocation id.
    pub build: String,
    /// Attempts the export holds for it.
    pub attempts: u64,
    /// Wall in microseconds; `None` when an attempt has no end or a time is out of order.
    pub wall_us: Option<u64>,
    /// Queue wait in microseconds; `None` when no attempt gave one.
    pub queue_wait_us: Option<u64>,
}

/// The per-build numbers of one arm.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BuildStats {
    /// Builds, ordered by their earliest queue time (then by name).
    pub runs: Vec<BuildRun>,
    /// Rows with no invocation id and no correlated id: in no build.
    pub rows_without_id: u64,
    /// Builds with an attempt that has no end: no wall.
    pub unfinished: u64,
    /// Timestamp pairs where the later is before the earlier.
    pub out_of_order: u64,
    /// Builds that gave a wall.
    pub wall_samples: u64,
    /// Wall percentiles.
    pub wall: Spread,
    /// Builds that gave a queue wait.
    pub queue_wait_samples: u64,
    /// Queue wait percentiles.
    pub queue_wait: Spread,
}

impl BuildStats {
    /// Whether the export names any build.
    pub fn recorded(&self) -> bool {
        !self.runs.is_empty()
    }

    /// One `js-analysis` `RunRecord` per build of arm `arm`, as JSON values.
    pub fn run_records(&self, arm: &str) -> Vec<Value> {
        let number = |v: Option<u64>| v.map_or(Value::Null, |v| json!(v as f64));
        self.runs
            .iter()
            .enumerate()
            .map(|(i, run)| {
                json!({
                    "setting": SETTING,
                    "arm": arm,
                    "run_index": i,
                    "seed": 0,
                    "commit": "",
                    "label": "",
                    "metrics": {
                        "build_wall_us": number(run.wall_us),
                        "build_queue_wait_us": number(run.queue_wait_us),
                        "build_attempts": run.attempts as f64,
                    },
                })
            })
            .collect()
    }
}

/// What one build's rows have said so far.
#[derive(Default)]
struct Open {
    attempts: u64,
    first_queued: u64,
    last_ended: Option<u64>,
    unfinished: bool,
    wait: Option<u64>,
}

/// The value at percentile `p` (1 to 100) of `sorted`, by nearest rank.
fn nearest(sorted: &[u64], p: u64) -> Option<u64> {
    let rank = (p * sorted.len() as u64).div_ceil(100).max(1);
    sorted.get((rank - 1) as usize).copied()
}

fn spread(mut values: Vec<u64>) -> Spread {
    values.sort_unstable();
    Spread {
        p50: nearest(&values, 50),
        p90: nearest(&values, 90),
    }
}

/// The per-build numbers of `rows`.
pub fn analyze(rows: &[Row]) -> BuildStats {
    let mut stats = BuildStats::default();
    let mut builds: BTreeMap<&str, Open> = BTreeMap::new();
    for row in rows {
        let Some(build) = row
            .correlated_invocations_id
            .as_deref()
            .or(row.invocation_id.as_deref())
        else {
            stats.rows_without_id += 1;
            continue;
        };
        let open = builds.entry(build).or_insert_with(|| Open {
            first_queued: row.queued_us,
            ..Open::default()
        });
        open.attempts += 1;
        open.first_queued = open.first_queued.min(row.queued_us);
        match row.ended_us {
            None => open.unfinished = true,
            Some(ended) => open.last_ended = open.last_ended.max(Some(ended)),
        }
        if let Some(started) = row.started_us {
            match started.checked_sub(row.queued_us) {
                Some(wait) => *open.wait.get_or_insert(0) += wait,
                None => stats.out_of_order += 1,
            }
        }
    }
    let mut ordered: Vec<(&str, Open)> = builds.into_iter().collect();
    ordered.sort_by_key(|(name, open)| (open.first_queued, *name));
    let (mut walls, mut waits) = (Vec::new(), Vec::new());
    for (name, open) in ordered {
        stats.unfinished += u64::from(open.unfinished);
        let wall = open
            .last_ended
            .filter(|_| !open.unfinished)
            .and_then(|ended| {
                let wall = ended.checked_sub(open.first_queued);
                stats.out_of_order += u64::from(wall.is_none());
                wall
            });
        walls.extend(wall);
        waits.extend(open.wait);
        stats.runs.push(BuildRun {
            build: name.to_owned(),
            attempts: open.attempts,
            wall_us: wall,
            queue_wait_us: open.wait,
        });
    }
    stats.wall_samples = walls.len() as u64;
    stats.wall = spread(walls);
    stats.queue_wait_samples = waits.len() as u64;
    stats.queue_wait = spread(waits);
    stats
}
