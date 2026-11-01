//! The per-build rows of the two-arm table and the per-build keys of its JSON.

use serde_json::Value;

use crate::builds::BuildStats;

/// Appends the per-build rows of the table.
pub(crate) fn rows(_out: &mut String, _a: &BuildStats, _b: &BuildStats) {}

/// One arm's per-build numbers.
pub(crate) fn arm_json(_arm: &BuildStats) -> Value {
    Value::Null
}

/// `b - a` of the per-build percentiles.
pub(crate) fn difference_json(_a: &BuildStats, _b: &BuildStats) -> Value {
    Value::Null
}
