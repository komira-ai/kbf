//! Fetch, execution and pack per where the phase times came from.

use serde_json::Value;

use crate::builds::Spread;
use crate::row::Row;

/// The phase durations of the rows of one provenance.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PhaseSet {
    /// Rows of this provenance.
    pub rows: u64,
    /// Input fetch percentiles.
    pub fetch: Spread,
    /// Execution percentiles.
    pub execution: Spread,
    /// Pack percentiles.
    pub pack: Spread,
}

/// One arm's phase numbers, never pooled across provenance.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PhaseStats {
    /// Rows a daemon measured, and rows that name no provenance.
    pub measured: PhaseSet,
    /// Rows whose phase times a task claimed.
    pub task_claim: PhaseSet,
    /// Rows naming a provenance this tool does not know.
    pub other: u64,
}

/// The phase numbers of `rows`.
pub fn analyze(_rows: &[Row]) -> PhaseStats {
    PhaseStats::default()
}

/// Appends the phase rows of the table.
pub(crate) fn rows(_out: &mut String, _a: &PhaseStats, _b: &PhaseStats) {}

/// One arm's phase numbers.
pub(crate) fn arm_json(_arm: &PhaseStats) -> Value {
    Value::Null
}

/// `b - a` of the phase percentiles.
pub(crate) fn difference_json(_a: &PhaseStats, _b: &PhaseStats) -> Value {
    Value::Null
}
