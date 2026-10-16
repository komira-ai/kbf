//! `kbf-coverage tighten`: lowers the `coverage-baseline` file to a measurement, and
//! refuses to raise it.
//!
//! The ratchet ([`crate::ratchet`]) fails when the file is looser than measured, so a
//! change that covers more code must lower the file in the same pull request. Running
//! `tighten` on the measured file the coverage job uploads is that step: it is
//! required when coverage improves, and it is not a bypass. The rule it keeps for
//! every value it writes ([`INVARIANT`]):
//!
//! `tighten` writes no count above the recorded one (a value recorded as `-`, and each
//! value of a crate the file does not list, counts as 0), never turns lines into `-`,
//! and turns branches into `-` only from 0 and only while the crate's lines are
//! measured, so a new crate goes in only as `0 0` or `0 -`.
//!
//! Lines `-` hides everything: a crate whose library says `[lib] test = false` has no
//! record in the tracefile at all, so it measures `- -` whatever untested code it
//! holds. [`plan`] compares each listed crate's metrics with [`ratchet::compare`], the
//! ratchet's own rule, and:
//!
//! - lowers a [`Compare::Looser`] value to the measured one (a lower count, `-`
//!   recorded where the crate now measures 0, or branches 0 recorded where the crate
//!   now has no branch and its lines are still measured);
//! - refuses a [`Compare::MoreMissed`] one: a higher count, or `-` to a count above 0
//!   ([`Refusal::Raised`]: cover the code); a count to no data
//!   ([`Refusal::NoLongerMeasured`]), lines 0 included; or a branch count above 0 to
//!   no data while the lines are measured ([`Refusal::BranchesNoLongerMeasured`]);
//! - adds a workspace crate the file lacks only as `0 0` or `0 -`: one that misses
//!   anything is [`Refusal::NewCrateUncovered`], and one whose lines measure `-` is
//!   [`Refusal::NewCrateUnmeasured`];
//! - removes a crate that is neither measured nor in the workspace (its
//!   `crates/<name>/Cargo.toml` is gone);
//! - refuses any other difference between the file's crates, the measured crates and
//!   the workspace's crates, which means the measurement is of another tree (CI
//!   measures the pull request merged with `main`, so a branch behind `main` gets one;
//!   [`OTHER_TREE_ADVICE`] says what to do, and [`tree_advice`] gives a softer form
//!   of it for any other refusal of a measured file);
//! - refuses everything when none of the file's crates is in the workspace: `--root`
//!   is not this repository, and every line would otherwise be removed.
//!
//! Every value that takes `-` where the rule forbids it (a crate whose code
//! legitimately stops being measured, or a new crate with nothing to measure) is a hand
//! edit in a reviewed pull request, and the refusal says so ([`HAND_EDIT`]).
//!
//! One refusal refuses everything: the file is written only when every difference is
//! one the rule allows. The result then equals the measurement, so the ratchet passes
//! on it and a second run finds nothing to change. `check` decides whether to tell the
//! user to run `tighten` with the same [`plan`] ([`check_advice`]), so it never
//! suggests a run that would be refused.

use std::collections::BTreeSet;
use std::fmt::{self, Write as _};
use std::io;
use std::path::{Path, PathBuf};

use crate::baseline::{Baseline, Entry};
use crate::ratchet::{self, Compare, Metric};
use crate::show;

/// One change [`plan`] makes to the file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    /// A metric lowered to the measured value.
    Lowered {
        krate: String,
        metric: Metric,
        from: Option<u64>,
        to: Option<u64>,
    },
    /// A new workspace crate, `0 0` or `0 -`.
    Added { krate: String, entry: Entry },
    /// A crate gone from the workspace.
    Removed { krate: String, entry: Entry },
}

/// `<lines> <branches>`, as the file writes them.
fn pair(e: Entry) -> String {
    format!("{} {}", show(e.lines), show(e.branches))
}

impl fmt::Display for Change {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Change::Lowered {
                krate,
                metric,
                from,
                to,
            } => write!(f, "{krate} {metric}: {} -> {}", show(*from), show(*to)),
            Change::Added { krate, entry } => write!(f, "{krate}: added as {}", pair(*entry)),
            Change::Removed { krate, entry } => write!(
                f,
                "{krate}: removed (was {}; crates/{krate}/Cargo.toml is gone)",
                pair(*entry)
            ),
        }
    }
}

/// The rule `tighten` keeps for every value it writes, as the docs state it.
pub const INVARIANT: &str = "`tighten` writes no count above the recorded one (a value \
     recorded as `-`, and each value of a crate the file does not list, counts as 0), never \
     turns lines into `-`, and turns branches into `-` only from 0 and only while the crate's \
     lines are measured, so a new crate goes in only as `0 0` or `0 -`.";

/// The fix every "no data" refusal names when no test target can come back.
pub const HAND_EDIT: &str = "if it has nothing to measure, put - in its line by hand in a \
     reviewed pull request";

/// What a failing check says `tighten` fixes: the changes [`plan`] writes.
pub const FIXES: &str = "coverage improved (fewer items missed than recorded), a crate was \
     added with its lines measured and none missed (`0 0` or `0 -`), or a crate was deleted";

/// What to do about a measurement of another tree, printed after the refusals when
/// any is [`Refusal::of_another_tree`].
pub const OTHER_TREE_ADVICE: &str = "CI measures the pull request merged with main: \
     bring the branch up to date with main, re-run CI, and run the command its coverage job prints";

/// What to do when a measured file was refused for other reasons: CI's file measures
/// the pull request merged with main, so main's changes can cause any refusal.
pub const MAYBE_OTHER_TREE_ADVICE: &str = "A measured file from CI is of the pull request \
     merged with main: if the branch is behind main, main's changes can cause these refusals; \
     bring the branch up to date with main, re-run CI, and run the command its coverage job \
     prints. A refusal that stays is this branch's to fix";

/// The advice printed after a refused run's `refusals`: [`OTHER_TREE_ADVICE`] when one
/// says the measurement is of another tree, else [`MAYBE_OTHER_TREE_ADVICE`] when it
/// came from a measured file (`--measured`, the file CI uploads), else none (measured
/// here from a tracefile).
pub fn tree_advice(refusals: &[Refusal], measured_file: bool) -> Option<&'static str> {
    if refusals.iter().any(Refusal::of_another_tree) {
        Some(OTHER_TREE_ADVICE)
    } else if measured_file {
        Some(MAYBE_OTHER_TREE_ADVICE)
    } else {
        None
    }
}

/// A difference `tighten` will not write.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Refusal {
    #[error(
        "{krate} {metric}: recorded {}, measured {}: more missed than recorded; cover the code",
        show(*recorded),
        show(*measured)
    )]
    Raised {
        krate: String,
        metric: Metric,
        recorded: Option<u64>,
        measured: Option<u64>,
    },
    #[error(
        "{krate} {metric}: recorded {recorded}, measured -: nothing measured where the file has a count \
         would hide misses (a crate with `[lib] test = false` has no coverage record at all); \
         give it a test target back, or, {}",
        HAND_EDIT
    )]
    NoLongerMeasured {
        krate: String,
        metric: Metric,
        recorded: u64,
    },
    #[error(
        "{krate} branches: recorded {recorded}, measured - while its lines are measured: the missed \
         branches were removed rather than covered, or the measurement lost branch data (it needs \
         --branch); tighten does not turn a count into -: {}",
        HAND_EDIT
    )]
    BranchesNoLongerMeasured { krate: String, recorded: u64 },
    #[error(
        "{krate}: not in the baseline, measured {}: a new crate must arrive covered (`0 0` or `0 -`)",
        pair(*measured)
    )]
    NewCrateUncovered { krate: String, measured: Entry },
    #[error(
        "{krate}: not in the baseline, measured {}: a new crate whose lines measure - has no coverage \
         record (a crate with `[lib] test = false` has none at all, whatever code it holds); give it \
         a test target, or, {}",
        pair(*measured),
        HAND_EDIT
    )]
    NewCrateUnmeasured { krate: String, measured: Entry },
    #[error(
        "{krate}: crates/{krate}/Cargo.toml exists but the measured file has no line for it; measure this tree"
    )]
    Unmeasured { krate: String },
    #[error(
        "{krate}: measured, but there is no crates/{krate}/Cargo.toml; the measurement is of another tree"
    )]
    NotInWorkspace { krate: String },
    #[error(
        "none of the baseline's {recorded} crate(s) has a crates/<name>/Cargo.toml under the root; \
         pass the workspace root as --root"
    )]
    NoRecordedCrateInWorkspace { recorded: usize },
}

impl Refusal {
    /// Whether the refusal means the measurement is of another tree than the one
    /// under the root.
    pub fn of_another_tree(&self) -> bool {
        matches!(
            self,
            Refusal::Unmeasured { .. } | Refusal::NotInWorkspace { .. }
        )
    }
}

/// Every change that takes `recorded` to `measured`, or every reason it may not;
/// `workspace` is the crates under `crates/` ([`ratchet::workspace_crates`]).
pub fn plan(
    recorded: &Baseline,
    measured: &Baseline,
    workspace: &BTreeSet<String>,
) -> Result<Vec<Change>, Vec<Refusal>> {
    // A root that holds none of the file's crates would see every line as gone.
    if !recorded.is_empty() && !recorded.keys().any(|k| workspace.contains(k)) {
        return Err(vec![Refusal::NoRecordedCrateInWorkspace {
            recorded: recorded.len(),
        }]);
    }
    let names: BTreeSet<&String> = recorded
        .keys()
        .chain(measured.keys())
        .chain(workspace)
        .collect();
    let (mut changes, mut refusals) = (Vec::new(), Vec::new());
    for krate in names {
        let in_workspace = workspace.contains(krate);
        let recorded = recorded.get(krate).copied();
        let krate = krate.clone();
        let Some(m) = measured.get(&krate).copied() else {
            // Not measured: a crate gone from the workspace leaves the file; one still
            // there (listed or not) means the measured file is of another tree.
            match recorded {
                Some(entry) if !in_workspace => changes.push(Change::Removed { krate, entry }),
                _ => refusals.push(Refusal::Unmeasured { krate }),
            }
            continue;
        };
        if !in_workspace {
            refusals.push(Refusal::NotInWorkspace { krate });
            continue;
        }
        let Some(r) = recorded else {
            let misses = |v: Option<u64>| v.is_some_and(|v| v > 0);
            if misses(m.lines) || misses(m.branches) {
                refusals.push(Refusal::NewCrateUncovered { krate, measured: m });
            } else if m.lines.is_none() {
                refusals.push(Refusal::NewCrateUnmeasured { krate, measured: m });
            } else {
                changes.push(Change::Added { krate, entry: m });
            }
            continue;
        };
        for (metric, compared) in ratchet::compare(r, m) {
            let krate = krate.clone();
            let (from, to) = (r.get(metric), m.get(metric));
            match compared {
                Compare::Equal => {}
                Compare::Looser => changes.push(Change::Lowered {
                    krate,
                    metric,
                    from,
                    to,
                }),
                Compare::MoreMissed => refusals.push(match (from, to) {
                    // Lines measured: compare lowers branches 0, so this is a count.
                    (Some(recorded), None) if metric == Metric::Branches && m.lines.is_some() => {
                        Refusal::BranchesNoLongerMeasured { krate, recorded }
                    }
                    (Some(recorded), None) => Refusal::NoLongerMeasured {
                        krate,
                        metric,
                        recorded,
                    },
                    _ => Refusal::Raised {
                        krate,
                        metric,
                        recorded: from,
                        measured: to,
                    },
                }),
            }
        }
    }
    if refusals.is_empty() {
        Ok(changes)
    } else {
        Err(refusals)
    }
}

/// What a failing check prints after its table: the `tighten` command (`command`)
/// when [`plan`] takes `recorded` to `measured` (every failure is one [`FIXES`] names),
/// else every refusal. `measured` lists every workspace crate, as `check` measures.
pub fn check_advice(recorded: &Baseline, measured: &Baseline, command: &str) -> String {
    let workspace: BTreeSet<String> = measured.keys().cloned().collect();
    match plan(recorded, measured, &workspace) {
        Ok(_) => format!(
            "Every failure above is one `kbf-coverage tighten` fixes: {FIXES}. Running it is \
             required, and it is not a bypass: it only lowers counts. Run this, then commit \
             coverage-baseline in this pull request:\n\n```sh\n{command}\n```\n"
        ),
        Err(refusals) => {
            let mut out = String::from(
                "`kbf-coverage tighten` cannot fix this failure: it only lowers counts, and it refuses:\n\n",
            );
            for r in refusals {
                // Writing to a String cannot fail.
                let _ = writeln!(out, "- {r}");
            }
            out
        }
    }
}

/// `recorded` with `changes` made.
pub fn apply(recorded: &Baseline, changes: &[Change]) -> Baseline {
    let mut out = recorded.clone();
    for c in changes {
        match c {
            Change::Lowered {
                krate, metric, to, ..
            } => {
                out.entry(krate.clone()).and_modify(|e| match metric {
                    Metric::Lines => e.lines = *to,
                    Metric::Branches => e.branches = *to,
                });
            }
            Change::Added { krate, entry } => {
                out.insert(krate.clone(), *entry);
            }
            Change::Removed { krate, .. } => {
                out.remove(krate);
            }
        }
    }
    out
}

/// Replaces `path` with `text` whole: written to `<path>.tighten-tmp` beside it, then
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
    temp.push(".tighten-tmp");
    let temp = PathBuf::from(temp);
    let result = std::fs::write(&temp, text).and_then(|()| rename(&temp, path));
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result
}

#[cfg(test)]
mod tests;
