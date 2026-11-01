//! `fill-paper`: writes the measured numbers into the paper's HTML.
//!
//! The paper marks each figure it quotes with an element carrying `data-cell="<id>"`. A mapping
//! file says what each id stands for (a cell, an arm, a metric, a unit); the result directory
//! holds the numbers. [`fill`] replaces the content of each marked element with the formatted
//! value and records where it came from in `data-source="<file>#<metric>"` and `data-label`
//! (`test instance` or `farm`). Everything outside the replaced element contents and the
//! attributes this module owns (`data-source`, `data-label`, `data-result`, `data-note`, and
//! `tbd` in `class`) is copied byte for byte:
//! the HTML is scanned, never re-serialised.
//!
//! Each filled value has a row in the result table ([`ResultTsv`]), whose name `data-result`
//! carries: kind `derived`, id `fill:<cell>:<metric>`, the number as printed, its unit, how it was
//! taken and the cell. A cell whose `data-result` is a table this run does not write is refused.
//!
//! The mapping, a JSON array of
//! `{id, cell, metric, unit, condition, size, blocked}` (`size` and `blocked` optional, `label`
//! optional):
//! - `cell` is a cell id (`cold-put`, ...) and `condition` the arm. With a `size`, the arm
//!   `<condition>-<size>` is tried first, then `<condition>`.
//! - `metric` is one of `p50`, `p95`, `p99`, `max`, `requests`, `failures`, `bytes`,
//!   `throughput`, `elapsed`; `a|b` is "a, else b" (`p99|p95`).
//! - `unit` suits the metric: `us`, `ms` or `s` for the latencies and `elapsed`; `B`, `KB`, `MB`,
//!   `GB`, `KiB`, `MiB`, `GiB` for `bytes`; the same with `/s` for `throughput`; `count` for
//!   `requests` and `failures`.
//! - `blocked` leaves the element alone (so does a `data-blocked` attribute on it).
//! - `label` overrides the data label; without it an arm naming `instance` is `test instance`
//!   and any other arm is `farm`.
//!
//! The figures: the passes that count are those that are not the warm-up, were not aborted by
//! the load gate or the cold store guard, and were not taken while the cold store was healing
//! (the reading before the pass says so). A result file is one attempt
//! (`<cell>-<arm>-a<N>.json`): when an arm has several, the latest attempt that gives a figure
//! is used, and `data-source` names its file. `p50`, `p95` and `max` take the median of the per-pass value (`max`: the
//! largest). `p99` takes the median of the quiet passes, and only when those passes made 1,000
//! requests between them; otherwise the metric is unavailable, the next alternative is tried,
//! and with none left the element keeps its text and gets a `data-note` saying why.
//! `throughput` is bytes over wall time of the passes that count.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use clap::Args;
use serde::Deserialize;

use crate::cells::CellId;
use crate::cells::engine::result_name;
use crate::result::{CellResult, PassResult, Window};

/// Requests the quiet passes must have made together before a p99 is quoted.
const P99_MIN_REQUESTS: u64 = 1_000;

/// Why a fill or a check stopped or failed.
#[derive(Debug, thiserror::Error)]
pub enum FillError {
    /// A file could not be read or written.
    #[error("{path}: {source}")]
    Io {
        /// The file.
        path: PathBuf,
        /// What the operating system said.
        #[source]
        source: std::io::Error,
    },
    /// The mapping file is not a valid mapping.
    #[error("mapping: {0}")]
    Mapping(String),
    /// A result file of the directory is not a result.
    #[error("{file}: {reason}")]
    Result {
        /// The file name.
        file: String,
        /// What is wrong with it.
        reason: String,
    },
    /// The paper's HTML cannot be scanned or an element cannot be filled.
    #[error("paper: {0}")]
    Html(String),
    /// The flags contradict each other.
    #[error("{0}")]
    Usage(&'static str),
    /// `--check` found problems; the text lists them.
    #[error("check failed:\n{0}")]
    Check(String),
    /// A line of the result table is not a row.
    #[error("result table: {0}")]
    Table(String),
    /// The name of the result table cannot be a `data-result`.
    #[error(
        "result table name {0:?}: lower-case letters, digits, `.`, `-` and `_`, ending in .tsv"
    )]
    TsvName(String),
    /// A cell names a result table this run did not write.
    #[error(
        "cell {cell:?} names the result table {found:?}, which this run did not write (it writes {ours:?}); a hand-filled cell is not rewritten"
    )]
    Foreign {
        /// The cell.
        cell: String,
        /// Its `data-result`.
        found: String,
        /// The table this run writes.
        ours: String,
    },
}

/// Flags of `fill-paper`.
#[derive(Debug, Args)]
pub struct FillArgs {
    /// The paper's HTML.
    #[arg(long, value_name = "FILE")]
    pub paper: PathBuf,
    /// The mapping from placeholder ids to cells and metrics (JSON).
    #[arg(long, value_name = "FILE")]
    pub cells: PathBuf,
    /// The directory of `<cell>-<arm>-a<N>.json` results.
    #[arg(long, value_name = "DIR")]
    pub result_dir: PathBuf,
    /// Where to write the filled paper.
    #[arg(long, value_name = "FILE")]
    pub out: Option<PathBuf>,
    /// The table of rows (kind, id, value, unit, how, cells) that backs every filled value;
    /// created or updated, and named in each filled cell's `data-result`.
    #[arg(long, value_name = "FILE")]
    pub result_tsv: Option<PathBuf>,
    /// Write over the paper instead of to `--out`.
    #[arg(long)]
    pub in_place: bool,
    /// Write nothing; exit non-zero listing placeholders without a mapping, mappings without a
    /// result, and filled values whose source no longer exists.
    #[arg(long)]
    pub check: bool,
}

/// Runs `fill-paper`.
///
/// # Errors
/// [`FillError`]: a file that cannot be read or written, a bad mapping, result or paper, a
/// contradictory set of flags, or (with `--check`) the list of problems found.
pub fn run(args: &FillArgs) -> Result<(), FillError> {
    let html = read(&args.paper)?;
    let mappings = Mapping::parse_all(&read(&args.cells)?)?;
    let results = Results::load(&args.result_dir)?;
    if args.check {
        if args.out.is_some() || args.in_place {
            return Err(FillError::Usage(
                "--check writes nothing: no --out or --in-place",
            ));
        }
        let table = match &args.result_tsv {
            Some(path) => Some(ResultTsv::parse(&table_name(path), &read(path)?)?),
            None => None,
        };
        let findings = check(&html, &mappings, &results, table.as_ref())?;
        return if findings.is_empty() {
            println!("fill-paper: check passed");
            Ok(())
        } else {
            Err(FillError::Check(findings.join("\n")))
        };
    }
    let target = match (&args.out, args.in_place) {
        (Some(_), true) => return Err(FillError::Usage("--out and --in-place exclude each other")),
        (None, false) => return Err(FillError::Usage("give --out FILE, or --in-place")),
        (Some(out), false) => out,
        (None, true) => &args.paper,
    };
    let Some(table_path) = &args.result_tsv else {
        return Err(FillError::Usage(
            "give --result-tsv FILE, the table of rows behind the numbers",
        ));
    };
    let name = table_name(table_path);
    let mut table = match std::fs::read_to_string(table_path) {
        Ok(text) => ResultTsv::parse(&name, &text)?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => ResultTsv::new(&name)?,
        Err(source) => {
            return Err(FillError::Io {
                path: table_path.clone(),
                source,
            });
        }
    };
    let (text, report) = fill(&html, &mappings, &results, &mut table)?;
    std::fs::write(table_path, table.render()).map_err(|source| FillError::Io {
        path: table_path.clone(),
        source,
    })?;
    std::fs::write(target, text).map_err(|source| FillError::Io {
        path: target.clone(),
        source,
    })?;
    println!(
        "fill-paper: {} filled, {} blocked, {} left with a note, {} without a mapping",
        report.filled, report.blocked, report.noted, report.unmapped
    );
    Ok(())
}

/// Whether `name` is a plain table name for a `data-result`: lower-case letters, digits, `.`,
/// `-` and `_`, ending in `.tsv`, no `..` (the rule of the paper's completion check).
fn tsv_name_ok(name: &str) -> bool {
    name.len() > ".tsv".len()
        && name.ends_with(".tsv")
        && !name.contains("..")
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || "._-".contains(c))
}

/// The comment lines a new table starts with.
const TSV_HEADER: [&str; 2] = [
    "# Rows behind the numbers tierstore-bench fill-paper printed. Columns: kind, id, value, unit, how the value was taken, cells.",
    "# A row with an id of the form fill:<cell>:<metric> is rewritten by the next run; every other line is left as it is.",
];

/// The result table: the rows that back the numbers `fill-paper` prints. A row is
/// `kind`, `id`, `value`, `unit`, `how` and `cells`, tab-separated; `cells` names the cell that
/// prints the value. `fill-paper` owns the rows whose id is `fill:<cell>:<metric>`: it updates
/// them in place or appends them; comments and other rows are kept as they are.
#[derive(Clone, Debug)]
pub struct ResultTsv {
    name: String,
    lines: Vec<String>,
}

impl ResultTsv {
    /// An empty table called `name`, with a header.
    ///
    /// # Errors
    /// [`FillError::TsvName`] if `name` cannot be a `data-result`.
    pub fn new(name: &str) -> Result<ResultTsv, FillError> {
        if !tsv_name_ok(name) {
            return Err(FillError::TsvName(name.to_owned()));
        }
        Ok(ResultTsv {
            name: name.to_owned(),
            lines: TSV_HEADER.iter().map(|l| (*l).to_owned()).collect(),
        })
    }

    /// The table `name` as it is on disk.
    ///
    /// # Errors
    /// [`FillError::TsvName`] if `name` cannot be a `data-result`; [`FillError::Table`] if a
    /// line that is not a comment has fewer than three columns or a value that is not a number.
    pub fn parse(name: &str, text: &str) -> Result<ResultTsv, FillError> {
        let mut table = ResultTsv::new(name)?;
        table.lines.clear();
        for (n, line) in text.lines().enumerate() {
            if !(line.is_empty() || line.starts_with('#')) {
                let columns: Vec<&str> = line.split('\t').collect();
                if columns.len() < 3 {
                    return Err(FillError::Table(format!(
                        "line {}: fewer than 3 tab-separated columns",
                        n + 1
                    )));
                }
                if columns[2].parse::<f64>().is_err() {
                    return Err(FillError::Table(format!(
                        "line {}: value {:?} is not a number",
                        n + 1,
                        columns[2]
                    )));
                }
            }
            table.lines.push(line.to_owned());
        }
        Ok(table)
    }

    /// The file name, as `data-result` writes it.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The file text: every line ends in a newline.
    #[must_use]
    pub fn render(&self) -> String {
        self.lines.iter().map(|l| format!("{l}\n")).collect()
    }

    /// Replaces the row with the id of `row` in place, or appends it.
    fn upsert(&mut self, row: String) {
        let id = row.split('\t').nth(1).unwrap_or_default().to_owned();
        let at = self
            .lines
            .iter()
            .position(|l| !l.starts_with('#') && l.split('\t').nth(1) == Some(id.as_str()));
        match at {
            Some(k) => self.lines[k] = row,
            None => self.lines.push(row),
        }
    }

    /// Whether a row names `cell` in its last column.
    fn names(&self, cell: &str) -> bool {
        self.lines.iter().any(|l| {
            !l.starts_with('#')
                && l.split('\t')
                    .nth(5)
                    .is_some_and(|cells| cells.split(',').any(|c| c.trim() == cell))
        })
    }
}

/// The file name of the table at `path`, as `data-result` writes it.
fn table_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn read(path: &Path) -> Result<String, FillError> {
    std::fs::read_to_string(path).map_err(|source| FillError::Io {
        path: path.to_owned(),
        source,
    })
}

// ---------------------------------------------------------------------------------------------
// The mapping

/// A size in the mapping: a label (`4MiB`) or a byte count.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(untagged)]
pub enum Size {
    /// As the arm names it.
    Text(String),
    /// Bytes.
    Bytes(u64),
}

impl Size {
    fn as_arm_suffix(&self) -> String {
        match self {
            Size::Text(t) => t.clone(),
            Size::Bytes(b) => b.to_string(),
        }
    }
}

/// One row of the mapping file, as written.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct MappingRow {
    id: String,
    cell: CellId,
    metric: String,
    unit: String,
    condition: String,
    #[serde(default)]
    size: Option<Size>,
    #[serde(default)]
    blocked: bool,
    #[serde(default)]
    label: Option<String>,
}

/// A figure of a result.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Metric {
    /// Median latency.
    P50,
    /// 95th percentile latency.
    P95,
    /// 99th percentile latency (quiet passes, 1,000 requests).
    P99,
    /// Slowest request.
    Max,
    /// Requests made.
    Requests,
    /// Requests that failed.
    Failures,
    /// Bytes moved.
    Bytes,
    /// Bytes per second.
    Throughput,
    /// Wall time of a pass.
    Elapsed,
}

impl Metric {
    const ALL: [(&'static str, Metric); 9] = [
        ("p50", Metric::P50),
        ("p95", Metric::P95),
        ("p99", Metric::P99),
        ("max", Metric::Max),
        ("requests", Metric::Requests),
        ("failures", Metric::Failures),
        ("bytes", Metric::Bytes),
        ("throughput", Metric::Throughput),
        ("elapsed", Metric::Elapsed),
    ];

    fn parse(name: &str) -> Option<Metric> {
        Self::ALL.iter().find(|(n, _)| *n == name).map(|(_, m)| *m)
    }

    fn name(self) -> &'static str {
        Self::ALL
            .iter()
            .find(|(_, m)| *m == self)
            .map_or("", |(n, _)| n)
    }
}

/// A unit and what it divides the base value (microseconds, bytes, bytes per second, a count)
/// by.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Unit {
    name: &'static str,
    divisor: f64,
    /// Printed without decimals whatever the value (the base unit of microseconds, bytes,
    /// counts).
    integral: bool,
    /// The suffix after the number, with its space; empty for counts.
    suffix: &'static str,
}

const TIME_UNITS: [Unit; 3] = [
    Unit {
        name: "us",
        divisor: 1.0,
        integral: true,
        suffix: " us",
    },
    Unit {
        name: "ms",
        divisor: 1_000.0,
        integral: false,
        suffix: " ms",
    },
    Unit {
        name: "s",
        divisor: 1_000_000.0,
        integral: false,
        suffix: " s",
    },
];
const SIZE_UNITS: [Unit; 7] = [
    Unit {
        name: "B",
        divisor: 1.0,
        integral: true,
        suffix: " B",
    },
    Unit {
        name: "KB",
        divisor: 1e3,
        integral: false,
        suffix: " KB",
    },
    Unit {
        name: "MB",
        divisor: 1e6,
        integral: false,
        suffix: " MB",
    },
    Unit {
        name: "GB",
        divisor: 1e9,
        integral: false,
        suffix: " GB",
    },
    Unit {
        name: "KiB",
        divisor: 1024.0,
        integral: false,
        suffix: " KiB",
    },
    Unit {
        name: "MiB",
        divisor: 1_048_576.0,
        integral: false,
        suffix: " MiB",
    },
    Unit {
        name: "GiB",
        divisor: 1_073_741_824.0,
        integral: false,
        suffix: " GiB",
    },
];
const RATE_UNITS: [Unit; 7] = [
    Unit {
        name: "B/s",
        divisor: 1.0,
        integral: true,
        suffix: " B/s",
    },
    Unit {
        name: "KB/s",
        divisor: 1e3,
        integral: false,
        suffix: " KB/s",
    },
    Unit {
        name: "MB/s",
        divisor: 1e6,
        integral: false,
        suffix: " MB/s",
    },
    Unit {
        name: "GB/s",
        divisor: 1e9,
        integral: false,
        suffix: " GB/s",
    },
    Unit {
        name: "KiB/s",
        divisor: 1024.0,
        integral: false,
        suffix: " KiB/s",
    },
    Unit {
        name: "MiB/s",
        divisor: 1_048_576.0,
        integral: false,
        suffix: " MiB/s",
    },
    Unit {
        name: "GiB/s",
        divisor: 1_073_741_824.0,
        integral: false,
        suffix: " GiB/s",
    },
];
const COUNT_UNITS: [Unit; 1] = [Unit {
    name: "count",
    divisor: 1.0,
    integral: true,
    suffix: "",
}];

fn units_of(metric: Metric) -> &'static [Unit] {
    match metric {
        Metric::P50 | Metric::P95 | Metric::P99 | Metric::Max | Metric::Elapsed => &TIME_UNITS,
        Metric::Bytes => &SIZE_UNITS,
        Metric::Throughput => &RATE_UNITS,
        Metric::Requests | Metric::Failures => &COUNT_UNITS,
    }
}

/// A validated mapping row.
#[derive(Clone, Debug)]
pub struct Mapping {
    id: String,
    cell: CellId,
    /// The alternatives, in order, each with its unit.
    metrics: Vec<(Metric, Unit)>,
    condition: String,
    size: Option<Size>,
    blocked: bool,
    label: Option<String>,
}

impl Mapping {
    /// Parses and validates the mapping file.
    ///
    /// # Errors
    /// [`FillError::Mapping`]: not JSON of the schema, a repeated id, an unknown metric, or a
    /// unit that does not suit the metric.
    pub fn parse_all(text: &str) -> Result<Vec<Mapping>, FillError> {
        let rows: Vec<MappingRow> =
            serde_json::from_str(text).map_err(|e| FillError::Mapping(e.to_string()))?;
        let mut seen = BTreeSet::new();
        let mut out = Vec::with_capacity(rows.len());
        for row in rows {
            if !seen.insert(row.id.clone()) {
                return Err(FillError::Mapping(format!("id {:?} is repeated", row.id)));
            }
            out.push(Mapping::from_row(row)?);
        }
        Ok(out)
    }

    fn from_row(row: MappingRow) -> Result<Mapping, FillError> {
        let mut metrics = Vec::new();
        for name in row.metric.split('|') {
            let metric = Metric::parse(name).ok_or_else(|| {
                FillError::Mapping(format!("{}: unknown metric {name:?}", row.id))
            })?;
            let unit = units_of(metric)
                .iter()
                .find(|u| u.name == row.unit)
                .ok_or_else(|| {
                    FillError::Mapping(format!(
                        "{}: unit {:?} does not suit metric {name}",
                        row.id, row.unit
                    ))
                })?;
            metrics.push((metric, *unit));
        }
        Ok(Mapping {
            id: row.id,
            cell: row.cell,
            metrics,
            condition: row.condition,
            size: row.size,
            blocked: row.blocked,
            label: row.label,
        })
    }

    fn arms(&self) -> Vec<String> {
        let mut arms = Vec::new();
        if let Some(size) = &self.size {
            arms.push(format!("{}-{}", self.condition, size.as_arm_suffix()));
        }
        arms.push(self.condition.clone());
        arms
    }

    fn label(&self, arm: &str) -> String {
        match &self.label {
            Some(l) => l.clone(),
            None if arm.contains("instance") => "test instance".to_owned(),
            None => "farm".to_owned(),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// The results

/// The results of a directory, by file name.
#[derive(Debug, Default)]
pub struct Results {
    files: BTreeMap<String, CellResult>,
}

impl Results {
    /// Reads every `<cell>-<arm>-a<N>.json` of `dir`; other files are not results and are skipped.
    ///
    /// # Errors
    /// [`FillError::Io`] if the directory or a result cannot be read; [`FillError::Result`] if
    /// a file named like a result is not one.
    pub fn load(dir: &Path) -> Result<Results, FillError> {
        let io = |source| FillError::Io {
            path: dir.to_owned(),
            source,
        };
        let mut files = BTreeMap::new();
        for entry in std::fs::read_dir(dir).map_err(io)? {
            let name = entry
                .map_err(io)?
                .file_name()
                .to_string_lossy()
                .into_owned();
            let named_like_a_result = name.ends_with(".json")
                && CellId::ALL
                    .iter()
                    .any(|c| name.starts_with(&format!("{}-", c.as_str())));
            if !named_like_a_result {
                continue;
            }
            let result =
                CellResult::from_json(&read(&dir.join(&name))?).map_err(|e| FillError::Result {
                    file: name.clone(),
                    reason: e.to_string(),
                })?;
            if name != result_name(result.cell, &result.arm, result.provenance.attempt) {
                return Err(FillError::Result {
                    file: name,
                    reason: "named for another cell, arm or attempt than it holds".to_owned(),
                });
            }
            files.insert(name, result);
        }
        Ok(Results { files })
    }

    /// Builds a set from parsed results, named `<cell>-<arm>-a<N>.json`.
    #[must_use]
    pub fn from_results(results: impl IntoIterator<Item = CellResult>) -> Results {
        Results {
            files: results
                .into_iter()
                .map(|r| (result_name(r.cell, &r.arm, r.provenance.attempt), r))
                .collect(),
        }
    }

    /// The attempts of `arm` of `cell`, the latest first.
    fn attempts(&self, cell: CellId, arm: &str) -> Vec<(&str, &CellResult)> {
        let mut found: Vec<(&str, &CellResult)> = self
            .files
            .iter()
            .filter(|(_, r)| r.cell == cell && r.arm == arm)
            .map(|(k, v)| (k.as_str(), v))
            .collect();
        found.sort_by_key(|(_, r)| std::cmp::Reverse(r.provenance.attempt));
        found
    }
}

/// A figure of a result with the passes it was taken from and the rule that took it.
struct Measured {
    /// In base units.
    value: f64,
    /// The pass indexes the value is made of, ascending.
    passes: Vec<u32>,
    /// How the passes were combined.
    rule: &'static str,
}

/// The value of `metric` in `result`, in base units; `Err` says why there is none.
fn measure(result: &CellResult, metric: Metric) -> Result<f64, String> {
    measure_with(result, metric).map(|m| m.value)
}

/// [`measure`] with the passes and the rule.
fn measure_with(result: &CellResult, metric: Metric) -> Result<Measured, String> {
    let healing: BTreeSet<u32> = result
        .cold_state
        .iter()
        .filter(|r| r.state.healing)
        .map(|r| r.pass)
        .collect();
    let counted: Vec<&PassResult> = result
        .passes
        .iter()
        .filter(|p| {
            !p.discarded
                && p.gate_tripped.is_none()
                && p.cold_guard.is_none()
                && !healing.contains(&p.pass)
        })
        .collect();
    if counted.is_empty() {
        return Err(
            "no pass counts (warm-up, gate-aborted, cold-guard-aborted or healing only)".to_owned(),
        );
    }
    #[allow(clippy::cast_precision_loss)]
    let as_f = |v: u64| v as f64;
    // The median (the lower middle of an even count) of the per-pass values, with the passes
    // that had one.
    let median = |pick: fn(&PassResult) -> Option<u64>, passes: &[&PassResult]| {
        let mut values: Vec<(u64, u32)> = passes
            .iter()
            .filter_map(|p| pick(p).map(|v| (v, p.pass)))
            .collect();
        values.sort_unstable();
        let mid = values.get(values.len().wrapping_sub(1) / 2).copied()?;
        let mut used: Vec<u32> = values.iter().map(|(_, pass)| *pass).collect();
        used.sort_unstable();
        Some((as_f(mid.0), used))
    };
    let all: Vec<u32> = counted.iter().map(|p| p.pass).collect();
    let figure = match metric {
        Metric::P50 => median(|p| p.percentiles.map(|q| q.p50_us), &counted)
            .map(|(v, used)| (v, used, "median of the per-pass p50")),
        Metric::P95 => median(|p| p.percentiles.map(|q| q.p95_us), &counted)
            .map(|(v, used)| (v, used, "median of the per-pass p95")),
        Metric::Max => {
            let mut with: Vec<(u64, u32)> = counted
                .iter()
                .filter_map(|p| p.percentiles.map(|q| (q.max_us, p.pass)))
                .collect();
            with.sort_unstable_by_key(|(_, pass)| *pass);
            with.iter().map(|(v, _)| *v).max().map(|v| {
                (
                    as_f(v),
                    with.iter().map(|(_, p)| *p).collect(),
                    "largest per-pass max",
                )
            })
        }
        Metric::P99 => {
            let quiet: Vec<&PassResult> = counted
                .iter()
                .copied()
                .filter(|p| p.window == Window::Quiet)
                .collect();
            let n: u64 = quiet.iter().map(|p| p.requests).sum();
            if n < P99_MIN_REQUESTS {
                return Err(format!(
                    "p99 needs {P99_MIN_REQUESTS} quiet requests, the result has {n}"
                ));
            }
            median(|p| p.percentiles.and_then(|q| q.p99_us), &quiet)
                .map(|(v, used)| (v, used, "median of the per-pass p99 of the quiet passes"))
        }
        Metric::Requests => Some((
            as_f(counted.iter().map(|p| p.requests).sum()),
            all,
            "sum over the passes",
        )),
        Metric::Failures => Some((
            as_f(counted.iter().map(|p| p.failures).sum()),
            all,
            "sum over the passes",
        )),
        Metric::Bytes => Some((
            as_f(counted.iter().map(|p| p.bytes).sum()),
            all,
            "sum over the passes",
        )),
        Metric::Elapsed => median(|p| Some(p.elapsed_us), &counted)
            .map(|(v, used)| (v, used, "median of the per-pass wall time")),
        Metric::Throughput => {
            let us: u64 = counted.iter().map(|p| p.elapsed_us).sum();
            let bytes: u64 = counted.iter().map(|p| p.bytes).sum();
            (us > 0).then(|| {
                (
                    as_f(bytes) * 1_000_000.0 / as_f(us),
                    all,
                    "sum of bytes over sum of wall time",
                )
            })
        }
    };
    figure
        .map(|(value, passes, rule)| Measured {
            value,
            passes,
            rule,
        })
        .ok_or_else(|| format!("no pass has a {}", metric.name()))
}

/// `1234.6` as `1,235`, `12.34` as `12.3`, `1.234` as `1.23`: three significant figures at
/// least, the integer part grouped by thousands.
fn format_value(value: f64, unit: Unit) -> String {
    let decimals = if unit.integral || value >= 100.0 {
        0
    } else if value >= 10.0 {
        1
    } else {
        2
    };
    let fixed = format!("{value:.decimals$}");
    let (int, frac) = match fixed.split_once('.') {
        Some((i, f)) => (i, Some(f)),
        None => (fixed.as_str(), None),
    };
    let mut grouped = String::new();
    for (k, ch) in int.chars().enumerate() {
        if k > 0 && (int.len() - k) % 3 == 0 {
            grouped.push(',');
        }
        grouped.push(ch);
    }
    if let Some(f) = frac {
        grouped.push('.');
        grouped.push_str(f);
    }
    grouped.push_str(unit.suffix);
    grouped
}

/// The number of a formatted value as the paper prints it: the thousands commas dropped, the
/// unit left out.
fn printed_number(text: &str) -> String {
    text.chars()
        .take_while(|c| c.is_ascii_digit() || matches!(c, '.' | ','))
        .filter(|c| *c != ',')
        .collect()
}

/// What a mapping gives for the results: the text, where from and the label.
struct Found {
    text: String,
    source: String,
    label: String,
    /// The row that backs the printed number.
    row: String,
}

/// The figure a mapping asks for, or why there is none.
fn evaluate(mapping: &Mapping, results: &Results) -> Result<Found, String> {
    let arms = mapping.arms();
    let Some(tries) = arms
        .iter()
        .map(|a| results.attempts(mapping.cell, a))
        .find(|t| !t.is_empty())
    else {
        return Err(format!(
            "no result for cell {} arm {}",
            mapping.cell.as_str(),
            arms.join(" or ")
        ));
    };
    let several = tries.len() > 1;
    let mut why = Vec::new();
    for (file, result) in tries {
        for (metric, unit) in &mapping.metrics {
            match measure_with(result, *metric) {
                Ok(m) => {
                    let text = format_value(m.value / unit.divisor, *unit);
                    let source = format!("{file}#{}", metric.name());
                    let passes: Vec<String> = m.passes.iter().map(u32::to_string).collect();
                    let how = format!(
                        "{source}; passes {}; {}; {} {} divided by {} and rounded as printed",
                        passes.join(","),
                        m.rule,
                        m.value,
                        units_of(*metric)[0].name,
                        unit.divisor,
                    );
                    let row = [
                        "derived",
                        &format!("fill:{}:{}", mapping.id, metric.name()),
                        &printed_number(&text),
                        unit.name,
                        &how,
                        &mapping.id,
                    ]
                    .join("\t");
                    return Ok(Found {
                        text,
                        source,
                        label: mapping.label(&result.arm),
                        row,
                    });
                }
                Err(e) if several => why.push(format!("{file}: {e}")),
                Err(e) => why.push(e),
            }
        }
    }
    Err(why.join("; "))
}

// ---------------------------------------------------------------------------------------------
// The HTML scan

#[derive(Debug)]
struct Attr {
    name: String,
    /// The whole attribute, `name` or `name="value"`.
    start: usize,
    end: usize,
    /// The value without quotes, character references for `& " < >` resolved.
    value: Option<String>,
}

#[derive(Debug)]
struct StartTag {
    name: String,
    start: usize,
    end: usize,
    attrs: Vec<Attr>,
    self_closing: bool,
}

impl StartTag {
    fn attr(&self, name: &str) -> Option<&Attr> {
        self.attrs.iter().find(|a| a.name == name)
    }
}

#[derive(Debug)]
enum Token {
    Open(StartTag),
    Close { name: String, start: usize },
}

const VOID: [&str; 15] = [
    "area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "param", "source",
    "track", "wbr", "keygen",
];
/// Elements whose content is text up to the matching end tag: markup in it is not markup.
const RAW_TEXT: [&str; 4] = ["script", "style", "textarea", "title"];

fn is_space(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | b'\r' | 0x0c)
}

fn unescape(raw: &str) -> String {
    raw.replace("&quot;", "\"")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Splits `html` into start and end tags, skipping comments, declarations and raw text. All
/// cut points are ASCII bytes, so every slice is on a character boundary.
fn tokenize(html: &str) -> Result<Vec<Token>, FillError> {
    let bytes = html.as_bytes();
    let lower = html.to_ascii_lowercase();
    let mut tokens = Vec::new();
    let mut i = 0;
    while let Some(off) = html[i..].find('<') {
        i += off;
        let rest = &html[i..];
        if rest.starts_with("<!--") {
            i = html[i + 4..]
                .find("-->")
                .map_or(html.len(), |e| i + 4 + e + 3);
        } else if rest.starts_with("<!") || rest.starts_with("<?") {
            i = html[i..].find('>').map_or(html.len(), |e| i + e + 1);
        } else if rest.starts_with("</") {
            let end = html[i..].find('>').map_or(html.len(), |e| i + e + 1);
            let name = html[i + 2..end.saturating_sub(1).max(i + 2)]
                .trim()
                .to_ascii_lowercase();
            tokens.push(Token::Close { name, start: i });
            i = end;
        } else if bytes.get(i + 1).is_some_and(u8::is_ascii_alphabetic) {
            let tag = parse_start_tag(html, i)?;
            i = tag.end;
            if RAW_TEXT.contains(&tag.name.as_str()) && !tag.self_closing {
                let close = format!("</{}", tag.name);
                let next = lower[i..].find(&close).map_or(html.len(), |e| i + e);
                tokens.push(Token::Open(tag));
                i = next;
            } else {
                tokens.push(Token::Open(tag));
            }
        } else {
            i += 1;
        }
    }
    Ok(tokens)
}

fn parse_start_tag(html: &str, start: usize) -> Result<StartTag, FillError> {
    let bytes = html.as_bytes();
    let unterminated = || FillError::Html(format!("a tag at byte {start} is not terminated"));
    let mut j = start + 1;
    while j < bytes.len() && !is_space(bytes[j]) && bytes[j] != b'/' && bytes[j] != b'>' {
        j += 1;
    }
    let name = html[start + 1..j].to_ascii_lowercase();
    let mut attrs = Vec::new();
    loop {
        while j < bytes.len() && (is_space(bytes[j]) || bytes[j] == b'/') {
            j += 1;
        }
        match bytes.get(j) {
            None => return Err(unterminated()),
            Some(b'>') => {
                let self_closing = bytes[start..j].last() == Some(&b'/');
                return Ok(StartTag {
                    name,
                    start,
                    end: j + 1,
                    attrs,
                    self_closing,
                });
            }
            Some(_) => {}
        }
        let attr_start = j;
        while j < bytes.len() && !is_space(bytes[j]) && !matches!(bytes[j], b'=' | b'>' | b'/') {
            j += 1;
        }
        if j == attr_start {
            // A stray `=`: take it as part of a name so the scan always advances.
            j += 1;
        }
        let attr_name = html[attr_start..j].to_ascii_lowercase();
        let mut k = j;
        while k < bytes.len() && is_space(bytes[k]) {
            k += 1;
        }
        let mut value = None;
        let mut end = j;
        if bytes.get(k) == Some(&b'=') {
            k += 1;
            while k < bytes.len() && is_space(bytes[k]) {
                k += 1;
            }
            match bytes.get(k) {
                None => return Err(unterminated()),
                Some(&q) if q == b'"' || q == b'\'' => {
                    let close = html[k + 1..].find(q as char).ok_or_else(unterminated)?;
                    value = Some(unescape(&html[k + 1..k + 1 + close]));
                    end = k + close + 2;
                }
                Some(_) => {
                    let v = k;
                    while k < bytes.len() && !is_space(bytes[k]) && bytes[k] != b'>' {
                        k += 1;
                    }
                    value = Some(unescape(&html[v..k]));
                    end = k;
                }
            }
            j = end;
        }
        attrs.push(Attr {
            name: attr_name,
            start: attr_start,
            end,
            value,
        });
    }
}

/// For each token, the start of the end tag of the element it opens: an end tag closes the nearest
/// open element of its name, and what was open inside it (an unclosed `<p>`) is dropped.
fn pair_closes(tokens: &[Token]) -> Vec<Option<usize>> {
    // Each entry is the byte offset where the matching end tag starts.
    let mut closes = vec![None; tokens.len()];
    let mut open: Vec<usize> = Vec::new();
    for (idx, token) in tokens.iter().enumerate() {
        match token {
            Token::Open(t) if !t.self_closing && !VOID.contains(&t.name.as_str()) => {
                open.push(idx);
            }
            Token::Open(_) => {}
            Token::Close { name, start } => {
                let found = open
                    .iter()
                    .rposition(|&o| matches!(&tokens[o], Token::Open(t) if &t.name == name));
                if let Some(pos) = found {
                    closes[open[pos]] = Some(*start);
                    open.truncate(pos);
                }
            }
        }
    }
    closes
}

/// A placeholder of the paper.
struct Placeholder<'a> {
    id: String,
    tag: &'a StartTag,
    /// The element's content, if the element is closed.
    inner: Option<(usize, usize)>,
}

fn placeholders(tokens: &[Token]) -> Vec<Placeholder<'_>> {
    let closes = pair_closes(tokens);
    tokens
        .iter()
        .enumerate()
        .filter_map(|(idx, token)| {
            let Token::Open(tag) = token else { return None };
            let id = tag.attr("data-cell")?.value.clone().unwrap_or_default();
            let inner = closes[idx].map(|close| (tag.end, close));
            Some(Placeholder { id, tag, inner })
        })
        .collect()
}

// ---------------------------------------------------------------------------------------------
// Fill and check

/// What a fill did, in counts.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FillReport {
    /// Elements given a value.
    pub filled: usize,
    /// Elements left alone as blocked.
    pub blocked: usize,
    /// Elements left with their text and a note.
    pub noted: usize,
    /// Elements whose id the mapping does not know (left alone).
    pub unmapped: usize,
}

/// One replacement of `start..end` of the paper by `text`.
struct Edit {
    start: usize,
    end: usize,
    text: String,
}

/// The edits that set the attributes `set` (in place when present, else appended to the tag) and
/// remove those of `remove` (with the whitespace before them) in `tag`.
fn attribute_edits(html: &str, tag: &StartTag, set: &[(&str, &str)], remove: &[&str]) -> Vec<Edit> {
    let mut edits = Vec::new();
    let mut appended = String::new();
    for (name, value) in set {
        let text = format!("{name}=\"{}\"", escape(value));
        match tag.attr(name) {
            Some(a) => edits.push(Edit {
                start: a.start,
                end: a.end,
                text,
            }),
            None => {
                appended.push(' ');
                appended.push_str(&text);
            }
        }
    }
    for name in remove {
        if let Some(a) = tag.attr(name) {
            let mut start = a.start;
            // An attribute follows the tag name, so this stops at the latest there.
            while is_space(html.as_bytes()[start - 1]) {
                start -= 1;
            }
            edits.push(Edit {
                start,
                end: a.end,
                text: String::new(),
            });
        }
    }
    // Straight after the last attribute, so spacing before the `>` is left as the author had it.
    let at = tag.attrs.last().map_or(tag.end - 1, |a| a.end);
    if !appended.is_empty() {
        edits.push(Edit {
            start: at,
            end: at,
            text: appended,
        });
    }
    edits
}

fn apply(html: &str, mut edits: Vec<Edit>) -> Result<String, FillError> {
    edits.sort_by_key(|e| (e.start, e.end));
    let mut out = String::with_capacity(html.len());
    let mut at = 0;
    for e in &edits {
        if e.start < at {
            return Err(FillError::Html(format!(
                "two placeholders overlap near byte {}: a data-cell inside another",
                e.start
            )));
        }
        out.push_str(&html[at..e.start]);
        out.push_str(&e.text);
        at = e.end;
    }
    out.push_str(&html[at..]);
    Ok(out)
}

/// Whether the paper says a cell is not to be given a measured number: it is blocked (a reason
/// it cannot be measured yet), or it has a basis (design or code reading, not a measurement).
fn is_held(tag: &StartTag) -> bool {
    tag.attr("data-blocked").is_some() || tag.attr("data-basis").is_some()
}

fn by_id(mappings: &[Mapping]) -> BTreeMap<&str, &Mapping> {
    mappings.iter().map(|m| (m.id.as_str(), m)).collect()
}

/// Fills the paper; returns the new text and what happened.
///
/// # Errors
/// [`FillError::Html`] if the paper cannot be scanned, a placeholder is not a closed element,
/// or two placeholders are nested.
pub fn fill(
    html: &str,
    mappings: &[Mapping],
    results: &Results,
    tsv: &mut ResultTsv,
) -> Result<(String, FillReport), FillError> {
    let tokens = tokenize(html)?;
    let known = by_id(mappings);
    let mut report = FillReport::default();
    let mut edits = Vec::new();
    for ph in placeholders(&tokens) {
        if is_held(ph.tag) {
            report.blocked += 1;
            continue;
        }
        let Some(mapping) = known.get(ph.id.as_str()) else {
            report.unmapped += 1;
            continue;
        };
        if mapping.blocked {
            report.blocked += 1;
            continue;
        }
        let Some((inner_start, inner_end)) = ph.inner else {
            return Err(FillError::Html(format!(
                "placeholder {:?} is not a closed element with content",
                ph.id
            )));
        };
        match evaluate(mapping, results) {
            Ok(Found {
                text,
                source,
                label,
                row,
            }) => {
                // A table this run did not write belongs to whoever filled the cell.
                if let Some(found) = ph.tag.attr("data-result").and_then(|a| a.value.as_deref())
                    && found.ends_with(".tsv")
                    && found != tsv.name()
                {
                    return Err(FillError::Foreign {
                        cell: ph.id,
                        found: found.to_owned(),
                        ours: tsv.name().to_owned(),
                    });
                }
                tsv.upsert(row);
                let mut set = vec![
                    ("data-source", source),
                    ("data-label", label),
                    ("data-result", tsv.name().to_owned()),
                ];
                let mut remove = vec!["data-note"];
                // A filled cell is no longer a placeholder: `tbd` leaves its class list.
                if let Some(class) = ph.tag.attr("class").and_then(|a| a.value.as_deref()) {
                    let kept: Vec<&str> =
                        class.split_whitespace().filter(|c| *c != "tbd").collect();
                    if kept.len() != class.split_whitespace().count() {
                        if kept.is_empty() {
                            remove.push("class");
                        } else {
                            set.push(("class", kept.join(" ")));
                        }
                    }
                }
                let set: Vec<(&str, &str)> = set.iter().map(|(n, v)| (*n, v.as_str())).collect();
                edits.extend(attribute_edits(html, ph.tag, &set, &remove));
                edits.push(Edit {
                    start: inner_start,
                    end: inner_end,
                    text: escape(&text).replace("&quot;", "\""),
                });
                report.filled += 1;
            }
            Err(why) => {
                edits.extend(attribute_edits(html, ph.tag, &[("data-note", &why)], &[]));
                report.noted += 1;
            }
        }
    }
    Ok((apply(html, edits)?, report))
}

/// Lists what is wrong between paper, mapping and results; empty when nothing is.
///
/// # Errors
/// [`FillError::Html`] if the paper cannot be scanned.
pub fn check(
    html: &str,
    mappings: &[Mapping],
    results: &Results,
    tsv: Option<&ResultTsv>,
) -> Result<Vec<String>, FillError> {
    let tokens = tokenize(html)?;
    let known = by_id(mappings);
    let mut findings = Vec::new();
    for ph in placeholders(&tokens) {
        if !is_held(ph.tag) && !known.contains_key(ph.id.as_str()) {
            findings.push(format!("placeholder {:?} has no mapping", ph.id));
        }
    }
    for m in mappings.iter().filter(|m| !m.blocked) {
        if let Err(why) = evaluate(m, results) {
            findings.push(format!("mapping {:?} has no result: {why}", m.id));
        }
    }
    for token in &tokens {
        let Token::Open(tag) = token else { continue };
        let who = tag.attr("data-cell").and_then(|a| a.value.clone());
        let who = who.unwrap_or_else(|| format!("<{}> at byte {}", tag.name, tag.start));
        if let Some(source) = tag.attr("data-source") {
            let source = source.value.clone().unwrap_or_default();
            if let Err(why) = source_exists(&source, results) {
                findings.push(format!(
                    "filled value {who:?} cites {source:?}, which no longer exists: {why}"
                ));
            }
        }
        // Only the table being written is looked up: the paper's hand-filled cells name a table
        // of their own.
        if let (Some(table), Some(name), Some(cell)) = (
            tsv,
            tag.attr("data-result").and_then(|a| a.value.as_deref()),
            tag.attr("data-cell").and_then(|a| a.value.as_deref()),
        ) && name == table.name()
            && !table.names(cell)
        {
            findings.push(format!("{who:?} has no row naming it in {name}"));
        }
    }
    Ok(findings)
}

/// Whether `file#metric` names a figure the results still have.
fn source_exists(source: &str, results: &Results) -> Result<(), String> {
    let (file, metric) = source
        .split_once('#')
        .ok_or_else(|| "not file#metric".to_owned())?;
    let metric = Metric::parse(metric).ok_or_else(|| format!("unknown metric {metric:?}"))?;
    let result = results
        .files
        .get(file)
        .ok_or_else(|| format!("no file {file}"))?;
    measure(result, metric).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::result::{Percentiles, Provenance, SCHEMA_VERSION};

    fn pass(index: u32, window: Window, requests: u64, p99: Option<u64>) -> PassResult {
        PassResult {
            pass: index,
            window,
            discarded: index == 0,
            requests,
            failures: 1,
            bytes: 1_000_000,
            elapsed_us: 500_000,
            percentiles: Some(Percentiles {
                p50_us: 1_000 * u64::from(index),
                p95_us: 20_000,
                p99_us: p99,
                max_us: 90_000 + u64::from(index),
            }),
            gate_tripped: None,
            cold_guard: None,
            proxy_requests: None,
            disk_busy: BTreeMap::new(),
            peak_in_flight: 0,
        }
    }

    /// Pass 0 is the warm-up (p50 0); passes 1..=3 have p50 1, 2, 3 ms.
    fn result(cell: CellId, arm: &str, requests: u64, p99: Option<u64>) -> CellResult {
        CellResult {
            schema_version: SCHEMA_VERSION,
            cell,
            arm: arm.to_owned(),
            started_unix_ms: 0,
            finished_unix_ms: 1,
            provenance: Provenance::unrecorded(),
            passes: (0..4)
                .map(|i| pass(i, Window::Quiet, requests, p99))
                .collect(),
            notes: Vec::new(),
            cold_state: Vec::new(),
            rate_caps: None,
        }
    }

    fn tsv() -> ResultTsv {
        ResultTsv::new("r.tsv").unwrap()
    }

    fn fill(html: &str, m: &[Mapping], r: &Results) -> Result<(String, FillReport), FillError> {
        super::fill(html, m, r, &mut tsv())
    }

    fn check(html: &str, m: &[Mapping], r: &Results) -> Result<Vec<String>, FillError> {
        super::check(html, m, r, None)
    }

    fn mappings(json: &str) -> Vec<Mapping> {
        Mapping::parse_all(json).unwrap()
    }

    fn row(id: &str, metric: &str, unit: &str, arm: &str) -> String {
        format!(
            r#"{{"id":"{id}","cell":"cold-put","metric":"{metric}","unit":"{unit}","condition":"{arm}"}}"#
        )
    }

    fn one(id: &str, metric: &str, unit: &str, arm: &str) -> Vec<Mapping> {
        mappings(&format!("[{}]", row(id, metric, unit, arm)))
    }

    fn results() -> Results {
        Results::from_results([
            result(CellId::ColdPut, "instance", 1_200, Some(80_000)),
            result(CellId::ColdPut, "direct", 100, None),
        ])
    }

    /// Catches a value written into the wrong place, a source or label not recorded, and any
    /// byte outside the element and the two attributes changing: comments, scripts, odd
    /// spacing, single quotes, CRLF and non-ASCII text all stay as they are.
    #[test]
    fn a_filled_paper_differs_only_in_the_cell_and_its_two_attributes() {
        let html = "<!DOCTYPE html>\r\n<!-- <b data-cell=\"x\">keep</b> -->\n<script>var s = '<i data-cell=\"x\">';</script>\n\
<p  class='a'>Naïve put <span  data-cell='x'   id=\"k\" >TBD</span>, then <b>bold</b>.</p>\n<p>tail</p>";
        let (out, report) = fill(html, &one("x", "p50", "ms", "instance"), &results()).unwrap();
        let expected = "<!DOCTYPE html>\r\n<!-- <b data-cell=\"x\">keep</b> -->\n<script>var s = '<i data-cell=\"x\">';</script>\n\
<p  class='a'>Naïve put <span  data-cell='x'   id=\"k\" data-source=\"cold-put-instance-a1.json#p50\" data-label=\"test instance\" data-result=\"r.tsv\" >2.00 ms</span>, then <b>bold</b>.</p>\n<p>tail</p>";
        assert_eq!(out, expected);
        assert_eq!(
            report,
            FillReport {
                filled: 1,
                ..FillReport::default()
            }
        );
    }

    /// Catches a rewrite that is not a fixed point: the second run must give the first run's
    /// bytes, with a value, with a note and with attributes already present.
    #[test]
    fn filling_twice_gives_the_same_bytes() {
        let html = "<table><tr><td data-cell=\"a\">?</td><td data-cell=\"b\">keep</td>\
<td data-cell=\"c\" data-source=\"old.json#p50\" data-label=\"farm\">9</td></tr></table>";
        let m = mappings(&format!(
            "[{},{},{}]",
            row("a", "p50", "ms", "instance"),
            row("b", "p99", "ms", "direct"),
            row("c", "p95", "us", "direct"),
        ));
        let (once, _) = fill(html, &m, &results()).unwrap();
        let (twice, report) = fill(&once, &m, &results()).unwrap();
        assert_eq!(once, twice);
        assert_eq!(report.filled, 2);
        assert_eq!(report.noted, 1);
        assert!(once.contains("data-source=\"cold-put-direct-a1.json#p95\""));
        assert!(once.contains("data-result=\"r.tsv\""));
        assert_eq!(once.matches("data-result").count(), 2, "{once}");
        assert!(!once.contains("old.json"));
    }

    /// Catches a blocked cell that is filled: by the mapping, by the attribute on the element,
    /// and an unmapped cell, which is counted and left alone.
    #[test]
    fn blocked_and_unmapped_cells_are_untouched() {
        let html =
            "<i data-cell=\"a\">1</i><i data-cell=\"b\" data-blocked>2</i><i data-cell=\"z\">3</i>";
        let m = mappings(&format!(
            "[{},{}]",
            row("a", "p50", "ms", "instance").replace('}', r#","blocked":true}"#),
            row("b", "p50", "ms", "instance"),
        ));
        let (out, report) = fill(html, &m, &results()).unwrap();
        assert_eq!(out, html);
        assert_eq!(
            report,
            FillReport {
                blocked: 2,
                unmapped: 1,
                ..FillReport::default()
            }
        );
    }

    /// Catches a p99 quoted from too few quiet requests, a missing fallback, and a stale note
    /// left on a cell that has since been filled.
    #[test]
    fn p99_needs_a_thousand_quiet_requests_else_falls_back_or_notes() {
        let html = "<b data-cell=\"a\">old</b>";
        // 3 counted passes x 100 requests: not enough.
        let few = Results::from_results([result(CellId::ColdPut, "direct", 100, Some(5_000))]);
        let (out, report) = fill(html, &one("a", "p99", "ms", "direct"), &few).unwrap();
        assert_eq!(report.noted, 1);
        assert!(out.starts_with("<b data-cell=\"a\" data-note=\"p99 needs 1000 quiet requests, the result has 300\">old</b>"), "{out}");
        assert!(!out.contains("data-source"));

        let (out, report) = fill(html, &one("a", "p99|p95", "ms", "direct"), &few).unwrap();
        assert_eq!(report.filled, 1);
        assert!(
            out.contains("data-source=\"cold-put-direct-a1.json#p95\""),
            "{out}"
        );
        assert!(out.ends_with(">20.0 ms</b>"), "{out}");

        // Busy passes do not count toward p99 whatever their number.
        let mut busy = result(CellId::ColdPut, "direct", 5_000, Some(1));
        for p in &mut busy.passes {
            p.window = Window::Busy;
        }
        let busy = Results::from_results([busy]);
        assert_eq!(
            fill(html, &one("a", "p99", "ms", "direct"), &busy)
                .unwrap()
                .1
                .noted,
            1
        );

        // Enough requests: filled, and the earlier note goes.
        let (noted, _) = fill(html, &one("a", "p99", "ms", "direct"), &few).unwrap();
        let (filled, _) = fill(&noted, &one("a", "p99", "ms", "instance"), &results()).unwrap();
        assert!(!filled.contains("data-note"), "{filled}");
        assert!(filled.ends_with(">80.0 ms</b>"), "{filled}");
    }

    /// Catches a unit that scales wrongly, a rounding rule that drifts, and grouping.
    #[test]
    fn values_are_formatted_by_unit() {
        let ms = TIME_UNITS[1];
        assert_eq!(format_value(1.234, ms), "1.23 ms");
        assert_eq!(format_value(12.34, ms), "12.3 ms");
        assert_eq!(format_value(123.4, ms), "123 ms");
        assert_eq!(format_value(1_234.6, ms), "1,235 ms");
        assert_eq!(format_value(999.0, COUNT_UNITS[0]), "999");
        assert_eq!(format_value(1_234_567.0, COUNT_UNITS[0]), "1,234,567");
        assert_eq!(format_value(100.0, TIME_UNITS[0]), "100 us");
        let r = results();
        let get = |metric, unit| {
            let m = one("a", metric, unit, "direct");
            evaluate(&m[0], &r).unwrap().text
        };
        assert_eq!(get("p50", "us"), "2,000 us");
        assert_eq!(get("p50", "s"), "0.00 s");
        assert_eq!(get("max", "ms"), "90.0 ms");
        assert_eq!(get("requests", "count"), "300");
        assert_eq!(get("failures", "count"), "3");
        assert_eq!(get("bytes", "MB"), "3.00 MB");
        assert_eq!(get("bytes", "KiB"), "2,930 KiB");
        // 3 MB over 1.5 s.
        assert_eq!(get("throughput", "MB/s"), "2.00 MB/s");
        assert_eq!(get("elapsed", "ms"), "500 ms");
    }

    /// Catches a mapping that is accepted and then does nothing: unit/metric mismatch, unknown
    /// metric, repeated id, unknown field, bad JSON.
    #[test]
    fn a_bad_mapping_is_refused() {
        let bad = |json: &str| Mapping::parse_all(json).unwrap_err().to_string();
        assert!(bad(&format!("[{}]", row("a", "p50", "MB", "x"))).contains("does not suit"));
        assert!(bad(&format!("[{}]", row("a", "p42", "ms", "x"))).contains("unknown metric"));
        let twice = format!("[{0},{0}]", row("a", "p50", "ms", "x"));
        assert!(bad(&twice).contains("repeated"));
        assert!(
            bad(&format!(
                "[{}]",
                row("a", "p50", "ms", "x").replace('}', r#","oops":1}"#)
            ))
            .contains("oops")
        );
        assert!(bad("{").contains("mapping"));
        let sized = r#"[{"id":"a","cell":"cold-put","metric":"p50","unit":"ms","condition":"c","size":4096,"label":"farm"}]"#;
        assert_eq!(
            Mapping::parse_all(sized).unwrap()[0].arms(),
            ["c-4096", "c"]
        );
        let named = sized.replace("4096", "\"4MiB\"");
        assert_eq!(
            Mapping::parse_all(&named).unwrap()[0].arms(),
            ["c-4MiB", "c"]
        );
    }

    /// Catches a size that does not choose the arm, and a label rule that drifts.
    #[test]
    fn the_size_picks_the_arm_and_the_label_follows_the_arm() {
        let r = Results::from_results([
            result(CellId::ColdPut, "direct-4096", 100, None),
            result(CellId::ColdPut, "direct", 100, None),
        ]);
        let json = r#"[{"id":"a","cell":"cold-put","metric":"requests","unit":"count","condition":"direct","size":4096}]"#;
        let (out, _) = fill("<b data-cell=\"a\"></b>", &mappings(json), &r).unwrap();
        assert!(
            out.contains("cold-put-direct-4096-a1.json#requests"),
            "{out}"
        );
        assert!(out.contains("data-label=\"farm\""), "{out}");
        let json = json.replace("\"size\":4096", "\"label\":\"custom\"");
        let (out, _) = fill("<b data-cell=\"a\"></b>", &mappings(&json), &r).unwrap();
        assert!(out.contains("cold-put-direct-a1.json#requests"), "{out}");
        assert!(out.contains("data-label=\"custom\""), "{out}");
    }

    /// Catches a check that passes when a placeholder has no mapping, a mapping has no
    /// result, or a filled value cites a file or figure that is gone; and one that complains
    /// when all is well.
    #[test]
    fn check_lists_unmapped_unresulted_and_orphaned() {
        let m = mappings(&format!(
            "[{},{},{}]",
            row("ok", "p50", "ms", "instance"),
            row("nores", "p50", "ms", "missing-arm"),
            row("few", "p99", "ms", "direct"),
        ));
        let html = "<i data-cell=\"ok\" data-source=\"cold-put-instance-a1.json#p50\">1</i>\
<i data-cell=\"nomap\">?</i><i data-cell=\"blk\" data-blocked>?</i>\
<i data-cell=\"nores\">?</i><i data-cell=\"few\">?</i>\
<u data-source=\"cold-put-gone.json#p50\">2</u>\
<u data-cell=\"g2\" data-source=\"cold-put-direct-a1.json#p99\">3</u>\
<u data-cell=\"g3\" data-source=\"cold-put-direct-a1.json#bogus\">3</u><u data-source=\"nohash\">4</u>";
        let findings = check(html, &m, &results()).unwrap();
        let text = findings.join("\n");
        assert!(
            text.contains("placeholder \"nomap\" has no mapping"),
            "{text}"
        );
        assert!(!text.contains("\"blk\""), "{text}");
        assert!(!text.contains("\"ok\""), "{text}");
        assert!(text.contains("mapping \"nores\" has no result"), "{text}");
        assert!(
            text.contains("mapping \"few\" has no result: p99 needs"),
            "{text}"
        );
        assert!(text.contains("<u> at byte"), "{text}");
        assert!(text.contains("no file cold-put-gone.json"), "{text}");
        assert!(
            text.contains("\"g2\" cites \"cold-put-direct-a1.json#p99\""),
            "{text}"
        );
        assert!(text.contains("unknown metric \"bogus\""), "{text}");
        assert!(text.contains("not file#metric"), "{text}");
        assert!(text.contains("placeholder \"g2\" has no mapping"), "{text}");
        assert_eq!(findings.len(), 9, "{text}");

        let good = "<i data-cell=\"ok\">1</i>";
        let (filled, _) = fill(good, &m, &results()).unwrap();
        let only_ok = mappings(&format!("[{}]", row("ok", "p50", "ms", "instance")));
        assert!(check(&filled, &only_ok, &results()).unwrap().is_empty());
    }

    /// Catches a placeholder that cannot be filled being skipped silently, and nesting that
    /// would drop an inner placeholder.
    #[test]
    fn unfillable_placeholders_are_errors() {
        let m = one("a", "p50", "ms", "instance");
        let unclosed = fill("<p><b data-cell=\"a\">x", &m, &results()).unwrap_err();
        assert!(
            unclosed.to_string().contains("not a closed element"),
            "{unclosed}"
        );
        let void = fill("<br data-cell=\"a\">", &m, &results()).unwrap_err();
        assert!(void.to_string().contains("not a closed element"), "{void}");
        let m2 = mappings(&format!(
            "[{},{}]",
            row("a", "p50", "ms", "instance"),
            row("b", "p95", "ms", "instance")
        ));
        let nested = fill(
            "<div data-cell=\"a\"><b data-cell=\"b\">x</b></div>",
            &m2,
            &results(),
        )
        .unwrap_err();
        assert!(nested.to_string().contains("overlap"), "{nested}");
        let cut = fill("<b data-cell=\"a", &m, &results()).unwrap_err();
        assert!(cut.to_string().contains("not terminated"), "{cut}");
        let cut = fill("<b data-cell=", &m, &results()).unwrap_err();
        assert!(cut.to_string().contains("not terminated"), "{cut}");
    }

    /// Catches a scanner that mishandles what real pages carry: unquoted values, valueless
    /// attributes, an unclosed `<p>` inside the cell's parent, upper-case tags, nested same-name
    /// elements, entities in the source file name.
    #[test]
    fn the_scanner_copes_with_ordinary_html() {
        let html = "<DIV><P>one<SPAN DATA-CELL=a hidden>x<span>in</span>y</SPAN><P>two</DIV>\
<?php x ?><![CDATA[ <b data-cell=\"a\"> ]]><textarea><b data-cell=\"a\"></textarea>";
        let (out, report) = fill(html, &one("a", "max", "ms", "instance"), &results()).unwrap();
        assert_eq!(report.filled, 1);
        assert!(out.contains("<SPAN DATA-CELL=a hidden data-source=\"cold-put-instance-a1.json#max\" data-label=\"test instance\" data-result=\"r.tsv\">90.0 ms</SPAN>"), "{out}");
        assert!(out.ends_with(
            "<P>two</DIV><?php x ?><![CDATA[ <b data-cell=\"a\"> ]]><textarea><b data-cell=\"a\"></textarea>"
        ));
    }

    type Attrs = Vec<(String, Option<String>)>;

    fn tags(html: &str) -> Vec<(String, Attrs)> {
        tokenize(html)
            .unwrap()
            .into_iter()
            .filter_map(|t| match t {
                Token::Open(t) => Some((
                    t.name,
                    t.attrs.into_iter().map(|a| (a.name, a.value)).collect(),
                )),
                Token::Close { .. } => None,
            })
            .collect()
    }

    fn attr(name: &str, value: Option<&str>) -> (String, Option<String>) {
        (name.to_owned(), value.map(str::to_owned))
    }

    /// Catches a scanner that loops, panics or mis-reads on the malformed corners of real
    /// pages: a lone `<`, self-closing tags, a slash inside a tag, a stray `=`, spaces around
    /// `=`, unquoted values, a stray end tag, and a page that stops inside a tag.
    #[test]
    fn the_scanner_survives_malformed_markup() {
        assert_eq!(tags("1 < 2 <i>x</i> a<"), [("i".to_owned(), vec![])]);
        assert_eq!(tags("<b a/b>")[0].1, [attr("a", None), attr("b", None)]);
        assert_eq!(tags("<b =x>")[0].1, [attr("=", None), attr("x", None)]);
        assert_eq!(
            tags("<b a = \"1\" c= 2 d =3>")[0].1,
            [
                attr("a", Some("1")),
                attr("c", Some("2")),
                attr("d", Some("3"))
            ]
        );
        for cut in ["<b", "<b ", "<b a", "<b a  ", "<b a=1", "<b a='1", "<b a= "] {
            let err = tokenize(cut).unwrap_err();
            assert!(err.to_string().contains("not terminated"), "{cut}: {err}");
        }
        // Self-closing tags and a stray end tag do not capture the cell; a self-closing
        // `<script />` does not swallow what follows it as raw text.
        let html = "</p><br/><x /><script src=x /><b data-cell=\"a\">?</b>";
        let (out, report) = fill(html, &one("a", "p50", "ms", "instance"), &results()).unwrap();
        assert_eq!(report.filled, 1);
        assert!(out.ends_with(">2.00 ms</b>"), "{out}");
        assert!(out.starts_with("</p><br/><x /><script src=x />"), "{out}");
    }

    /// Catches a result with nothing countable (only the warm-up, or all gate-aborted) filled
    /// with a number.
    #[test]
    fn a_result_with_no_counted_pass_has_no_figures() {
        let mut r = result(CellId::ColdPut, "direct", 100, None);
        r.passes.truncate(1);
        assert!(
            measure(&r, Metric::P50)
                .unwrap_err()
                .contains("no pass counts")
        );
        let mut r = result(CellId::ColdPut, "direct", 100, None);
        for p in &mut r.passes {
            p.gate_tripped = Some("loadavg_1m".to_owned());
        }
        assert!(measure(&r, Metric::Requests).is_err());
        let mut r = result(CellId::ColdPut, "direct", 100, None);
        for p in &mut r.passes {
            p.percentiles = None;
            p.elapsed_us = 0;
        }
        assert!(
            measure(&r, Metric::P50)
                .unwrap_err()
                .contains("no pass has a p50")
        );
        assert!(measure(&r, Metric::Throughput).is_err());
        assert!(measure(&r, Metric::Max).is_err());
    }

    fn attempt(n: u32, mut r: CellResult) -> CellResult {
        r.provenance.attempt = n;
        r
    }

    /// Catches a retry read as a second arm, an older attempt winning over a later one, and a
    /// later attempt with nothing countable hiding a good earlier one.
    #[test]
    fn the_latest_attempt_that_gives_a_figure_is_used() {
        let first = result(CellId::ColdPut, "direct", 100, None);
        let mut second = result(CellId::ColdPut, "direct", 100, None);
        for p in &mut second.passes {
            p.requests = 7;
        }
        let both = Results::from_results([first.clone(), attempt(2, second.clone())]);
        let m = one("a", "requests", "count", "direct");
        let found = evaluate(&m[0], &both).unwrap();
        assert_eq!(found.source, "cold-put-direct-a2.json#requests");
        assert_eq!(found.text, "21");

        // The retry was aborted on every counted pass: the first try's figure stands.
        for p in &mut second.passes {
            p.gate_tripped = Some("loadavg_1m".to_owned());
        }
        let both = Results::from_results([first, attempt(2, second)]);
        let found = evaluate(&m[0], &both).unwrap();
        assert_eq!(found.source, "cold-put-direct-a1.json#requests");

        // With no attempt giving a figure the reason names each file.
        let mut only = result(CellId::ColdPut, "direct", 100, None);
        only.passes.truncate(1);
        let mut only2 = only.clone();
        only2.provenance.attempt = 2;
        let none = Results::from_results([only, only2]);
        let why = evaluate(&m[0], &none).err().unwrap();
        assert!(
            why.contains("cold-put-direct-a2.json: no pass counts"),
            "{why}"
        );
        assert!(
            why.contains("cold-put-direct-a1.json: no pass counts"),
            "{why}"
        );
    }

    /// Catches a pass aborted by the cold store guard, or taken while the cold store was
    /// healing, counting toward a figure.
    #[test]
    fn cold_guard_and_healing_passes_do_not_count() {
        use crate::result::{ColdState, ColdTrip, PassColdState};
        let mut r = result(CellId::ColdPut, "direct", 100, None);
        r.passes[1].cold_guard = Some(ColdTrip {
            metric: "p99_ms".to_owned(),
            value: 9.0,
            limit: 1.0,
            requests: 50,
            window_secs: 10,
        });
        r.cold_state.push(PassColdState {
            pass: 2,
            state: ColdState {
                healing: true,
                percent_done: None,
                drives: Vec::new(),
                read_unix_ms: 0,
                source: None,
            },
        });
        // Only pass 3 (p50 3 ms, 100 requests) is left.
        assert_eq!(measure(&r, Metric::Requests).unwrap(), 100.0);
        assert_eq!(measure(&r, Metric::P50).unwrap(), 3_000.0);
        r.passes[3].gate_tripped = Some("loadavg_1m".to_owned());
        assert!(
            measure(&r, Metric::P50)
                .unwrap_err()
                .contains("no pass counts")
        );
    }

    /// Catches a result file read under a name that is not its cell, arm and attempt.
    #[test]
    fn a_result_name_carries_the_attempt() {
        assert_eq!(
            Results::from_results([attempt(3, result(CellId::ColdPut, "x", 1, None))])
                .files
                .keys()
                .collect::<Vec<_>>(),
            ["cold-put-x-a3.json"]
        );
    }

    /// Catches a filled cell that does not say which result file it came from, a cell that stays
    /// a `tbd` placeholder after it is filled, a `tbd` span with no `data-cell` being written,
    /// and a class list that loses its other classes.
    #[test]
    fn a_filled_cell_names_its_result_and_stops_being_a_placeholder() {
        let html = "<p><span class=\"tbd\" data-cell=\"a\">[to measure]</span> \
<span class=\"x tbd y\" data-cell=\"b\">[to measure]</span> \
<span class=\"tbd\">[no cell]</span> <span class=\"tbd\" data-cell=\"z\">[no map]</span></p>";
        let m = mappings(&format!(
            "[{},{}]",
            row("a", "p50", "ms", "instance"),
            row("b", "p95", "ms", "direct")
        ));
        let (out, report) = fill(html, &m, &results()).unwrap();
        let expected = "<p><span data-cell=\"a\" data-source=\"cold-put-instance-a1.json#p50\" data-label=\"test instance\" data-result=\"r.tsv\">2.00 ms</span> \
<span class=\"x y\" data-cell=\"b\" data-source=\"cold-put-direct-a1.json#p95\" data-label=\"farm\" data-result=\"r.tsv\">20.0 ms</span> \
<span class=\"tbd\">[no cell]</span> <span class=\"tbd\" data-cell=\"z\">[no map]</span></p>";
        assert_eq!(out, expected);
        assert_eq!((report.filled, report.unmapped), (2, 1));
        // A cell left with a note keeps its class and gets no result file.
        let few = Results::from_results([result(CellId::ColdPut, "direct", 100, None)]);
        let (noted, _) = fill(
            "<i class=\"tbd\" data-cell=\"a\">?</i>",
            &one("a", "p99", "ms", "direct"),
            &few,
        )
        .unwrap();
        assert!(noted.contains("class=\"tbd\""), "{noted}");
        assert!(!noted.contains("data-result"), "{noted}");
    }

    /// Catches a cell the paper marks as design or code reading (`data-basis`) being given a
    /// measured number, a blocked cell that already names a result being rewritten, and the two
    /// being counted as unmapped or listed by the check.
    #[test]
    fn a_cell_with_a_basis_or_a_block_is_never_written() {
        let html = "<i data-cell=\"a\" data-basis=\"code-reading\">text</i>\
<i class=\"tbd\" data-cell=\"b\" data-blocked=\"not run\" data-result=\"r.tsv\">old</i>";
        let m = mappings(&format!(
            "[{},{}]",
            row("a", "p50", "ms", "instance"),
            row("b", "p50", "ms", "instance")
        ));
        let (out, report) = fill(html, &m, &results()).unwrap();
        assert_eq!(out, html);
        assert_eq!(
            report,
            FillReport {
                blocked: 2,
                ..FillReport::default()
            }
        );
        let findings = check(html, &[], &results()).unwrap();
        assert!(findings.is_empty(), "{findings:?}");
    }

    /// The rows of `tsv` that are not comments, as columns.
    fn rows(tsv: &ResultTsv) -> Vec<Vec<String>> {
        tsv.render()
            .lines()
            .filter(|l| l.contains('\t'))
            .map(|l| l.split('\t').map(str::to_owned).collect())
            .collect()
    }

    /// The numbers a text prints: runs of digits and points, a thousands comma removed.
    fn printed(text: &str) -> Vec<String> {
        text.split(|c: char| !matches!(c, '0'..='9' | '.' | ','))
            .filter(|t| t.starts_with(|c: char| c.is_ascii_digit()))
            .map(|t| t.replace(',', ""))
            .collect()
    }

    /// Catches a filled cell with no row behind its number, a row whose value is not the number
    /// printed (the check of the paper compares the printed number, as rounded), a row that does
    /// not name the cell, the source file, the passes and the arithmetic, and a noted cell that
    /// gets a row.
    #[test]
    fn every_filled_cell_has_a_row_with_the_number_it_prints() {
        let html = "<i data-cell=\"a\">?</i><i data-cell=\"b\">?</i><i data-cell=\"n\">?</i>";
        let m = mappings(&format!(
            "[{},{},{}]",
            row("a", "p50", "ms", "instance"),
            row("b", "throughput", "MB/s", "direct"),
            row("n", "p99", "ms", "direct"),
        ));
        let few = Results::from_results([
            result(CellId::ColdPut, "instance", 1_200, Some(80_000)),
            result(CellId::ColdPut, "direct", 100, None),
        ]);
        let mut t = tsv();
        let (out, report) = super::fill(html, &m, &few, &mut t).unwrap();
        assert_eq!((report.filled, report.noted), (2, 1));
        let rows = rows(&t);
        assert_eq!(rows.len(), 2, "{}", t.render());
        let a = &rows[0];
        assert_eq!(a[..4], ["derived", "fill:a:p50", "2.00", "ms"]);
        assert_eq!(a[5], "a");
        assert!(a[4].contains("cold-put-instance-a1.json#p50"), "{a:?}");
        assert!(a[4].contains("passes 1,2,3"), "{a:?}");
        assert!(a[4].contains("median"), "{a:?}");
        assert!(a[4].contains("2000"), "{a:?}");
        let b = &rows[1];
        assert_eq!(b[..4], ["derived", "fill:b:throughput", "2.00", "MB/s"]);
        assert_eq!(b[5], "b");
        assert!(b[4].contains("sum"), "{b:?}");
        // The number each cell prints is the value of its row.
        assert!(out.contains(">2.00 ms</i>"), "{out}");
        assert!(out.contains(">2.00 MB/s</i>"), "{out}");
        assert_eq!(printed("1,235 ms"), ["1235"]);
    }

    /// Catches a row value that differs from the printed text when the printed number is
    /// grouped or rounded.
    #[test]
    fn the_row_holds_the_number_as_printed() {
        let html = "<i data-cell=\"a\">?</i>";
        let mut t = tsv();
        let big = Results::from_results([result(CellId::ColdPut, "direct", 1_234_567, None)]);
        let (out, _) =
            super::fill(html, &one("a", "requests", "count", "direct"), &big, &mut t).unwrap();
        assert!(out.ends_with(">3,703,701</i>"), "{out}");
        let r = rows(&t);
        assert_eq!(r[0][2], "3703701");
        assert_eq!(r[0][3], "count");
        assert_eq!(printed("3,703,701"), [r[0][2].clone()]);
    }

    /// Catches a second run that changes the paper or the table, and a table that grows a row per
    /// run.
    #[test]
    fn a_second_run_gives_the_same_paper_and_table() {
        let html = "<p><b class=\"tbd\" data-cell=\"a\">[x]</b> <b data-cell=\"b\">?</b></p>";
        let m = mappings(&format!(
            "[{},{}]",
            row("a", "p50", "ms", "instance"),
            row("b", "p95", "ms", "direct")
        ));
        let mut t = tsv();
        let (once, _) = super::fill(html, &m, &results(), &mut t).unwrap();
        let first = t.render();
        let mut again = ResultTsv::parse("r.tsv", &first).unwrap();
        let (twice, _) = super::fill(&once, &m, &results(), &mut again).unwrap();
        assert_eq!(once, twice);
        assert_eq!(first, again.render());
        assert_eq!(rows(&again).len(), 2);
    }

    /// Catches a table rewritten whole: comments and rows of other owners stay where they are, and
    /// a changed number updates its row in place instead of adding one.
    #[test]
    fn the_table_keeps_other_rows_and_updates_its_own_in_place() {
        let old = "# kept comment\n\nraw\thand.row\t7\tms\tmeasured by hand\tz\n\
derived\tfill:a:p50\t9.99\tms\tstale\ta\nraw\tlast.row\t8\tms\tby hand\n";
        let mut t = ResultTsv::parse("r.tsv", old).unwrap();
        let html = "<i data-cell=\"a\">?</i>";
        super::fill(html, &one("a", "p50", "ms", "instance"), &results(), &mut t).unwrap();
        let text = t.render();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 5, "{text}");
        assert_eq!(lines[0], "# kept comment");
        assert_eq!(lines[1], "");
        assert_eq!(lines[2], "raw\thand.row\t7\tms\tmeasured by hand\tz");
        assert!(
            lines[3].starts_with("derived\tfill:a:p50\t2.00\tms\t"),
            "{text}"
        );
        assert_eq!(lines[4], "raw\tlast.row\t8\tms\tby hand");
        assert!(text.ends_with('\n'));
    }

    /// Catches a new table with no header and a table name that the paper's check would refuse.
    #[test]
    fn a_table_has_a_header_and_a_plain_name() {
        let t = ResultTsv::new("ok-1_a.tsv").unwrap();
        assert!(t.render().starts_with("# "), "{}", t.render());
        for bad in [
            "../x.tsv", "X.tsv", "a.txt", "a b.tsv", "", ".tsv.", "a/b.tsv",
        ] {
            let err = ResultTsv::new(bad).unwrap_err();
            assert!(matches!(err, FillError::TsvName(_)), "{bad}: {err}");
            assert!(ResultTsv::parse(bad, "").is_err(), "{bad}");
        }
        let err = ResultTsv::parse("r.tsv", "# c\nraw\tonly\n").unwrap_err();
        assert!(err.to_string().contains("line 2"), "{err}");
        let err = ResultTsv::parse("r.tsv", "raw\tx\tmany\n").unwrap_err();
        assert!(err.to_string().contains("not a number"), "{err}");
    }

    /// Catches a hand-filled cell (a result table this run did not write) being rewritten, and
    /// the refusal not naming the cell and the table; a cell naming the table being written, or
    /// an old `.json` name, is ours to rewrite; an unmapped hand-filled cell is left alone
    /// without a complaint.
    #[test]
    fn a_hand_filled_cell_is_refused_unless_the_table_is_ours() {
        let m = one("a", "p50", "ms", "instance");
        let hand = "<i data-cell=\"a\" data-result=\"hand.tsv\">by hand</i>";
        let err = super::fill(hand, &m, &results(), &mut tsv()).unwrap_err();
        let text = err.to_string();
        assert!(matches!(err, FillError::Foreign { .. }), "{text}");
        assert!(text.contains("\"a\""), "{text}");
        assert!(text.contains("hand.tsv"), "{text}");
        assert!(text.contains("r.tsv"), "{text}");
        let mut t = tsv();
        let ours = "<i data-cell=\"a\" data-result=\"r.tsv\">old</i>";
        let (out, _) = super::fill(ours, &m, &results(), &mut t).unwrap();
        assert!(out.ends_with(">2.00 ms</i>"), "{out}");
        let json = "<i data-cell=\"a\" data-result=\"cold-put-x-a1.json\">old</i>";
        let (out, _) = super::fill(json, &m, &results(), &mut tsv()).unwrap();
        assert!(out.contains("data-result=\"r.tsv\""), "{out}");
        assert!(out.ends_with(">2.00 ms</i>"), "{out}");
        let (out, report) = super::fill(hand, &[], &results(), &mut tsv()).unwrap();
        assert_eq!((out.as_str(), report.unmapped), (hand, 1));
        // A mapped hand-filled cell with no figure is left as it is, not refused.
        let none = one("a", "p50", "ms", "no-such-arm");
        let (out, report) = super::fill(hand, &none, &results(), &mut tsv()).unwrap();
        assert_eq!(report.noted, 1);
        assert!(out.contains("data-result=\"hand.tsv\""), "{out}");
    }

    /// Catches a check that does not look in the table: a cell that cites this table but has no
    /// row naming it is a finding; a cell citing another table is not ours to look up.
    #[test]
    fn check_follows_data_result_to_the_table() {
        let html = "<i data-cell=\"a\" data-result=\"r.tsv\">1</i>\
<i data-cell=\"b\" data-result=\"r.tsv\">2</i>\
<i data-cell=\"c\" data-result=\"phase-b.tsv\">3</i>";
        let m = mappings(&format!(
            "[{},{},{}]",
            row("a", "p50", "ms", "instance"),
            row("b", "p50", "ms", "instance"),
            row("c", "p50", "ms", "instance")
        ));
        let t = ResultTsv::parse("r.tsv", "derived\tfill:a:p50\t2.00\tms\thow\ta\n").unwrap();
        let findings = super::check(html, &m, &results(), Some(&t)).unwrap();
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert!(
            findings[0].contains("\"b\" has no row naming it in r.tsv"),
            "{findings:?}"
        );
        assert!(super::check(html, &m, &results(), None).unwrap().is_empty());
    }

    /// Catches a class list rewritten when it holds no `tbd`: a filled cell keeps the classes it
    /// had, byte for byte.
    #[test]
    fn a_class_without_tbd_is_left_as_written() {
        let html = "<b class='how  now' data-cell=\"a\">?</b>";
        let (out, _) = fill(html, &one("a", "p50", "ms", "instance"), &results()).unwrap();
        assert!(
            out.starts_with("<b class='how  now' data-cell=\"a\" "),
            "{out}"
        );
    }
}
