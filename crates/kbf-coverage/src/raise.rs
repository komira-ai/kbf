//! Floors only rise: `raise` lifts them to a measurement, and `no-lowering` is the CI
//! check that a pull request did not lower one.
//!
//! A crate's threshold for a metric is its floor, else 99.00% ([`Floor::threshold`]),
//! so adding a floor lowers the threshold from 99.00 and removing one raises it. That
//! makes both directions mechanical:
//!
//! - [`plan`] touches only floors that exist. A floor moves to the measured percentage
//!   (floored to hundredths) when that is higher, stays when the measurement is lower
//!   (the gate fails it; a floor does not follow coverage down), and is removed once
//!   the measurement reaches 99.00% or there is nothing to measure, since the default
//!   threshold is then the stricter one. A line left with no floor goes. A floors line
//!   for a crate the workspace lacks goes. `plan` never adds a floor.
//! - [`lowered`] compares two floors files by threshold: it reports every crate and
//!   metric whose threshold in `head` is under `base`'s. A new floor is such a case.

use std::collections::BTreeMap;
use std::fmt::{self, Write as _};
use std::io;
use std::path::{Path, PathBuf};

use crate::floors::{Floor, Floors};
use crate::gate::Metric;
use crate::{CrateCoverage, Percent};

/// One floor that [`plan`] moved or removed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    pub krate: String,
    pub metric: Metric,
    pub from: Percent,
    /// `None`: the floor is removed and the metric must reach 99.00%.
    pub to: Option<Percent>,
}

impl fmt::Display for Change {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} {}: {} -> {}",
            self.krate,
            self.metric,
            self.from,
            self.to
                .map_or_else(|| "removed (99.00 applies)".to_owned(), |p| p.to_string())
        )
    }
}

/// The floors after raising to `measured`, and what moved.
pub fn plan(floors: &Floors, measured: &BTreeMap<String, CrateCoverage>) -> (Floors, Vec<Change>) {
    let mut out = Floors::new();
    let mut changes = Vec::new();
    for (krate, floor) in floors {
        let Some(cov) = measured.get(krate) else {
            for metric in [Metric::Lines, Metric::Branches] {
                if let Some(from) = floor.get(metric) {
                    changes.push(Change {
                        krate: krate.clone(),
                        metric,
                        from,
                        to: None,
                    });
                }
            }
            continue;
        };
        let mut raised = Floor::default();
        for metric in [Metric::Lines, Metric::Branches] {
            let Some(from) = floor.get(metric) else {
                continue;
            };
            let counts = match metric {
                Metric::Lines => cov.lines,
                Metric::Branches => cov.branches,
            };
            let now = counts.percent().filter(|p| *p < Percent::TARGET);
            let to = now.map(|p| p.max(from));
            if to != Some(from) {
                changes.push(Change {
                    krate: krate.clone(),
                    metric,
                    from,
                    to,
                });
            }
            match metric {
                Metric::Lines => raised.lines = to,
                Metric::Branches => raised.branches = to,
            }
        }
        if raised != Floor::default() {
            out.insert(krate.clone(), raised);
        }
    }
    (out, changes)
}

/// A threshold that is lower in the head's floors file than in the base's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lowered {
    pub krate: String,
    pub metric: Metric,
    pub base: Percent,
    pub head: Percent,
}

impl fmt::Display for Lowered {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} {}: threshold {} -> {} ({})",
            self.krate,
            self.metric,
            self.base,
            self.head,
            if self.base == Percent::TARGET {
                "a floor was added; a crate under 99% needs tests, not a floor"
            } else {
                "a floor may only rise"
            }
        )
    }
}

/// Every crate and metric whose threshold is lower in `head` than in `base`.
pub fn lowered(base: &Floors, head: &Floors) -> Vec<Lowered> {
    let mut crates: Vec<&String> = base.keys().chain(head.keys()).collect();
    crates.sort();
    crates.dedup();
    let mut out = Vec::new();
    for krate in crates {
        let at = |f: &Floors| f.get(krate).copied().unwrap_or_default();
        for metric in [Metric::Lines, Metric::Branches] {
            let (b, h) = (at(base).threshold(metric), at(head).threshold(metric));
            if h < b {
                out.push(Lowered {
                    krate: krate.clone(),
                    metric,
                    base: b,
                    head: h,
                });
            }
        }
    }
    out
}

/// Replaces `path` with `text` whole: written to `<path>.raise-tmp` beside it, then
/// renamed over it, so an interrupted run leaves the old file or the new one, never
/// part of one. A failed write or rename removes the temporary file.
pub fn replace(path: &Path, text: &str) -> io::Result<()> {
    replace_with(path, text, |from, to| std::fs::rename(from, to))
}

/// [`replace`], renaming with `rename` (a test passes one that fails).
pub fn replace_with(
    path: &Path,
    text: &str,
    rename: impl FnOnce(&Path, &Path) -> io::Result<()>,
) -> io::Result<()> {
    let mut temp = path.as_os_str().to_owned();
    temp.push(".raise-tmp");
    let temp = PathBuf::from(temp);
    let result = std::fs::write(&temp, text).and_then(|()| rename(&temp, path));
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result
}

/// `plan`'s changes as the lines a check prints, or `None` when there are none.
pub fn advice(changes: &[Change], command: &str) -> Option<String> {
    if changes.is_empty() {
        return None;
    }
    let mut out = String::from("floors that can rise (not a failure):\n");
    for c in changes {
        let _ = writeln!(out, "  {c}");
    }
    let _ = writeln!(out, "raise them with: {command}");
    Some(out)
}

#[cfg(test)]
mod tests;
