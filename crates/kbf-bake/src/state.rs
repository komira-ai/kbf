//! The bake's state file: what a restart needs so that it does not restart the clock.

use std::collections::BTreeSet;
use std::path::Path;

use kbf_gate::{CommitId, DocError, Sample, SetHash, Status, Waiver, Window, write_atomic};
use serde::{Deserialize, Serialize};

/// The state file's format number.
pub const STATE_SCHEMA: u32 = 1;

/// Why a state file could not be read or written.
#[derive(Debug, thiserror::Error)]
pub enum StateError {
    /// The file system refused.
    #[error("{op} state file {path}: {source}")]
    Io {
        /// `read` or `write`.
        op: &'static str,
        /// The path.
        path: String,
        /// The error.
        source: std::io::Error,
    },
    /// The bytes are not a state file.
    #[error("state file {path} is not a bake state: {why}")]
    Parse {
        /// The path.
        path: String,
        /// Why.
        why: String,
    },
}

/// A counter followed across scrapes: the increase since the first one, with a reset (the
/// value falling) read as a restart of the counter, as Prometheus does.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Counter {
    /// The last value seen.
    pub last: f64,
    /// The increase since the first value.
    pub increase: f64,
}

impl Counter {
    /// Follow `sample` in `slot`; the total increase, or `None` when the series is absent now.
    pub fn follow(slot: &mut Option<Counter>, sample: Sample) -> Option<f64> {
        let Sample::Value(v) = sample else {
            return None;
        };
        let c = slot.get_or_insert(Counter {
            last: v,
            increase: 0.0,
        });
        c.increase += if v >= c.last { v - c.last } else { v };
        c.last = v;
        Some(c.increase)
    }
}

/// One bucket of the queue-to-start histogram, followed as a counter.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct BucketCount {
    /// The upper bound in seconds; absent for `+Inf` (JSON cannot hold infinity).
    pub le: Option<f64>,
    /// The cumulative count, followed.
    pub count: Counter,
}

/// The ratios of the seven days before the deploy, which the error and latency gates compare
/// against (section 7). An absent baseline fails its gate.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct Baseline {
    /// Errors over requests.
    pub error_ratio: Option<f64>,
    /// Queue-to-start p99, seconds.
    pub queue_p99: Option<f64>,
}

/// Every counter the gates follow.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Counters {
    /// Farm-class failed actions.
    pub failed: Option<Counter>,
    /// Blob losses.
    pub losses: Option<Counter>,
    /// Execute errors.
    pub errors: Option<Counter>,
    /// Execute requests.
    pub requests: Option<Counter>,
    /// Failed write-tier flushes.
    pub flush_failures: Option<Counter>,
    /// Digest mismatches at a flush.
    pub digest_mismatches: Option<Counter>,
    /// The queue-to-start histogram.
    pub queued: Vec<BucketCount>,
}

/// The bake's state.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct State {
    /// [`STATE_SCHEMA`].
    pub schema: u32,
    /// The commit under test.
    pub commit: CommitId,
    /// The release set under test.
    pub set_hash: SetHash,
    /// The evaluator's own id for this bake.
    pub run_id: String,
    /// The generation the deploy set.
    pub desired_generation: Option<u64>,
    /// The deploy's span.
    pub deploy: Option<Window>,
    /// When the bake's clock started (unix seconds). Never reset by a restart.
    pub bake_start: u64,
    /// How long the bake lasts.
    pub bake_secs: u64,
    /// The baseline ratios.
    pub baseline: Baseline,
    /// The waivers an operator recorded.
    pub waivers: Vec<Waiver>,
    /// The time of the last scrape that was read.
    pub last_scrape: Option<u64>,
    /// Seconds nothing watched the farm (the evaluator was down, or scrapes failed): they
    /// do not count toward the bake, which extends by them.
    pub blind_secs: u64,
    /// The followed counters.
    pub counters: Counters,
    /// The hours (unix time over 3600) in which executes ran.
    pub load_hours: BTreeSet<u64>,
    /// Builds seen across the bake, as the last scrape counted them.
    pub builds: u64,
    /// Where the current stretch of under-replicated blobs outside a node step began.
    pub under_since: Option<u64>,
    /// The final status, once the bake has ended green or red. Extended is not final.
    pub ended: Option<Status>,
}

impl State {
    /// A state for a bake that starts at `now`.
    pub fn new(
        commit: CommitId,
        set_hash: SetHash,
        run_id: String,
        bake_secs: u64,
        now: u64,
    ) -> Self {
        Self {
            schema: STATE_SCHEMA,
            commit,
            set_hash,
            run_id,
            desired_generation: None,
            deploy: None,
            bake_start: now,
            bake_secs,
            baseline: Baseline::default(),
            waivers: Vec::new(),
            last_scrape: None,
            blind_secs: 0,
            counters: Counters::default(),
            load_hours: BTreeSet::new(),
            builds: 0,
            under_since: None,
            ended: None,
        }
    }

    /// Whether the minimum load is waived.
    pub fn min_load_waived(&self) -> bool {
        self.waivers.iter().any(|w| w.what == crate::WAIVE_MIN_LOAD)
    }

    /// Read the state at `path`; `None` when there is no file. A file that is not a state, or
    /// holds a waiver without a reason, is an error: a bake never starts over silently.
    pub fn load(path: &Path) -> Result<Option<Self>, StateError> {
        let name = path.display().to_string();
        let bytes = match std::fs::read(path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(source) => {
                return Err(StateError::Io {
                    op: "read",
                    path: name,
                    source,
                });
            }
        };
        let parse = |why: String| StateError::Parse {
            path: name.clone(),
            why,
        };
        let state: Self = serde_json::from_slice(&bytes).map_err(|e| parse(e.to_string()))?;
        if state.schema != STATE_SCHEMA {
            return Err(parse(format!(
                "schema {}, this reader knows {STATE_SCHEMA}",
                state.schema
            )));
        }
        for w in &state.waivers {
            Waiver::new(&w.what, &w.by, &w.reason).map_err(|e: DocError| parse(e.to_string()))?;
        }
        Ok(Some(state))
    }

    /// Write the state to `path`, atomically.
    pub fn save(&self, path: &Path) -> Result<(), StateError> {
        let mut bytes = serde_json::to_vec_pretty(self).map_err(|e| StateError::Parse {
            path: path.display().to_string(),
            why: e.to_string(),
        })?;
        bytes.push(b'\n');
        write_atomic(path, &bytes).map_err(|source| StateError::Io {
            op: "write",
            path: path.display().to_string(),
            source,
        })
    }
}
