//! One gate: a measurement against a limit, where a missing measurement is red.

use serde::{Deserialize, Serialize};

use crate::metrics::Sample;

/// A gate's outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Verdict {
    /// Within the limit.
    Green,
    /// Past a soft limit: reported, does not fail the stage.
    Warn,
    /// Past the limit, or not measured.
    Red,
}

/// What a gate allows.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Limit {
    /// Up to and including this value.
    AtMost(f64),
    /// This value and above.
    AtLeast(f64),
    /// Up to `warn` is green, above it up to `red` is a warning, above `red` is red.
    Band {
        /// The soft limit.
        warn: f64,
        /// The hard limit.
        red: f64,
    },
}

impl Limit {
    /// Whether every number in the limit is finite (JSON has no NaN to write it as).
    pub fn is_finite(&self) -> bool {
        match *self {
            Limit::AtMost(x) | Limit::AtLeast(x) => x.is_finite(),
            Limit::Band { warn, red } => warn.is_finite() && red.is_finite(),
        }
    }
}

/// The verdict for `sample` under `limit`. Equal to the limit passes, one step above fails,
/// and `Absent` is red under every limit.
pub fn judge(sample: Sample, limit: Limit) -> Verdict {
    let Sample::Value(v) = sample else {
        return Verdict::Red;
    };
    match limit {
        Limit::AtMost(max) if v <= max => Verdict::Green,
        Limit::AtLeast(min) if v >= min => Verdict::Green,
        Limit::Band { warn, .. } if v <= warn => Verdict::Green,
        Limit::Band { red, .. } if v <= red => Verdict::Warn,
        _ => Verdict::Red,
    }
}

/// One decided gate of a result document.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GateResult {
    /// The gate's name.
    pub name: String,
    /// What was measured; absent when the series was not there.
    pub measured: Option<f64>,
    /// The limit it was held to.
    pub threshold: Limit,
    /// The outcome.
    pub verdict: Verdict,
    /// When the measurement began (unix seconds).
    pub started: u64,
    /// When it was decided.
    pub finished: u64,
}

impl GateResult {
    /// Decide `sample` under `limit`.
    pub fn decide(name: &str, sample: Sample, limit: Limit, started: u64, finished: u64) -> Self {
        Self {
            name: name.to_owned(),
            measured: match sample {
                Sample::Value(v) if v.is_finite() => Some(v),
                Sample::Value(_) | Sample::Absent => None,
            },
            threshold: limit,
            verdict: judge(sample, limit),
            started,
            finished,
        }
    }
}
