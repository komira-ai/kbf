//! The gates of scenario B0 (docs/design/kbf-cicd.md, section 4.4), evaluated from what
//! the run measured. Pure: no process, no clock. Every gate is a row of the result
//! document (`name`, `measured`, `threshold`, `verdict`, `started`, `finished`).
//!
//! The rule that runs through all of them is fail closed: a metric that is not there is
//! not zero. A series absent from the first scrape, or from any later one, fails its
//! gate, so a metric the frontend stops exporting cannot turn a gate green, and a run
//! with no builds, or too few scrapes to take a difference, passes nothing.

use serde::Serialize;
use serde_json::{Value, json};

use crate::scrape::{Sample, Scrapes};

/// The failed-Execute counter (no by-class counter exists yet).
pub const EXECUTE_ERRORS: &str = "kbf_execute_errors_total";
/// Blobs marked lost now.
pub const LOST_BLOBS: &str = "kbf_cache_lost_blobs";
/// Blobs ever marked lost.
pub const BLOB_LOSSES: &str = "kbf_cache_blob_losses_total";

/// The fewest of a second's scrapes that must have succeeded, as a fraction.
pub const MIN_SCRAPE_COVERAGE: f64 = 0.9;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum Verdict {
    #[serde(rename = "PASS")]
    Pass,
    #[serde(rename = "FAIL")]
    Fail,
}

/// One row of the result document's `gates`.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Gate {
    pub name: String,
    pub measured: Value,
    pub threshold: String,
    pub verdict: Verdict,
    pub started: String,
    pub finished: String,
    /// Why it failed, or what was measured, in words.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// One build of the load.
#[derive(Clone, Debug, PartialEq)]
pub struct BuildResult {
    /// What it built (`compile`, `pause`, `upload`).
    pub kind: String,
    /// Exit status 0, inside its time bound.
    pub ok: bool,
    /// Actions that failed in its event log, or why the log could not be read.
    pub failed_actions: Result<u64, String>,
}

/// What the scenario measured.
pub struct Evidence<'a> {
    pub builds: &'a [BuildResult],
    pub scrapes: &'a Scrapes,
    /// Roles that ended during the load without being ended: role and how.
    pub exited_during_load: &'a [(String, String)],
    /// Roles' ends when the farm was stopped: role and how (`exit 0` is clean).
    pub stopped: &'a [(String, String)],
    /// Whether the scenario's wall-clock bound was hit.
    pub hung: bool,
    /// The window the load ran in, as two timestamps, and its length in seconds.
    pub load: (String, String),
    pub load_secs: f64,
    /// The window the farm was stopped in.
    pub stop: (String, String),
    /// How often the scraper scrapes, in seconds.
    pub scrape_every_secs: f64,
}

/// A series' values in every scrape, or why one is missing.
fn series(scrapes: &Scrapes, name: &str) -> Result<Vec<f64>, String> {
    if scrapes.samples.len() < 2 {
        return Err(format!(
            "{} scrape(s): a difference needs two",
            scrapes.samples.len()
        ));
    }
    scrapes
        .samples
        .iter()
        .enumerate()
        .map(|(i, s): (usize, &Sample)| {
            s.value(name).ok_or_else(|| {
                format!(
                    "{name} is absent from scrape {i} of {}",
                    scrapes.samples.len()
                )
            })
        })
        .collect()
}

impl Evidence<'_> {
    fn row(
        &self,
        name: &str,
        measured: Value,
        threshold: &str,
        pass: bool,
        detail: Option<String>,
    ) -> Gate {
        let (started, finished) = if matches!(name, "clean_stop") {
            self.stop.clone()
        } else {
            self.load.clone()
        };
        Gate {
            name: name.to_owned(),
            measured,
            threshold: threshold.to_owned(),
            verdict: if pass { Verdict::Pass } else { Verdict::Fail },
            started,
            finished,
            detail,
        }
    }

    /// A gate on how a series moved over the run.
    fn series_gate(
        &self,
        name: &str,
        metric: &str,
        threshold: &str,
        reduce: impl Fn(&[f64]) -> f64,
    ) -> Gate {
        match series(self.scrapes, metric) {
            Ok(values) => {
                let measured = reduce(&values);
                self.row(
                    name,
                    json!(measured),
                    threshold,
                    measured == 0.0,
                    Some(format!("{metric} over {} scrapes", values.len())),
                )
            }
            Err(why) => self.row(name, Value::Null, threshold, false, Some(why)),
        }
    }

    /// Every gate, in the order of the design.
    #[must_use]
    pub fn gates(&self) -> Vec<Gate> {
        let builds = u64::try_from(self.builds.len()).unwrap_or(u64::MAX);
        let failed_builds = self.builds.iter().filter(|b| !b.ok).count();
        let failed_actions: u64 = self
            .builds
            .iter()
            .map(|b| b.failed_actions.as_ref().map_or(1, |n| *n))
            .sum();
        let unreadable: Vec<String> = self
            .builds
            .iter()
            .filter_map(|b| b.failed_actions.as_ref().err().cloned())
            .collect();
        let coverage = if self.load_secs > 0.0 {
            self.scrapes.samples.len() as f64 / (self.load_secs / self.scrape_every_secs)
        } else {
            0.0
        };
        let clean = |ends: &[(String, String)]| {
            ends.iter()
                .filter(|(_, how)| how != "exit 0")
                .map(|(role, how)| format!("{role}: {how}"))
                .collect::<Vec<_>>()
        };
        let unclean = clean(self.stopped);
        let exited: Vec<String> = self
            .exited_during_load
            .iter()
            .map(|(role, how)| format!("{role}: {how}"))
            .collect();
        vec![
            self.row("builds_run", json!(builds), "at least 1", builds >= 1, None),
            self.row(
                "builds_failed",
                json!(failed_builds),
                "0",
                failed_builds == 0,
                None,
            ),
            self.row(
                "actions_failed",
                json!(failed_actions),
                "0",
                failed_actions == 0,
                (!unreadable.is_empty()).then(|| {
                    format!("event logs unreadable (each counts as a failure): {unreadable:?}")
                }),
            ),
            self.series_gate("execute_errors", EXECUTE_ERRORS, "delta 0", |v| {
                v[v.len() - 1] - v[0]
            }),
            self.series_gate(
                "lost_blobs_gauge",
                LOST_BLOBS,
                "max 0 in every scrape",
                |v| v.iter().copied().fold(f64::MIN, f64::max),
            ),
            self.series_gate("blob_losses", BLOB_LOSSES, "delta 0", |v| {
                v[v.len() - 1] - v[0]
            }),
            self.row(
                "scrape_coverage",
                json!(coverage),
                "at least 0.9",
                coverage >= MIN_SCRAPE_COVERAGE,
                self.scrapes.last_error.clone(),
            ),
            self.row(
                "processes_alive",
                json!(exited.len()),
                "0",
                exited.is_empty(),
                (!exited.is_empty()).then(|| exited.join("; ")),
            ),
            self.row(
                "clean_stop",
                json!(unclean.len()),
                "0",
                unclean.is_empty(),
                (!unclean.is_empty()).then(|| unclean.join("; ")),
            ),
            self.row("hang", json!(self.hung), "false", !self.hung, None),
        ]
    }
}

/// Whether every gate passed (and there is at least one).
#[must_use]
pub fn all_pass(gates: &[Gate]) -> bool {
    !gates.is_empty() && gates.iter().all(|g| g.verdict == Verdict::Pass)
}

#[cfg(test)]
#[path = "gates_tests.rs"]
mod tests;
