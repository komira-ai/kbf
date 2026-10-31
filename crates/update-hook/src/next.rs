//! What the updater runs next, per step and outcome: the table of the contract as a pure
//! function.
//!
//! | step ran | success (0) | failure (1), timeout, killed | deferred (75) | halt (76) |
//! |---|---|---|---|---|
//! | `prepare` | `cutover` | `rollback`, then `Failed` (`before_commit`, or `after_commit` if a step reported `reversible: false`) | `rollback`, then `Deferred` | stop, `halt` |
//! | `cutover` | `verify` | `rollback`, then `Failed` | `rollback`, then `Deferred`; only valid if nothing irreversible happened | stop, `halt` |
//! | `verify` | `cleanup` | `rollback`, then `Failed` with the hook's class | `verify` again with the backoff until the deadline, then `rollback` | stop, `halt` |
//! | `rollback` | report the original `Failed` or `Deferred` | before the commit: `halt`; after it: `Failed(after_commit)` if the hook says `reversible: false`, else `halt` | `rollback` again up to its bound, then as for failure | stop, `halt` |
//! | `cleanup` | `Succeeded` (with `promotion_pending` of the last `verify`) | `cleanup` again, up to `cleanup_retries`, then `Failed(after_commit)`; never `rollback` | `cleanup` again later, within the same retries | stop, `halt` |
//!
//! The function holds no clock and no counter: the updater says whether the verify deadline has
//! passed and whether a retry is left ([`Context`]), and does the waiting itself.

use crate::{Class, Outcome, Report, Step};

/// Where in the life of a failure it happened, assigned by the updater and never by the hook.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    /// Nothing irreversible had been reported.
    BeforeCommit,
    /// `verify` failed (or ran out of time) before the commit, the hook's class `release`.
    GateRelease,
    /// As `GateRelease`, the hook's class `environment`.
    GateEnvironment,
    /// A step had reported `reversible: false`.
    AfterCommit,
    /// A `halt`.
    Halt,
}

/// What the updater reports for the update when it is over.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    Succeeded {
        promotion_pending: bool,
    },
    Failed {
        phase: Phase,
        class: Class,
    },
    Deferred {
        retry_after_secs: Option<u64>,
    },
    /// No further step runs; an operator clears it.
    Halt,
}

/// What the updater does after a step.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Next {
    /// Run this other step.
    Run(Step),
    /// Run the same step again, after `after_secs` when the hook gave a backoff.
    Again { step: Step, after_secs: Option<u64> },
    /// Run `rollback`; when it succeeds, report `then`.
    Rollback { then: Verdict },
    /// Run nothing more and report.
    Report(Verdict),
}

/// What the updater knows besides the step and how it ended.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Context {
    /// A step reported `reversible: false` (see [`Context::observe`]).
    pub committed: bool,
    /// The last `verify` reported `promotion_pending`.
    pub promotion_pending: bool,
    /// What to report when the running `rollback` succeeds; set from [`Next::Rollback`].
    pub then: Option<Verdict>,
    /// `verify` has been deferred past `verify_deadline_secs`.
    pub verify_deadline_passed: bool,
    /// The step that just ran may be run again: `rollback` within its bound, `cleanup` within
    /// `cleanup_retries`.
    pub retries_left: bool,
}

impl Context {
    /// Records what a finished step told: the commit, and `verify`'s pending promotion.
    pub fn observe(&mut self, step: Step, report: &Report) {
        todo!()
    }
}

/// The updater's next move after `step` ended as `outcome` with `report` (the hook's last line,
/// `Report::default()` when it wrote none).
pub fn next(step: Step, outcome: Outcome, report: &Report, ctx: &Context) -> Next {
    todo!()
}
