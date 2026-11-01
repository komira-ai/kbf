//! Slowdown on busy hosts: how much slower each run took than the same work on a quiet host.
//!
//! # Definition
//!
//! * **Measured run**: an attempt of the export whose outcome is `ok`, that has a start and
//!   an end apart, and a shape key. Its **wall** is `ended - started` of the record. Rows
//!   that are not measured are counted by reason ([`Funnel`]).
//! * **Key**: an approximation of the estimator's key (`kbf_estimator::Key`): instance,
//!   platform class, basis, shape and configuration. The server's key carries the OS and the
//!   CPU architecture in its platform class; the export's platform is the OS alone
//!   (`linux`, `macos`, `-`), read from the node's state at export time. A row whose node
//!   is no longer listed has platform `-` and forms its own key here; the server skips such
//!   an attempt. The basis is the row's tag (`mac_footprint`), else `cgroup`.
//! * **Quiet**: the node's CPU pressure at the lease start (`some avg10`, hundredths of a
//!   percent) is recorded and at most `quiet_pressure_centi` (default 500 = 5.00%), and at
//!   most `max_overlap` (default 0) other attempts of the export, whatever their outcome,
//!   overlapped the run on its node (touching intervals do not overlap). A run with no
//!   pressure reading is [`Quiet::Unknown`]: never quiet, never imputed.
//! * **Baseline**: the `baseline_percentile` (default 10, nearest rank) of the walls of the
//!   key's quiet runs. A low percentile and not the minimum, so that one run that was fast
//!   for a reason the key does not hold (a warm cache, a cut-short run) does not set it;
//!   with fewer than ten quiet runs nearest rank at 10 is the minimum.
//! * **Slowdown**: `wall / baseline`, for every measured run of a key that has a baseline
//!   (below 1.0 is possible). A key with no quiet run has no baseline: its runs are counted
//!   in [`Report::runs_without_baseline`] and carry no slowdown.
//! * **Summaries** (per key and overall): median and p90 by nearest rank, and the count of
//!   runs strictly above 1.5x.

use std::collections::BTreeMap;

use kbf_estimator::{Basis, Cfg, Instance, Key, PlatformClass, Shape};
use serde_json::json;

use crate::row::Row;

/// The default quiet threshold, in hundredths of a percent.
pub const DEFAULT_QUIET_PRESSURE_CENTI: u64 = 500;
/// The default number of other attempts allowed to overlap a quiet run.
pub const DEFAULT_MAX_OVERLAP: u64 = 0;
/// The default baseline percentile.
pub const DEFAULT_BASELINE_PERCENTILE: u64 = 10;

/// What the report cannot see. Printed with every report.
pub const LIMITS: [&str; 8] = [
    "Quiet is judged at the lease start only: the node's CPU pressure then (avg10) and the exported attempts that overlapped the run on its node. The record has no count of concurrent leases and no reading during the run, so a run that started quiet and got busy is counted quiet.",
    "Overlap counts only attempts in the export; attempts the history does not hold (no invocation id, aged out of retention) are invisible, so it is a lower bound.",
    "A run with no pressure reading (a server or daemon that predates it) is neither quiet nor busy; it can be slowed against a baseline, never be one.",
    "The platform class is the OS alone: no CPU architecture, no node speed. Runs of one key on different hardware differ for that reason too, and the report counts it as slowdown.",
    "Wall is the record's start to end of the attempt, whatever it covers (fetch, run, upload); only successful runs are measured.",
    "A key with no quiet run has no baseline; its runs are counted, not imputed.",
    "Attempts with no end (lost, or still in flight at the export) are invisible to the overlap count, though they took the node's CPU.",
    "Rows with an empty node name share one bucket for the overlap count.",
];

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
            quiet_pressure_centi: DEFAULT_QUIET_PRESSURE_CENTI,
            max_overlap: DEFAULT_MAX_OVERLAP,
            baseline_percentile: DEFAULT_BASELINE_PERCENTILE,
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
    pub fn of(pairs: &[(u64, u64)]) -> Self {
        let mut ratios: Vec<f64> = pairs.iter().map(|&(w, b)| w as f64 / b as f64).collect();
        ratios.sort_by(f64::total_cmp);
        Self {
            runs: pairs.len() as u64,
            median: nearest(&ratios, 50),
            p90: nearest(&ratios, 90),
            // `wall / baseline > 3/2` without division, so 150 s over 100 s is on the line.
            above_1_5: pairs
                .iter()
                .filter(|&&(w, b)| u128::from(w) * 2 > u128::from(b) * 3)
                .count() as u64,
        }
    }
}

/// The value at percentile `p` (1 to 100) of `sorted`, by nearest rank.
fn nearest<T: Copy>(sorted: &[T], p: u64) -> Option<T> {
    let rank = (p * sorted.len() as u64).div_ceil(100).max(1);
    sorted.get(usize::try_from(rank - 1).ok()?).copied()
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

/// The attempts of an export by node, as sorted starts and sorted ends, to count how many
/// overlap a given one.
struct Load<'a>(BTreeMap<&'a str, (Vec<u64>, Vec<u64>)>);

impl<'a> Load<'a> {
    fn of(rows: &'a [Row]) -> Self {
        let mut by_node: BTreeMap<&str, (Vec<u64>, Vec<u64>)> = BTreeMap::new();
        for row in rows {
            if let Some((start, end)) = span(row) {
                let (starts, ends) = by_node.entry(row.node.as_str()).or_default();
                starts.push(start);
                ends.push(end);
            }
        }
        for (starts, ends) in by_node.values_mut() {
            starts.sort_unstable();
            ends.sort_unstable();
        }
        Self(by_node)
    }

    /// The other attempts of `node` with a start before `end` and an end after `start`:
    /// those that started before `end`, less those already over by `start` (which started
    /// before they ended, so before `end` too), less the attempt itself, which `node`
    /// holds because it has the span.
    fn overlap(&self, node: &str, start: u64, end: u64) -> u64 {
        let (starts, ends) = &self.0[node];
        let began = starts.partition_point(|s| *s < end);
        let over = ends.partition_point(|e| *e <= start);
        (began - over).saturating_sub(1) as u64
    }
}

/// The start and end of an attempt that has a duration.
fn span(row: &Row) -> Option<(u64, u64)> {
    row.started_us
        .zip(row.ended_us)
        .filter(|(start, end)| start < end)
}

/// The approximate estimator key of a row and the text that names it: instance, OS-only
/// platform class, basis, shape and configuration.
fn key_of(row: &Row) -> Option<(Key, String)> {
    let shape = u64::from_str_radix(&row.shape, 16)
        .ok()
        .filter(|s| *s != 0)?;
    let keyed_cfg = row.cfg != "-" && !row.cfg.is_empty();
    let cfg = keyed_cfg.then(|| Cfg(kbf_server::learned::cfg_hash(&row.cfg)));
    let (basis, basis_name) = if row.basis.as_deref() == Some("mac_footprint") {
        (Basis::MacFootprint, "mac_footprint")
    } else {
        (Basis::Cgroup, "cgroup")
    };
    let label = format!(
        "{}/{}/{basis_name}/{:016x}/{}",
        row.instance,
        row.platform,
        shape,
        if keyed_cfg { row.cfg.as_str() } else { "-" }
    );
    let key = Key::new(
        Instance::new(row.instance.clone()),
        PlatformClass::new(row.platform.clone()),
        basis,
        Shape(shape),
        cfg,
    );
    Some((key, label))
}

/// A measured run, before its key has a baseline.
struct Measured<'a> {
    row: &'a Row,
    label: String,
    start: u64,
    wall: u64,
    quiet: Quiet,
}

/// The report of `rows`.
pub fn analyze(rows: &[Row], params: &Params) -> Report {
    let load = Load::of(rows);
    let mut funnel = Funnel {
        rows: rows.len() as u64,
        ..Funnel::default()
    };
    let mut groups: BTreeMap<Key, Vec<Measured>> = BTreeMap::new();
    for row in rows {
        if row.outcome != "ok" {
            funnel.not_ok += 1;
            continue;
        }
        let Some((start, end)) = span(row) else {
            funnel.no_wall += 1;
            continue;
        };
        let Some((key, label)) = key_of(row) else {
            funnel.no_shape += 1;
            continue;
        };
        let quiet = match row.node_cpu_pressure_centi {
            None => Quiet::Unknown,
            Some(p)
                if p <= params.quiet_pressure_centi
                    && load.overlap(&row.node, start, end) <= params.max_overlap =>
            {
                Quiet::Quiet
            }
            Some(_) => Quiet::Busy,
        };
        funnel.measured += 1;
        groups.entry(key).or_default().push(Measured {
            row,
            label,
            start,
            wall: end - start,
            quiet,
        });
    }
    let mut report = Report {
        params: *params,
        funnel,
        unknown_pressure: 0,
        keys: Vec::new(),
        runs: Vec::new(),
        overall: Summary::default(),
        keys_without_baseline: 0,
        runs_without_baseline: 0,
    };
    let mut scored = Vec::new();
    for group in groups.into_values() {
        let mut quiet_walls: Vec<u64> = group
            .iter()
            .filter(|m| m.quiet == Quiet::Quiet)
            .map(|m| m.wall)
            .collect();
        quiet_walls.sort_unstable();
        let baseline = nearest(&quiet_walls, params.baseline_percentile);
        let pairs: Vec<(u64, u64)> = baseline
            .into_iter()
            .flat_map(|b| group.iter().map(move |m| (m.wall, b)))
            .collect();
        report.keys_without_baseline += u64::from(baseline.is_none());
        report.runs_without_baseline += if baseline.is_none() {
            group.len() as u64
        } else {
            0
        };
        report.unknown_pressure +=
            group.iter().filter(|m| m.quiet == Quiet::Unknown).count() as u64;
        report.keys.push(KeyReport {
            key: group[0].label.clone(),
            runs: group.len() as u64,
            quiet_runs: quiet_walls.len() as u64,
            baseline_us: baseline,
            summary: Summary::of(&pairs),
        });
        scored.extend(pairs);
        report.runs.extend(group.iter().map(|m| RunResult {
            run_id: m.row.run_id.clone(),
            key: m.label.clone(),
            node: m.row.node.clone(),
            started_us: m.start,
            wall_us: m.wall,
            quiet: m.quiet,
            baseline_us: baseline,
            slowdown: baseline.map(|b| m.wall as f64 / b as f64),
        }));
    }
    report.overall = Summary::of(&scored);
    report.keys.sort_by(|a, b| a.key.cmp(&b.key));
    report
        .runs
        .sort_by(|a, b| (a.started_us, &a.run_id).cmp(&(b.started_us, &b.run_id)));
    report
}

impl Quiet {
    /// The state's name in a report.
    pub fn name(self) -> &'static str {
        match self {
            Self::Quiet => "quiet",
            Self::Busy => "busy",
            Self::Unknown => "unknown",
        }
    }
}

fn times(v: Option<f64>) -> String {
    match v {
        Some(v) => format!("{v:.2}x"),
        None => "-".to_owned(),
    }
}

fn share(hits: u64, of: u64) -> String {
    match of {
        0 => "-".to_owned(),
        _ => format!("{:.1}%", 100.0 * hits as f64 / of as f64),
    }
}

fn millis(us: Option<u64>) -> String {
    match us {
        Some(us) => format!("{} ms", us / 1_000),
        None => "none".to_owned(),
    }
}

fn summary_line(s: &Summary) -> String {
    format!(
        "median {}, p90 {}, above 1.5x: {}/{} = {}",
        times(s.median),
        times(s.p90),
        s.above_1_5,
        s.runs,
        share(s.above_1_5, s.runs)
    )
}

/// The report as text: the definition, the funnel, the overall summary, one line per key.
/// The runs are in the JSON form.
pub fn render_text(report: &Report) -> String {
    use std::fmt::Write;
    let (p, f) = (&report.params, &report.funnel);
    let mut out = String::new();
    let mut line = |text: String| {
        let _ = writeln!(out, "{text}");
    };
    line("kbf estimator replay: slowdown on busy hosts".to_owned());
    line(format!(
        "quiet: node CPU pressure at lease start <= {:.2}% and at most {} other exported attempts overlapping on the node",
        p.quiet_pressure_centi as f64 / 100.0,
        p.max_overlap
    ));
    line(format!(
        "baseline: percentile {} (nearest rank) of the wall of a key's quiet runs; slowdown = wall / baseline",
        p.baseline_percentile
    ));
    line(format!(
        "rows: {}; not ok {}; no start/end apart {}; no shape key {}; measured {}",
        f.rows, f.not_ok, f.no_wall, f.no_shape, f.measured
    ));
    line(format!(
        "measured runs with no pressure reading: {}",
        report.unknown_pressure
    ));
    line(format!(
        "keys: {} with a quiet baseline, {} without; {} runs without a baseline (not imputed)",
        report.keys.len() as u64 - report.keys_without_baseline,
        report.keys_without_baseline,
        report.runs_without_baseline
    ));
    line(format!(
        "overall ({} runs): {}",
        report.overall.runs,
        summary_line(&report.overall)
    ));
    line("per key (instance/platform/basis/shape/cfg):".to_owned());
    for k in &report.keys {
        line(format!(
            "  {}  runs {}  quiet {}  baseline {}  {}",
            k.key,
            k.runs,
            k.quiet_runs,
            millis(k.baseline_us),
            summary_line(&k.summary)
        ));
    }
    line("limits:".to_owned());
    for limit in LIMITS {
        line(format!("  - {limit}"));
    }
    out
}

fn summary_json(s: &Summary) -> serde_json::Value {
    json!({"runs": s.runs, "median": s.median, "p90": s.p90, "above_1_5": s.above_1_5})
}

/// The report as one JSON document, with every run.
pub fn render_json(report: &Report) -> String {
    let (p, f) = (&report.params, &report.funnel);
    let keys: Vec<_> = report
        .keys
        .iter()
        .map(|k| {
            json!({
                "key": k.key, "runs": k.runs, "quiet_runs": k.quiet_runs,
                "baseline_us": k.baseline_us, "summary": summary_json(&k.summary),
            })
        })
        .collect();
    let runs: Vec<_> = report
        .runs
        .iter()
        .map(|r| {
            json!({
                "run_id": r.run_id, "key": r.key, "node": r.node,
                "started_us": r.started_us, "wall_us": r.wall_us,
                "host": r.quiet.name(), "baseline_us": r.baseline_us, "slowdown": r.slowdown,
            })
        })
        .collect();
    let doc = json!({
        "params": {
            "quiet_pressure_centi": p.quiet_pressure_centi,
            "max_overlap": p.max_overlap,
            "baseline_percentile": p.baseline_percentile,
        },
        "funnel": {
            "rows": f.rows, "not_ok": f.not_ok, "no_wall": f.no_wall,
            "no_shape": f.no_shape, "measured": f.measured,
        },
        "unknown_pressure": report.unknown_pressure,
        "keys_without_baseline": report.keys_without_baseline,
        "runs_without_baseline": report.runs_without_baseline,
        "overall": summary_json(&report.overall),
        "keys": keys,
        "runs": runs,
    });
    // A value built from strings, integers and floats that are finite always serializes.
    serde_json::to_string_pretty(&doc).unwrap_or_default()
}
