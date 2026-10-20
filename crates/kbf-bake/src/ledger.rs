//! The evaluator's own earlier scrapes of the queue-to-start histogram, kept in a file so the
//! latency baseline can be taken from them (section 7: the farm keeps no time series, so the
//! seven days before a deploy are the evaluator's own samples of them).
//!
//! `kbf-bake sample` appends one reading; `kbf-bake baseline` reads the window. A ledger that
//! does not reach back over the window is no baseline: the answer is an error, never a
//! shorter baseline.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::{WriteError, write_json};

use crate::eval::{follow_buckets, percentile};
use crate::state::BucketCount;
use crate::table::{BASELINE_SECS, BASELINE_SLACK_SECS};

/// The ledger's format number.
pub const LEDGER_SCHEMA: u32 = 1;

/// One cumulative bucket; `le` is absent for `+Inf` (JSON cannot hold infinity).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Point {
    /// The upper bound, seconds.
    pub le: Option<f64>,
    /// The cumulative count.
    pub count: f64,
}

/// One reading of the histogram.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Snapshot {
    /// When (unix seconds).
    pub at: u64,
    /// The buckets.
    pub buckets: Vec<Point>,
}

/// The ledger.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Ledger {
    /// [`LEDGER_SCHEMA`].
    pub schema: u32,
    /// Readings, oldest first.
    pub snapshots: Vec<Snapshot>,
}

impl Default for Ledger {
    fn default() -> Self {
        Self {
            schema: LEDGER_SCHEMA,
            snapshots: Vec::new(),
        }
    }
}

impl Ledger {
    /// Read the ledger at `path`; an empty one when there is no file. A file that is not a
    /// ledger is an error: a history is never silently started over.
    pub fn load(path: &Path) -> Result<Self, String> {
        let bytes = match std::fs::read(path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => return Err(format!("read {}: {e}", path.display())),
        };
        let ledger: Self = serde_json::from_slice(&bytes)
            .map_err(|e| format!("{} is not a ledger: {e}", path.display()))?;
        if ledger.schema != LEDGER_SCHEMA {
            return Err(format!(
                "{}: schema {}, this reader knows {LEDGER_SCHEMA}",
                path.display(),
                ledger.schema
            ));
        }
        Ok(ledger)
    }

    /// Write the ledger to `path`, atomically.
    pub fn save(&self, path: &Path) -> Result<(), WriteError> {
        write_json(path, self)
    }

    /// Add a reading taken at `at`. Readings older than twice the baseline window are
    /// dropped. A reading not after the newest is refused (the clock went back).
    pub fn push(&mut self, at: u64, buckets: &[(f64, f64)]) -> Result<(), String> {
        if let Some(last) = self.snapshots.last().filter(|s| s.at >= at) {
            return Err(format!(
                "a reading at {at} is not after the newest one, at {}",
                last.at
            ));
        }
        self.snapshots.push(Snapshot {
            at,
            buckets: buckets
                .iter()
                .map(|&(bound, count)| Point {
                    le: bound.is_finite().then_some(bound),
                    count,
                })
                .collect(),
        });
        let keep_from = at.saturating_sub(2 * BASELINE_SECS);
        self.snapshots.retain(|s| s.at >= keep_from);
        Ok(())
    }

    /// The queue-to-start p99 over the [`BASELINE_SECS`] before `at`, in seconds, from the
    /// increase of the buckets across the readings. An error says why there is none: the
    /// ledger does not reach back far enough, its newest reading is too old, nothing was
    /// observed, or the p99 is past the largest bucket.
    pub fn queue_p99(&self, at: u64) -> Result<f64, String> {
        let from = at.saturating_sub(BASELINE_SECS);
        let window: Vec<&Snapshot> = self
            .snapshots
            .iter()
            .filter(|s| s.at >= from && s.at <= at)
            .collect();
        let (Some(first), Some(last)) = (window.first(), window.last()) else {
            return Err("the ledger has no reading in the seven days before the deploy".to_owned());
        };
        if first.at > from + BASELINE_SLACK_SECS {
            return Err(format!(
                "the ledger's first reading in the window is at {}, {} s after its start: it does not cover seven days",
                first.at,
                first.at - from
            ));
        }
        if last.at + BASELINE_SLACK_SECS < at {
            return Err(format!(
                "the ledger's newest reading is at {}, {} s before the deploy",
                last.at,
                at - last.at
            ));
        }
        let mut acc: Vec<BucketCount> = Vec::new();
        for s in &window {
            let pairs: Vec<(f64, f64)> = s
                .buckets
                .iter()
                .map(|p| (p.le.unwrap_or(f64::INFINITY), p.count))
                .collect();
            follow_buckets(&mut acc, &pairs);
        }
        let p99 = percentile(&acc, 0.99).ok_or("the ledger holds no histogram")?;
        if p99 <= 0.0 {
            return Err("nothing was observed in the seven days before the deploy".to_owned());
        }
        if !p99.is_finite() {
            return Err("the p99 is past the histogram's largest bucket".to_owned());
        }
        Ok(p99)
    }
}
