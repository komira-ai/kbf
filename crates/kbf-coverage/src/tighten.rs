//! `kbf-coverage tighten`: lowers the `coverage-baseline` file to a measurement, and
//! refuses to raise it.
//!
//! The ratchet ([`crate::ratchet`]) fails when the file is looser than measured, so a
//! change that covers more code must lower the file in the same pull request. Running
//! `tighten` on the measured file the coverage job uploads is that step: it is
//! required when coverage improves, and it is not a bypass. [`plan`] compares each
//! metric with [`ratchet::compare`], the ratchet's own rule, and:
//!
//! - lowers a [`Compare::Looser`] value to the measured one (a lower count, `-`
//!   recorded where the crate now measures 0, or 0 recorded where it now measures no
//!   data: nothing was missed, and nothing can be);
//! - refuses a [`Compare::MoreMissed`] one (a higher count, `-` to a count above 0, a
//!   count above 0 to no data, which would hide those misses);
//! - adds a workspace crate the file lacks only when every measured value is 0 or
//!   `-` (new code arrives covered), and refuses it otherwise;
//! - removes a crate that is neither measured nor in the workspace (its
//!   `crates/<name>/Cargo.toml` is gone);
//! - refuses any other difference between the file's crates, the measured crates and
//!   the workspace's crates, which means the measurement is of another tree (CI
//!   measures the pull request merged with `main`, so a branch behind `main` gets one;
//!   [`OTHER_TREE_ADVICE`] says what to do);
//! - refuses everything when none of the file's crates is in the workspace: `--root`
//!   is not this repository, and every line would otherwise be removed.
//!
//! One refusal refuses everything: the file is written only when every difference is
//! one the ratchet calls looser. The result then equals the measurement, so the
//! ratchet passes on it and a second run finds nothing to change.

use std::collections::BTreeSet;
use std::fmt;

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
    /// A new workspace crate, every value 0 or `-`.
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

/// What to do about a measurement of another tree, printed after the refusals when
/// any is [`Refusal::of_another_tree`].
pub const OTHER_TREE_ADVICE: &str = "CI measures the pull request merged with main: \
     bring the branch up to date with main, re-run CI, and run the command its coverage job prints";

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
        "{krate}: not in the baseline, measured {}: a new crate must arrive covered (every value 0 or -)",
        pair(*measured)
    )]
    NewCrateUncovered { krate: String, measured: Entry },
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
            let covered = |v: Option<u64>| v.is_none_or(|v| v == 0);
            if covered(m.lines) && covered(m.branches) {
                changes.push(Change::Added { krate, entry: m });
            } else {
                refusals.push(Refusal::NewCrateUncovered { krate, measured: m });
            }
            continue;
        };
        for (metric, from, to) in [
            (Metric::Lines, r.lines, m.lines),
            (Metric::Branches, r.branches, m.branches),
        ] {
            let krate = krate.clone();
            match ratchet::compare(from, to) {
                Compare::Equal => {}
                Compare::Looser => changes.push(Change::Lowered {
                    krate,
                    metric,
                    from,
                    to,
                }),
                Compare::MoreMissed => refusals.push(Refusal::Raised {
                    krate,
                    metric,
                    recorded: from,
                    measured: to,
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

#[cfg(test)]
mod tests;
