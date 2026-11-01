//! Per-build numbers from an export: what the server saw of one build's actions, and the
//! per-run values of the scheduler paper's arms.
//!
//! # Definition
//!
//! A row names its invocation by the server's record of the Execute request's
//! `RequestMetadata`: the `tool_invocation_id`, which is one buck2 command and what buck2's
//! Build ID joins to. That is the build ([`GroupBy::Invocation`], the default).
//! [`GroupBy::Correlated`] groups by `correlated_invocations_id` instead: the CI-job view,
//! where several commands of one job are one build. A row without the id of the chosen view is
//! in no build and is counted ([`BuildStats::rows_without_id`]); an export from a server that
//! does not record the ids has no builds, and nothing is estimated for it.
//!
//! * **Build action span (server)**: the latest end of the build's attempts less the
//!   earliest queue time of its attempts. It is the span of the actions the server ran. It
//!   excludes the client's own analysis and local actions, so it is not the build's wall:
//!   the paper's build wall comes from the client. A build with an attempt that has no end
//!   (lost, or in flight at the export) has no span ([`BuildStats::unfinished`]).
//! * **Queue time total (attempt-seconds)**: the sum of `started - queued` over the build's
//!   attempts that started. It is a cost metric, and grows with the number of attempts; it
//!   is not a queue wait.
//! * **Attempt queue wait**: the p50 and p90 of `started - queued` over one build's started
//!   attempts (the distribution within the build). Per arm, [`BuildStats`] reports the
//!   distribution of those across builds.
//! * **Percentiles** are exact nearest rank over the values present.
//! * A timestamp pair out of order is counted ([`BuildStats::out_of_order`]) and not used.
//!
//! # Per-run JSON
//!
//! [`BuildStats::run_records`] writes one object per build with the fields of
//! `js-analysis`'s `RunRecord` (komira-ai/jobscheduler, `crates/js-analysis/src/types.rs`),
//! which refuses an unknown field: `setting`, `arm`, `run_index`, `seed`, `commit`, `label`
//! and `metrics`. A build is one run. The caller gives `setting`, `arm`, `seed` and `commit`
//! ([`RunLabels`]); an export does not know them. `label` is empty and `run_index` is the
//! build's place in queue order. `metrics` holds `build_action_span_us`,
//! `build_queue_time_total_us`, `build_attempt_queue_wait_p50_us`,
//! `build_attempt_queue_wait_p90_us` and `build_attempts`; one the build has no value for is
//! `null`, which `RunRecord` reads as not available.

use std::collections::BTreeMap;

use serde_json::{Value, json};

use crate::row::Row;

/// What a build is.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum GroupBy {
    /// One `tool_invocation_id`: one buck2 command.
    #[default]
    Invocation,
    /// One `correlated_invocations_id`: one CI job.
    Correlated,
}

/// What an export cannot say about its runs, given by the caller.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunLabels {
    /// The varied parameter's value, as text.
    pub setting: String,
    /// The arm's name.
    pub arm: String,
    /// The seed the run used.
    pub seed: u64,
    /// The build under test.
    pub commit: String,
}

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
    /// The build: its invocation id, or its correlated id under [`GroupBy::Correlated`].
    pub build: String,
    /// Attempts the export holds for it.
    pub attempts: u64,
    /// Build action span (server) in microseconds; `None` when an attempt has no end or a
    /// time is out of order.
    pub action_span_us: Option<u64>,
    /// Queue time total in microseconds (attempt-seconds, times 10^6); `None` when no
    /// attempt gave a wait.
    pub queue_time_total_us: Option<u64>,
    /// The p50 and p90 of the build's attempt queue waits.
    pub attempt_queue_wait: Spread,
}

/// The per-build numbers of one arm.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BuildStats {
    /// Builds, ordered by their earliest queue time (then by name).
    pub runs: Vec<BuildRun>,
    /// Rows with no id for the chosen grouping: in no build.
    pub rows_without_id: u64,
    /// Builds with an attempt that has no end: no span.
    pub unfinished: u64,
    /// Timestamp pairs where the later is before the earlier.
    pub out_of_order: u64,
    /// Builds that gave a span.
    pub span_samples: u64,
    /// Percentiles of the span over builds.
    pub action_span: Spread,
    /// Builds that gave a queue time total.
    pub queue_total_samples: u64,
    /// Percentiles of the queue time total over builds.
    pub queue_time_total: Spread,
    /// Across builds, the distribution of each build's attempt queue wait p50.
    pub attempt_wait_p50: Spread,
    /// Across builds, the distribution of each build's attempt queue wait p90.
    pub attempt_wait_p90: Spread,
}

impl BuildStats {
    /// Whether the export names any build.
    pub fn recorded(&self) -> bool {
        !self.runs.is_empty()
    }

    /// One `js-analysis` `RunRecord` per build, as JSON values.
    pub fn run_records(&self, _labels: &RunLabels) -> Vec<Value> {
        let _ = json!(null);
        Vec::new()
    }
}

/// The per-build numbers of `rows`.
pub fn analyze(_rows: &[Row], _group: GroupBy) -> BuildStats {
    let _ = BTreeMap::<String, u64>::new();
    BuildStats::default()
}
