//! The gates of scenario B0 (docs/design/kbf-cicd.md, section 4.4), evaluated from what
//! the run measured. Pure: no process, no clock. Every gate is a row of the result
//! document (`name`, `measured`, `threshold`, `verdict`, `started`, `finished`).
//!
//! The rule that runs through all of them is fail closed: a metric that is not there is
//! not zero. A series absent from the first scrape, or from any later one, fails its
//! gate, so a metric the frontend stops exporting cannot turn a gate green, and a run
//! with no builds, or too few scrapes to take a difference, passes nothing. A value that is
//! not a finite number (`NaN`, an infinity) fails its gate too, and a counter that falls
//! between two scrapes was reset (a restart), which no delta of its ends can show.

use serde::Serialize;
use serde_json::{Value, json};

use crate::scrape::{Sample, Scrapes, family};

/// The failed-Execute counter: the sum of its `cause` series (status, client_abort, dropped).
pub const EXECUTE_ERRORS: &str = "kbf_execute_errors_total";
/// The counter of operations finished as failed, one series per failure class
/// (`class="farm"` and the rest of [`FAILURE_CLASSES`]).
pub const FAILED_ACTIONS: &str = "kbf_execute_failed_actions_total";
/// The values of the `class` label of [`FAILED_ACTIONS`]: the four classes of
/// docs/design/failure-classes.md, section 1.
pub const FAILURE_CLASSES: [&str; 4] = ["farm", "request", "action", "ambiguous"];
/// Blobs marked lost now.
pub const LOST_BLOBS: &str = "kbf_cache_lost_blobs";
/// Blobs ever marked lost.
pub const BLOB_LOSSES: &str = "kbf_cache_blob_losses_total";

/// Registered storage members that are not `ACTIVE`, exported only while known.
pub const MEMBERS_NOT_ACTIVE: &str = "kbf_members_not_active";
/// Each storage member's boot counter (`kbf_member_boot{member}`): a restart raises it.
pub const MEMBER_BOOT: &str = "kbf_member_boot";
/// The most members that may be down at once.
pub const MAX_MEMBERS_DOWN: f64 = 1.0;

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
    /// Each storage node's `/metrics`, by node name: the lost-blob series are the nodes',
    /// the frontend holds no cache.
    pub nodes: &'a [(String, Scrapes)],
    /// Roles that ended during the load without being ended: role and how.
    pub exited_during_load: &'a [(String, String)],
    /// Roles' ends when the farm was stopped: role and how (`exit 0` is clean).
    pub stopped: &'a [(String, String)],
    /// Whether the farm was still whole when the load and its settling ended (every node
    /// connected, every storage member active), or what it lacked.
    pub farm_whole: &'a Result<(), String>,
    /// Whether the scenario's wall-clock bound was hit.
    pub hung: bool,
    /// How many times each storage member may have restarted during the load (0 for a
    /// run that restarts nothing; 1 for a rolling update).
    pub restarts_allowed: u32,
    /// The window the load ran in, as two timestamps, and its length in seconds.
    pub load: (String, String),
    pub load_secs: f64,
    /// The window the farm was stopped in.
    pub stop: (String, String),
    /// How often the scraper scrapes, in seconds.
    pub scrape_every_secs: f64,
}

/// A series' values in every scrape, or why one is missing. `name` is a family (summed over
/// its labels) or, with braces, one series' whole key (`name{class="farm"}`).
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
            let read = if name.contains('{') {
                s.series.get(name).copied()
            } else {
                s.value(name)
            };
            let value = read.ok_or_else(|| {
                format!(
                    "{name} is absent from scrape {i} of {}",
                    scrapes.samples.len()
                )
            })?;
            if value.is_finite() {
                Ok(value)
            } else {
                Err(format!(
                    "{name} is {value} in scrape {i}: not a finite number"
                ))
            }
        })
        .collect()
}

/// How much a counter grew over the scrapes `v` (at least two): the last less the first, and
/// an error if it fell between any two, which is a reset: the counter's process restarted,
/// and the difference of the ends is no longer the number of events.
fn counter_delta(v: &[f64]) -> Result<f64, String> {
    if let Some(i) = (1..v.len()).find(|i| v[*i] < v[*i - 1]) {
        return Err(format!(
            "went down from {} to {} between scrapes {} and {i}: reset",
            v[i - 1],
            v[i],
            i - 1
        ));
    }
    Ok(v[v.len() - 1] - v[0])
}

/// The most restarts any one member shows over `scrapes`: each member's boot counter
/// ([`MEMBER_BOOT`]) rises by one per restart, so its growth is the number of restarts.
/// Every member of the first scrape must be in every later one; a boot that falls is an
/// error ([`counter_delta`]), as is one that is not a finite number.
fn member_restarts(scrapes: &Scrapes) -> Result<f64, String> {
    let count = scrapes.samples.len();
    if count < 2 {
        return Err(format!("{count} scrape(s): a difference needs two"));
    }
    let members: Vec<&String> = scrapes.samples[0]
        .series
        .keys()
        .filter(|key| crate::scrape::family(key) == MEMBER_BOOT)
        .collect();
    if members.is_empty() {
        return Err(format!("{MEMBER_BOOT} is absent from scrape 0 of {count}"));
    }
    let mut most = 0.0;
    for key in members {
        let mut boots = Vec::with_capacity(count);
        for (i, sample) in scrapes.samples.iter().enumerate() {
            let boot = *sample
                .series
                .get(key)
                .ok_or_else(|| format!("{key} is absent from scrape {i} of {count}"))?;
            if !boot.is_finite() {
                return Err(format!(
                    "{key} is {boot} in scrape {i}: not a finite number"
                ));
            }
            boots.push(boot);
        }
        most = f64::max(
            most,
            counter_delta(&boots).map_err(|e| format!("{key} {e}"))?,
        );
    }
    Ok(most)
}

/// How each series of the family `name` grew from the first scrape to the last, as
/// `key +delta`; a series absent from the last is left out. For scrapes [`series`] read:
/// at least two.
fn series_deltas(scrapes: &Scrapes, name: &str) -> Vec<String> {
    let first = &scrapes.samples[0];
    let last = &scrapes.samples[scrapes.samples.len() - 1];
    first
        .series
        .iter()
        .filter(|(key, _)| family(key) == name)
        .filter_map(|(key, was)| {
            let now = last.series.get(key)?;
            Some(format!("{key} +{}", now - was))
        })
        .collect()
}

/// The largest value of the scrapes `v`.
fn gauge_max(v: &[f64]) -> Result<f64, String> {
    Ok(v.iter().copied().fold(f64::MIN, f64::max))
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
        reduce: impl Fn(&[f64]) -> Result<f64, String>,
    ) -> Gate {
        match series(self.scrapes, metric).and_then(|values| Ok((reduce(&values)?, values.len()))) {
            Ok((measured, scrapes)) => self.row(
                name,
                json!(measured),
                threshold,
                measured == 0.0,
                Some(format!("{metric} over {scrapes} scrapes")),
            ),
            Err(why) => self.row(name, Value::Null, threshold, false, Some(why)),
        }
    }

    /// The gate on the failed-action counter of every class: the delta of each class's series
    /// over the run, which is 0 when the load caused no failure. A class whose series is absent
    /// from any scrape fails it, as for every series.
    fn failed_actions_gate(&self) -> Gate {
        let mut measured = serde_json::Map::new();
        let mut why = Vec::new();
        for class in FAILURE_CLASSES {
            let key = format!("{FAILED_ACTIONS}{{class=\"{class}\"}}");
            match series(self.scrapes, &key).and_then(|values| counter_delta(&values)) {
                Ok(delta) => {
                    measured.insert(class.to_owned(), json!(delta));
                    if delta != 0.0 {
                        why.push(format!("{class}: delta {delta}"));
                    }
                }
                Err(e) => {
                    measured.insert(class.to_owned(), Value::Null);
                    why.push(format!("{class}: {e}"));
                }
            }
        }
        self.row(
            "failed_actions_by_class",
            Value::Object(measured),
            "delta 0 in every class",
            why.is_empty(),
            (!why.is_empty()).then(|| why.join("; ")),
        )
    }

    /// The execute-errors gate: the sum of the `cause` series must not move; when it does,
    /// the detail names how each series moved, so the report says which cause turned it red.
    fn execute_errors_gate(&self) -> Gate {
        let mut gate = self.series_gate("execute_errors", EXECUTE_ERRORS, "delta 0", counter_delta);
        // A gate that moved has causes to name; one with no measure (a series absent, too
        // few scrapes) has none.
        if gate.measured.as_f64().is_some_and(|moved| moved != 0.0) {
            let moved = series_deltas(self.scrapes, EXECUTE_ERRORS);
            let why = gate.detail.take().unwrap_or_default();
            gate.detail = Some(format!("{why}; by series: {}", moved.join(", ")));
        }
        gate
    }

    /// A gate on how a series of the storage nodes moved over the run: `reduce` of each
    /// node's scrapes, summed over the nodes. Fails closed on no node at all, and on a
    /// node whose series is absent from any of its scrapes, whatever the others read.
    fn nodes_gate(
        &self,
        name: &str,
        metric: &str,
        threshold: &str,
        reduce: impl Fn(&[f64]) -> Result<f64, String>,
    ) -> Gate {
        let reads: Vec<(&str, Result<f64, String>)> = self
            .nodes
            .iter()
            .map(|(node, scrapes)| {
                let read = series(scrapes, metric).and_then(|values| reduce(&values));
                (node.as_str(), read)
            })
            .collect();
        let why: Vec<String> = reads
            .iter()
            .filter_map(|(node, read)| read.as_ref().err().map(|e| format!("{node}: {e}")))
            .collect();
        if reads.is_empty() || !why.is_empty() {
            let why = if reads.is_empty() {
                "no storage node was scraped".to_owned()
            } else {
                why.join("; ")
            };
            return self.row(name, Value::Null, threshold, false, Some(why));
        }
        let measured: f64 = reads
            .iter()
            .filter_map(|(_, read)| read.as_ref().ok())
            .sum();
        self.row(
            name,
            json!(measured),
            threshold,
            measured == 0.0,
            Some(format!(
                "{metric} summed over {} storage nodes",
                reads.len()
            )),
        )
    }

    /// Never more than [`MAX_MEMBERS_DOWN`] storage members out of `ACTIVE` in any scrape.
    fn members_down_gate(&self) -> Gate {
        let threshold = "at most 1 member not active in every scrape";
        match series(self.scrapes, MEMBERS_NOT_ACTIVE).and_then(|v| gauge_max(&v)) {
            Ok(most) => self.row(
                "members_down_at_most_one",
                json!(most),
                threshold,
                most <= MAX_MEMBERS_DOWN,
                Some(format!("{MEMBERS_NOT_ACTIVE} over the scrapes")),
            ),
            Err(why) => self.row(
                "members_down_at_most_one",
                Value::Null,
                threshold,
                false,
                Some(why),
            ),
        }
    }

    /// No storage member restarted more often than `restarts_allowed` times.
    fn restarts_gate(&self) -> Gate {
        let threshold = format!(
            "at most {} restart(s) of each member",
            self.restarts_allowed
        );
        match member_restarts(self.scrapes) {
            Ok(most) => self.row(
                "member_restarts",
                json!(most),
                &threshold,
                most <= f64::from(self.restarts_allowed),
                Some(format!("growth of {MEMBER_BOOT}, the most of any member")),
            ),
            Err(why) => self.row("member_restarts", Value::Null, &threshold, false, Some(why)),
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
        // The scrapes that gave a page, of the scrapes tried, or of the scrapes the load
        // should have seen if the scraper tried fewer (one stuck on a wedged frontend): so
        // neither a failed scrape nor a missed one is made up for by the scrapes of the
        // settling window.
        let coverage = if self.load_secs > 0.0 {
            let got = self.scrapes.samples.len() as f64;
            let tried = got + self.scrapes.failed as f64;
            got / tried.max(self.load_secs / self.scrape_every_secs)
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
            self.execute_errors_gate(),
            self.failed_actions_gate(),
            self.nodes_gate(
                "lost_blobs_gauge",
                LOST_BLOBS,
                "max 0 in every scrape of every storage node",
                gauge_max,
            ),
            self.nodes_gate(
                "blob_losses",
                BLOB_LOSSES,
                "delta 0 on every storage node",
                counter_delta,
            ),
            self.members_down_gate(),
            self.restarts_gate(),
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
                "farm_whole",
                json!(self.farm_whole.is_ok()),
                "true: every node connected, every storage member active",
                self.farm_whole.is_ok(),
                self.farm_whole.as_ref().err().cloned(),
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
