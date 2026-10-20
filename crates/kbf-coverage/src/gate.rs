//! Per-crate totals, the comparison with the thresholds, and the table CI prints.
//!
//! A crate is a directory under `<root>/crates` holding a `Cargo.toml` (the workspace's
//! `members = ["crates/*"]`). A crate whose code no test binary instruments (an empty
//! skeleton) has no records and nothing to measure; it is still listed, with `-`.
//!
//! A crate passes when its line coverage and its branch coverage each meet the
//! metric's threshold: the floor the floors file lists for it, else 99.00%
//! ([`Floor::threshold`]). A metric with nothing to measure (no branches, say) passes.
//! The one exception: a metric with a listed floor and nothing to measure fails
//! ([`Status::NoData`]), because a crate whose tests stopped being instrumented would
//! otherwise slip past its floor; the floor is removed with the code. A floors line
//! for a crate the workspace does not have fails too ([`Status::Stale`]).
//!
//! A crate that is not listed and has no records at all passes; this gate cannot see
//! tests that stopped being instrumented for such a crate (`[lib] test = false`).

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::{self, Write as _};
use std::io;
use std::path::{Path, PathBuf};

use crate::floors::{Floor, Floors};
use crate::lcov::FileRecord;
use crate::{Counts, CrateCoverage, Percent, show};

/// The workspace's crates: the directories under `<root>/crates` with a `Cargo.toml`.
pub fn workspace_crates(root: &Path) -> io::Result<BTreeSet<String>> {
    let mut out = BTreeSet::new();
    for entry in std::fs::read_dir(root.join("crates"))? {
        let entry = entry?;
        if entry.path().join("Cargo.toml").is_file() {
            out.insert(entry.file_name().to_string_lossy().into_owned());
        }
    }
    Ok(out)
}

/// A tracefile source that is not inside a workspace crate.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0} is not under <root>/crates/<crate>/ for any workspace crate")]
pub struct Unattributed(pub PathBuf);

/// Sums the records per crate. Every crate in `crates` gets an entry, zero when no
/// record names it; a record outside every crate is an error rather than dropped.
pub fn by_crate(
    root: &Path,
    crates: &BTreeSet<String>,
    files: &[FileRecord],
) -> Result<BTreeMap<String, CrateCoverage>, Unattributed> {
    let base = root.join("crates");
    let mut out: BTreeMap<String, CrateCoverage> = crates
        .iter()
        .map(|c| (c.clone(), CrateCoverage::default()))
        .collect();
    for f in files {
        let total = f
            .path
            .strip_prefix(&base)
            .ok()
            .filter(|rest| rest.components().count() > 1)
            .and_then(|rest| rest.iter().next())
            .and_then(|name| out.get_mut(name.to_str()?));
        let Some(total) = total else {
            return Err(Unattributed(f.path.clone()));
        };
        *total = total.plus(f.coverage);
    }
    Ok(out)
}

/// Lines or branches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Metric {
    Lines,
    Branches,
}

impl fmt::Display for Metric {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Metric::Lines => "lines",
            Metric::Branches => "branches",
        })
    }
}

/// How one crate stands against its thresholds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    /// Every metric meets its threshold.
    Ok,
    /// These metrics are under their thresholds: fails.
    Below(Vec<Metric>),
    /// These metrics have a floor and nothing was measured: fails.
    NoData(Vec<Metric>),
    /// A floors line for a crate the workspace does not have: fails.
    Stale,
}

impl Status {
    pub fn fails(&self) -> bool {
        !matches!(self, Status::Ok)
    }
}

impl fmt::Display for Status {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let names = |m: &[Metric]| {
            m.iter()
                .map(Metric::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        };
        match self {
            Status::Ok => f.write_str("ok"),
            Status::Below(m) => write!(f, "BELOW THRESHOLD ({})", names(m)),
            Status::NoData(m) => write!(
                f,
                "FLOOR BUT NOTHING MEASURED ({}); remove the floor",
                names(m)
            ),
            Status::Stale => f.write_str("NOT IN WORKSPACE"),
        }
    }
}

/// One crate: what was measured (`None` if it is not in the workspace) and its floors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub krate: String,
    pub measured: Option<CrateCoverage>,
    pub floor: Floor,
}

impl Row {
    fn counts(&self, metric: Metric) -> Option<Counts> {
        self.measured.map(|m| match metric {
            Metric::Lines => m.lines,
            Metric::Branches => m.branches,
        })
    }

    pub fn status(&self) -> Status {
        if self.measured.is_none() {
            return Status::Stale;
        }
        let (mut below, mut no_data) = (Vec::new(), Vec::new());
        for metric in [Metric::Lines, Metric::Branches] {
            let counts = self.counts(metric).unwrap_or_default();
            if counts.found() == 0 {
                if self.floor.get(metric).is_some() {
                    no_data.push(metric);
                }
            } else if !counts.meets(self.floor.threshold(metric)) {
                below.push(metric);
            }
        }
        if !below.is_empty() {
            Status::Below(below)
        } else if !no_data.is_empty() {
            Status::NoData(no_data)
        } else {
            Status::Ok
        }
    }
}

/// Every crate in the workspace or the floors file, in name order.
pub fn rows(measured: &BTreeMap<String, CrateCoverage>, floors: &Floors) -> Vec<Row> {
    let names: BTreeSet<&String> = measured.keys().chain(floors.keys()).collect();
    names
        .into_iter()
        .map(|k| Row {
            krate: k.clone(),
            measured: measured.get(k).copied(),
            floor: floors.get(k).copied().unwrap_or_default(),
        })
        .collect()
}

/// `hit/found`, the percentage and the threshold for one metric; `-` for the first
/// two where nothing was measured, as the floors file writes it.
fn cells(c: Option<Counts>, threshold: Percent) -> [String; 3] {
    let Some(c) = c.filter(|c| c.found() > 0) else {
        return ["-".into(), "-".into(), threshold.to_string()];
    };
    [
        format!("{}/{}", c.hit(), c.found()),
        show(c.percent()),
        threshold.to_string(),
    ]
}

/// The report as a Markdown table (readable in a log, rendered in a job summary),
/// with a workspace total and a closing verdict line.
pub fn render(rows: &[Row]) -> String {
    let mut out = String::new();
    out.push_str(
        "| crate | lines | line % | line threshold | branches | branch % | branch threshold | status |\n\
         |---|---:|---:|---:|---:|---:|---:|---|\n",
    );
    let mut line = |name: &str, m: Option<CrateCoverage>, f: Floor, status: String| {
        let [l, lp, lt] = cells(m.map(|m| m.lines), f.threshold(Metric::Lines));
        let [b, bp, bt] = cells(m.map(|m| m.branches), f.threshold(Metric::Branches));
        // Writing to a String cannot fail.
        let _ = writeln!(
            out,
            "| {name} | {l} | {lp} | {lt} | {b} | {bp} | {bt} | {status} |"
        );
    };
    let mut total = CrateCoverage::default();
    let mut failing = 0;
    for r in rows {
        let status = r.status();
        failing += usize::from(status.fails());
        total = total.plus(r.measured.unwrap_or_default());
        line(&r.krate, r.measured, r.floor, status.to_string());
    }
    line(
        "**workspace**",
        Some(total),
        Floor::default(),
        String::new(),
    );
    let _ = write!(
        out,
        "\n{}\n",
        if failing == 0 {
            "coverage gate: PASS".to_owned()
        } else {
            format!("coverage gate: FAIL ({failing} crate(s) need attention; see status)")
        }
    );
    out
}

#[cfg(test)]
mod tests;
