//! The counters of `--placement-predictions`: how many operations got a prediction, why the
//! others did not, how the finished runs fell against their predictions (shadow), and the
//! nodes whose actions cgroup has a CPU quota while placement reads the load. Rendered with
//! the cache's families ([`super::CacheMetrics::render`]).

use std::fmt::Write as _;

use super::{Families, Level};

/// The figures a finished run is compared with its prediction on, as the `axis` label spells
/// them: the cores it used on average, its peak memory, and its life in minutes.
pub const PREDICTION_AXES: [&str; 3] = ["cpu", "mem", "life"];

/// How a finished run fell against its prediction on one axis, as the `bucket` label spells
/// it: `under` is above the prediction, `fit` is at most the prediction and at least half
/// of it, `over` is below half of it.
pub const PREDICTION_BUCKETS: [&str; 3] = ["under", "fit", "over"];

/// Why a new operation got no prediction, as the `reason` label of
/// `kbf_prediction_unset_total` spells it: the estimator's fallbacks (`missing`, `stale`,
/// `cold`, `corrupt`, `over_cap`) and what the farm does not ask it about (`kind`, `no_os`,
/// `no_subject`).
pub const PREDICTION_UNSET: [&str; 8] = [
    "missing",
    "stale",
    "cold",
    "corrupt",
    "over_cap",
    "kind",
    "no_os",
    "no_subject",
];

/// The predictions' counts, set by the owner of the tally each time it publishes.
#[derive(Debug, Default)]
pub struct PredictionTotals {
    made: Level,
    unset: [Level; 8],
    vs_run: [[Level; 3]; 3],
    cpu_limited_nodes: Level,
}

impl PredictionTotals {
    /// Sets the count of the operations that were given a prediction.
    pub fn set_made(&self, total: u64) {
        self.made.set(total);
    }

    /// The count of the operations that were given a prediction.
    #[must_use]
    pub fn made(&self) -> u64 {
        self.made.get()
    }

    /// Sets the count of the operations with no prediction for reason number `reason`
    /// ([`PREDICTION_UNSET`]).
    pub fn set_unset(&self, reason: usize, total: u64) {
        if let Some(level) = self.unset.get(reason) {
            level.set(total);
        }
    }

    /// The count of the operations with no prediction for reason number `reason`.
    #[must_use]
    pub fn unset(&self, reason: usize) -> u64 {
        self.unset.get(reason).map_or(0, Level::get)
    }

    /// Sets the count of the finished runs that fell in bucket number `bucket`
    /// ([`PREDICTION_BUCKETS`]) on axis number `axis` ([`PREDICTION_AXES`]).
    pub fn set_vs_run(&self, axis: usize, bucket: usize, total: u64) {
        if let Some(level) = self.vs_run.get(axis).and_then(|row| row.get(bucket)) {
            level.set(total);
        }
    }

    /// The count of the finished runs that fell in `bucket` on `axis`.
    #[must_use]
    pub fn vs_run(&self, axis: usize, bucket: usize) -> u64 {
        self.vs_run
            .get(axis)
            .and_then(|row| row.get(bucket))
            .map_or(0, Level::get)
    }

    /// Sets how many nodes report an actions cgroup with a CPU quota while placement reads
    /// the load.
    pub fn set_cpu_limited_nodes(&self, nodes: u64) {
        self.cpu_limited_nodes.set(nodes);
    }

    /// How many nodes report an actions cgroup with a CPU quota while placement reads the
    /// load.
    #[must_use]
    pub fn cpu_limited_nodes(&self) -> u64 {
        self.cpu_limited_nodes.get()
    }

    pub(super) fn render(&self, out: &mut Families) {
        out.counter(
            "kbf_prediction_made_total",
            "New operations the estimator gave a prediction (--placement-predictions shadow or on)",
            self.made(),
        );
        let name = "kbf_prediction_unset_total";
        out.head(
            name,
            "counter",
            "New operations that got no prediction, by why (--placement-predictions shadow or on)",
        );
        for (reason, label) in PREDICTION_UNSET.iter().enumerate() {
            let _ = writeln!(out.0, "{name}{{reason=\"{label}\"}} {}", self.unset(reason));
        }
        let name = "kbf_prediction_vs_run_total";
        out.head(
            name,
            "counter",
            "Finished runs compared with the prediction made for their operation, by the figure \
             and whether the run was under, within or over half of the prediction (shadow only)",
        );
        for (axis, axis_label) in PREDICTION_AXES.iter().enumerate() {
            for (bucket, bucket_label) in PREDICTION_BUCKETS.iter().enumerate() {
                let _ = writeln!(
                    out.0,
                    "{name}{{axis=\"{axis_label}\",bucket=\"{bucket_label}\"}} {}",
                    self.vs_run(axis, bucket)
                );
            }
        }
        out.gauge(
            "kbf_placement_cpu_limited_nodes",
            "Nodes whose latest load reading says the actions cgroup has a CPU quota while \
             --placement-load is on: nodes that placement charges by prediction alone, which is \
             a misconfiguration",
            self.cpu_limited_nodes(),
        );
    }
}
