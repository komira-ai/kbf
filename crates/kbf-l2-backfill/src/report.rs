//! What a run reports: the counts, as JSON and as lines an operator reads.

use serde::Serialize;

use crate::cli::Mode;

/// The most trees listed in a report's problems.
pub const MAX_PROBLEMS: usize = 100;

/// A tree that was not done, and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Problem {
    /// The tree's id, in hex.
    pub tree: String,
    /// `lost` (an object the list names is not in the store; the row is kept) or `failed` (a
    /// call failed; the row is kept).
    pub kind: &'static str,
    /// What the engine or the store said.
    pub detail: String,
}

/// What the backfill pass did.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize)]
pub struct Backfill {
    /// `--within-days`.
    pub within_days: u32,
    /// The unlisted trees handled.
    pub trees_seen: u64,
    /// The trees whose sidecar was written (an apply).
    pub written: u64,
    /// The trees whose list is in the store, in a dry run: an apply would handle them.
    pub would_write: u64,
    /// The trees whose list the store no longer has; in an apply their unlisted row is dropped.
    pub gone: u64,
    /// The trees with an object the store does not have; their row is kept.
    pub lost: u64,
    /// The trees that failed for another reason; their row is kept.
    pub failed: u64,
    /// The objects read for their cells.
    pub objects_read: u64,
    /// The bytes read for them.
    pub bytes_read: u64,
    /// Whether the unlisted trees were all handled (and not stopped at `--limit-trees`).
    pub finished: bool,
    /// The first [`MAX_PROBLEMS`] trees that were lost or failed.
    pub problems: Vec<Problem>,
}

impl Backfill {
    /// Notes a tree that was not done.
    pub fn problem(&mut self, tree: &str, kind: &'static str, detail: String) {
        if self.problems.len() < MAX_PROBLEMS {
            self.problems.push(Problem {
                tree: tree.to_owned(),
                kind,
                detail,
            });
        }
    }
}

/// What the sample did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Audit {
    /// The seed used, given or taken from the clock: pass it as `--audit-seed` to take the
    /// same sample again.
    pub seed: u64,
    /// `--audit-keys`.
    pub requested: usize,
    /// The packs the index holds live.
    pub live_packs: u64,
    /// The keys written.
    pub written: usize,
}

/// The report of a run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Report {
    /// `dry-run` or `apply`.
    pub mode: Mode,
    /// The backfill pass, if the run had one.
    pub backfill: Option<Backfill>,
    /// The sample, if the run took one.
    pub audit: Option<Audit>,
}

/// The report as text, one fact to a line, ending with the problems it lists.
#[must_use]
pub fn render(report: &Report) -> String {
    let how = match report.mode {
        Mode::DryRun => "dry run, nothing was written",
        Mode::Apply => "apply",
    };
    let mut text = format!("kbf-l2-backfill: {how}\n");
    if let Some(b) = &report.backfill {
        let Backfill {
            within_days,
            trees_seen,
            written,
            would_write,
            gone,
            lost,
            failed,
            objects_read,
            bytes_read,
            finished,
            problems,
        } = b;
        text.push_str(&format!(
            "backfill: unlisted within {within_days} days, trees {trees_seen}, \
             all handled {finished}\n\
             sidecars written {written}, would write {would_write}, list gone {gone}, \
             object lost {lost}, failed {failed}\n\
             objects read {objects_read}, bytes read {bytes_read}\n\
             problems listed {}\n",
            problems.len()
        ));
        for problem in problems {
            text.push_str(&format!(
                "  {} {}: {}\n",
                problem.tree, problem.kind, problem.detail
            ));
        }
    }
    if let Some(a) = &report.audit {
        let Audit {
            seed,
            requested,
            live_packs,
            written,
        } = a;
        text.push_str(&format!(
            "audit sample: seed {seed}, {written} keys written of {requested} asked, \
             {live_packs} live packs\n"
        ));
    }
    text
}
