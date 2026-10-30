//! Slowdown on busy hosts: how much slower each run was than the same work on a quiet host.
//! (Stub: the types and the signatures; the rules come after the tests are seen red.)

use crate::row::Row;

/// What the report cannot see. Printed with every report.
pub const LIMITS: [&str; 1] = ["stub"];

/// The knobs of the report.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Params {
    /// A run is quiet only if the node's CPU pressure at its start, in hundredths of a
    /// percent, is at most this.
    pub quiet_pressure_centi: u64,
    /// ... and at most this many other exported attempts overlapped it on its node.
    pub max_overlap: u64,
    /// The percentile (1 to 100, nearest rank) of a key's quiet walls taken as its baseline.
    pub baseline_percentile: u64,
}

impl Default for Params {
    fn default() -> Self {
        Self {
            quiet_pressure_centi: 500,
            max_overlap: 0,
            baseline_percentile: 10,
        }
    }
}

/// Whether the host was quiet when a run started.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Quiet {
    /// Pressure reading and overlap both within the knobs.
    Quiet,
    /// A reading above the threshold, or more overlap than allowed.
    Busy,
    /// No pressure reading was recorded.
    Unknown,
}

/// Where the rows went.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Funnel {
    /// Rows read.
    pub rows: u64,
    /// Not a successful run.
    pub not_ok: u64,
    /// No start, no end, or none apart.
    pub no_wall: u64,
    /// No shape key.
    pub no_shape: u64,
    /// Measured runs.
    pub measured: u64,
}

/// One measured run.
#[derive(Clone, Debug, PartialEq)]
pub struct RunResult {
    /// The run id.
    pub run_id: String,
    /// The key, as text.
    pub key: String,
    /// The node.
    pub node: String,
    /// When it started, microseconds after the epoch.
    pub started_us: u64,
    /// Wall time, microseconds.
    pub wall_us: u64,
    /// Host state at its start.
    pub quiet: Quiet,
    /// Its key's baseline, when the key has one.
    pub baseline_us: Option<u64>,
    /// `wall / baseline`.
    pub slowdown: Option<f64>,
}

/// Median, p90 and the count above 1.5x of a set of slowdowns.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Summary {
    /// Runs.
    pub runs: u64,
    /// Median (nearest rank).
    pub median: Option<f64>,
    /// 90th percentile (nearest rank).
    pub p90: Option<f64>,
    /// Runs strictly above 1.5x.
    pub above_1_5: u64,
}

impl Summary {
    /// The summary of `(wall, baseline)` pairs in microseconds.
    pub fn of(_pairs: &[(u64, u64)]) -> Self {
        Self::default()
    }
}

/// One key.
#[derive(Clone, Debug, PartialEq)]
pub struct KeyReport {
    /// The key as text.
    pub key: String,
    /// Measured runs of it.
    pub runs: u64,
    /// Of those, quiet ones.
    pub quiet_runs: u64,
    /// The baseline, when there is a quiet run.
    pub baseline_us: Option<u64>,
    /// The summary of its runs against the baseline.
    pub summary: Summary,
}

/// The report.
#[derive(Clone, Debug, PartialEq)]
pub struct Report {
    /// The knobs.
    pub params: Params,
    /// Where the rows went.
    pub funnel: Funnel,
    /// Measured runs with no pressure reading.
    pub unknown_pressure: u64,
    /// Keys, in key order.
    pub keys: Vec<KeyReport>,
    /// Runs, by start then id.
    pub runs: Vec<RunResult>,
    /// All scored runs.
    pub overall: Summary,
    /// Keys with no quiet run.
    pub keys_without_baseline: u64,
    /// Runs of those keys.
    pub runs_without_baseline: u64,
}

/// The report of `rows`.
pub fn analyze(_rows: &[Row], params: &Params) -> Report {
    Report {
        params: *params,
        funnel: Funnel::default(),
        unknown_pressure: 0,
        keys: Vec::new(),
        runs: Vec::new(),
        overall: Summary::default(),
        keys_without_baseline: 0,
        runs_without_baseline: 0,
    }
}

/// The report as text.
pub fn render_text(_report: &Report) -> String {
    String::new()
}

/// The report as one JSON document.
pub fn render_json(_report: &Report) -> String {
    String::new()
}
