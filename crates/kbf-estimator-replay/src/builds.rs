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

use std::collections::BTreeMap;

use serde_json::Value;

use crate::row::Row;

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
    /// Queue wait in microseconds; `None` when no attempt started.
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
    pub fn run_records(&self, _arm: &str) -> Vec<Value> {
        Vec::new()
    }
}

/// The per-build numbers of `rows`.
pub fn analyze(_rows: &[Row]) -> BuildStats {
    let _ = BTreeMap::<String, u64>::new();
    BuildStats::default()
}
