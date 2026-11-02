//! What a hook may declare the harness should accept: three behaviours a hook may show for
//! safety, each refused unless the hook's test opts in ([`super::Conformance::with_allowances`]).
//! docs/design/host-updates.md (section 9, "Allowances") says why each is safe.

use std::path::PathBuf;

use super::sandbox::Tree;
use crate::Report;

/// The `reason` of a first-install `rollback` that refuses to stop what `cutover` started
/// because there is no previous release to go back to.
pub const NO_PREVIOUS_RELEASE: &str = "no_previous_release";

/// The behaviours a hook declares; the default declares none, and the harness then checks
/// strictly.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Allowances {
    /// A `rollback` run again may say `reversible: true` where the first run said `false` (a
    /// rerun that finds no record cannot know the first run committed). The updater ignores
    /// `reversible` on a `rollback` that succeeded, so only the flag of a successful rollback
    /// is let go: the rerun must still end as the first did and change nothing. A flag that
    /// falls from `true` to `false`, a flip of a failed rollback, and a flip on any other step
    /// are still refused.
    pub rollback_rerun_may_say_reversible: bool,
    /// A cordon the hook may leave to lapse by its lease after a crash between placing it and
    /// recording it as its own ([`LapsingCordon`]). `None`: no such cordon.
    pub lapsing_cordon: Option<LapsingCordon>,
    /// On a first install (no previous release in the params), a `rollback` after `cutover` may
    /// fail (exit `1`, not a deferral or a halt) with the reason [`NO_PREVIOUS_RELEASE`] rather
    /// than stop what it started. It must change nothing (world and record), and run again it
    /// must fail with the same reason and the same `reversible` and change nothing. With a
    /// previous release, or with another reason, the rollback must still succeed.
    ///
    /// It covers the `rollback` that [`super::Conformance::idempotence`] runs, which is what
    /// [`super::Conformance::check_all`] runs as a first install after its update checks. A
    /// caller that sets [`super::Conformance::with_first_install`] runs every check as a first
    /// install, and crash-resume and the sequences still require the rollback to succeed.
    pub first_install_rollback_may_fail: bool,
}

/// A cordon the hook places and records as its own, in the state the harness compares: a world
/// path as it is, a record path under `record/`.
///
/// After a crash right after the change that placed the cordon, and before the hook recorded
/// it, a run again cannot tell that cordon from one an operator placed, and leaves it. A
/// divergence of [`paths`](Self::paths), added or changed, is accepted only for a crash at the
/// kill point right after the change that made a cordon path appear, and only while no path of
/// [`ownership`](Self::ownership) is there; a cordon removed, a divergence of any other path,
/// a cordon left by a crash at any other point (a renewal of a cordon already there included),
/// and a cordon left although its ownership was recorded, are still refused.
///
/// The kill point before a kill ran in another sandbox, so the window is judged on which paths
/// exist, never on what they hold: a cordon's bytes may name its sandbox (its path, its
/// directory's name, a process id), and are compared only within one sandbox, against the run
/// the harness is checking.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LapsingCordon {
    /// The paths that hold the cordon.
    pub paths: Vec<PathBuf>,
    /// The paths whose presence means the hook has recorded the cordon as its own. Empty: the
    /// harness cannot tell that the cordon is unrecorded, and accepts no cordon.
    pub ownership: Vec<PathBuf>,
}

impl Allowances {
    /// Whether a `rollback` rerun that said `second` where the first said `first` is accepted.
    pub(crate) fn rollback_reversible_rise(&self, first: bool, second: bool) -> bool {
        self.rollback_rerun_may_say_reversible && !first && second
    }

    /// Whether a crash that left the state `at_kill`, where the kill point before it (or the
    /// step's start) had left `before`, is the crash window of the lapsing cordon: a path of the
    /// cordon is there and was not before, and the hook has recorded no ownership. Only which
    /// paths exist is read, since `before` may come from another sandbox.
    pub(crate) fn in_cordon_window(&self, before: &Tree, at_kill: &Tree) -> bool {
        let Some(cordon) = &self.lapsing_cordon else {
            return false;
        };
        let placed = cordon
            .paths
            .iter()
            .any(|path| at_kill.contains_key(path) && !before.contains_key(path));
        let unrecorded = !cordon.ownership.is_empty()
            && !cordon
                .ownership
                .iter()
                .any(|path| at_kill.contains_key(path));
        placed && unrecorded
    }

    /// `want` and `got` without the cordon paths that `got` holds: what is left to compare
    /// once a cordon left to lapse (added or changed) is set aside. A cordon path `got` lacks
    /// stays in both, so its removal is still a divergence.
    pub(crate) fn without_lapsed(&self, want: &Tree, got: &Tree) -> (Tree, Tree) {
        let (mut want, mut got) = (want.clone(), got.clone());
        for path in self.lapsing_cordon.iter().flat_map(|c| &c.paths) {
            if got.remove(path).is_some() {
                want.remove(path);
            }
        }
        (want, got)
    }

    /// Whether a `rollback` that ended as `outcome` saying `report` is the accepted failure of
    /// a first install (`first_install`: the params hold no previous release).
    pub(crate) fn rollback_may_fail(
        &self,
        first_install: bool,
        outcome: crate::Outcome,
        report: &Report,
    ) -> bool {
        self.first_install_rollback_may_fail
            && first_install
            && outcome == crate::Outcome::Failure
            && report.reason.as_deref() == Some(NO_PREVIOUS_RELEASE)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Outcome;
    use crate::conformance::Entry;

    fn tree(entries: &[(&str, &str)]) -> Tree {
        entries
            .iter()
            .map(|(path, body)| (PathBuf::from(path), Entry::File(body.as_bytes().to_vec())))
            .collect()
    }

    fn cordon() -> Allowances {
        Allowances {
            lapsing_cordon: Some(LapsingCordon {
                paths: vec![PathBuf::from("cordon")],
                ownership: vec![PathBuf::from("record/cordon_mine")],
            }),
            ..Allowances::default()
        }
    }

    /// Catches an allowance that lets the flag fall, or that is on without being declared.
    #[test]
    fn only_a_declared_rise_of_reversible_is_accepted() {
        let on = Allowances {
            rollback_rerun_may_say_reversible: true,
            ..Allowances::default()
        };
        assert!(on.rollback_reversible_rise(false, true));
        assert!(!on.rollback_reversible_rise(true, false));
        assert!(!Allowances::default().rollback_reversible_rise(false, true));
    }

    /// Catches a window that opens on a cordon that was already there (renewed or not, its
    /// bytes from another sandbox or not), that went away, whose ownership is recorded, or whose
    /// ownership the declaration cannot see.
    #[test]
    fn the_window_is_the_kill_right_after_an_unrecorded_cordon_appeared() {
        let none = tree(&[]);
        let placed = tree(&[("cordon", "1")]);
        let renewed = tree(&[("cordon", "2")]);
        let owned = tree(&[("cordon", "1"), ("record/cordon_mine", "1")]);
        assert!(cordon().in_cordon_window(&none, &placed));
        assert!(!cordon().in_cordon_window(&placed, &renewed));
        assert!(!cordon().in_cordon_window(&placed, &placed));
        assert!(!cordon().in_cordon_window(&placed, &none));
        assert!(!cordon().in_cordon_window(&none, &owned));
        assert!(!Allowances::default().in_cordon_window(&none, &placed));
        let blind = Allowances {
            lapsing_cordon: Some(LapsingCordon {
                paths: vec![PathBuf::from("cordon")],
                ownership: Vec::new(),
            }),
            ..Allowances::default()
        };
        assert!(!blind.in_cordon_window(&none, &placed));
    }

    /// Catches setting aside a cordon that was removed, or a path that is not the cordon.
    #[test]
    fn only_a_cordon_that_is_there_is_set_aside() {
        let want = tree(&[("current", "v2")]);
        let left = tree(&[("current", "v2"), ("cordon", "1")]);
        assert_eq!(
            cordon().without_lapsed(&want, &left),
            (want.clone(), want.clone())
        );
        let removed = tree(&[("cordon", "1")]);
        let (kept_want, kept_got) = cordon().without_lapsed(&removed, &tree(&[]));
        assert_eq!((kept_want, kept_got), (removed, tree(&[])));
        let other = tree(&[("current", "v2"), ("drain", "1")]);
        let (_, got) = cordon().without_lapsed(&want, &other);
        assert_eq!(got, other);
        assert_eq!(
            Allowances::default().without_lapsed(&want, &left),
            (want, left)
        );
    }

    /// Catches a rollback failure accepted on an update, with another reason or outcome, or
    /// undeclared.
    #[test]
    fn a_rollback_may_fail_only_on_a_declared_first_install_with_the_reason() {
        let on = Allowances {
            first_install_rollback_may_fail: true,
            ..Allowances::default()
        };
        let said = |reason: Option<&str>| Report {
            reason: reason.map(str::to_owned),
            ..Report::default()
        };
        let reason = said(Some(NO_PREVIOUS_RELEASE));
        assert!(on.rollback_may_fail(true, Outcome::Failure, &reason));
        assert!(!on.rollback_may_fail(false, Outcome::Failure, &reason));
        assert!(!on.rollback_may_fail(true, Outcome::Deferred, &reason));
        assert!(!on.rollback_may_fail(true, Outcome::Halt, &reason));
        assert!(!on.rollback_may_fail(true, Outcome::Failure, &said(Some("agent_busy"))));
        assert!(!on.rollback_may_fail(true, Outcome::Failure, &said(None)));
        assert!(!Allowances::default().rollback_may_fail(true, Outcome::Failure, &reason));
    }
}
