//! The updater's phase log, as the evaluator reads it: when a node step was running and
//! which processes restarted. The log is an input; the updater that writes it is not part of
//! this crate. The format is this crate's reading of what the evaluator needs (a JSON
//! document), and it is `deny_unknown_fields` so a writer that grows a field is noticed.

use serde::Deserialize;

/// Why a log is refused.
#[derive(Debug, thiserror::Error)]
#[error("updater log: {0}")]
pub struct LogError(#[from] serde_json::Error);

/// A node step: from `start` to `end` (absent while it runs), unix seconds.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Step {
    /// The host stepped.
    pub host: String,
    /// When the step began.
    pub start: u64,
    /// When it ended; absent while it runs.
    pub end: Option<u64>,
}

/// A process restart the updater made.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Restart {
    /// The host.
    pub host: String,
    /// When.
    pub at: u64,
}

/// The phase log: node steps and restarts.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdaterLog {
    /// Every node step.
    pub steps: Vec<Step>,
    /// Every restart.
    pub restarts: Vec<Restart>,
}

impl UpdaterLog {
    /// Read a log.
    pub fn parse(bytes: &[u8]) -> Result<Self, LogError> {
        Ok(serde_json::from_slice(bytes)?)
    }

    /// Whether a node step is running at `at` (both edges count as inside).
    pub fn in_step(&self, at: u64) -> bool {
        self.steps
            .iter()
            .any(|s| s.start <= at && s.end.is_none_or(|e| at <= e))
    }

    /// The restarts the updater made from `start` to `until`, both included: the bake follows
    /// the deploy, so a restart in it is one the deploy did not make.
    pub fn restarts_in(&self, start: u64, until: u64) -> u64 {
        let n = self
            .restarts
            .iter()
            .filter(|r| r.at >= start && r.at <= until)
            .count();
        u64::try_from(n).unwrap_or(u64::MAX)
    }
}
