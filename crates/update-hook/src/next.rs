//! What the updater runs next, per step and outcome: the table of the contract as a pure
//! function.
//!
//! | step ran | success (0) | failure (1), timeout, killed | deferred (75) | halt (76) |
//! |---|---|---|---|---|
//! | `prepare` | `cutover` | `rollback`, then `Failed` (`before_commit`, or `after_commit` if a step reported `reversible: false`) | `rollback`, then `Deferred` | stop, `halt` |
//! | `cutover` | `verify` | `rollback`, then `Failed` | `rollback`, then `Deferred`; only valid if nothing irreversible happened | stop, `halt` |
//! | `verify` | `cleanup` | `rollback`, then `Failed` with the hook's class | `verify` again with the backoff until the deadline, then `rollback` | stop, `halt` |
//! | `rollback` | report the original `Failed` or `Deferred` | before the commit: `halt`; after it: `Failed(after_commit)` if the hook says `reversible: false`, else `halt` | `rollback` again up to its bound, then as for failure | stop, `halt` |
//! | `cleanup` | `Succeeded` (with `promotion_pending` of the last `verify`) | if its line states `reversible: true` (and nothing committed): `rollback`, and the host's result is the rollback's; otherwise (`false`, or the field left out) `cleanup` again, up to `cleanup_retries`, then `Failed(after_commit)` | `cleanup` again later, within the same retries | stop, `halt` |
//!
//! The function holds no clock and no counter: the updater says whether the verify deadline has
//! passed and whether a retry is left ([`Context`]), and does the waiting itself.

use serde::{Deserialize, Serialize};

use crate::{Class, Outcome, Report, Step};

/// Where in the life of a failure it happened, assigned by the updater and never by the hook.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    /// Nothing irreversible had been reported. Also the phase of a failed `cleanup` that was
    /// rolled back for stating `reversible: true`. It does not count toward the coordinator's
    /// `rollback_after_failed` (only `after_commit` and `gate_release` do).
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
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
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
        self.committed |= !report.reversible;
        if step == Step::Verify {
            self.promotion_pending = report.promotion_pending;
        }
    }
}

/// The updater's next move after `step` ended as `outcome` with `report` (the hook's last line,
/// `Report::default()` when it wrote none).
pub fn next(step: Step, outcome: Outcome, report: &Report, ctx: &Context) -> Next {
    let committed = ctx.committed || !report.reversible;
    let class = report.class;
    let after_secs = report.retry_after_secs;
    let failed = |phase| Verdict::Failed { phase, class };
    let before_or_after = if committed {
        Phase::AfterCommit
    } else {
        Phase::BeforeCommit
    };
    let gate = match (committed, class) {
        (true, _) => Phase::AfterCommit,
        (false, Class::Release) => Phase::GateRelease,
        (false, Class::Environment) => Phase::GateEnvironment,
    };
    let rollback_then = |then| Next::Rollback { then };
    match (step, outcome) {
        (_, Outcome::Halt) => Next::Report(Verdict::Halt),
        (Step::Prepare, Outcome::Success) => Next::Run(Step::Cutover),
        (Step::Cutover, Outcome::Success) => Next::Run(Step::Verify),
        (Step::Verify, Outcome::Success) => Next::Run(Step::Cleanup),
        (Step::Cleanup, Outcome::Success) => Next::Report(Verdict::Succeeded {
            promotion_pending: ctx.promotion_pending,
        }),
        (Step::Rollback, Outcome::Success) => {
            Next::Report(ctx.then.clone().unwrap_or_else(|| failed(before_or_after)))
        }
        (Step::Prepare | Step::Cutover, Outcome::Failure) => rollback_then(failed(before_or_after)),
        (Step::Prepare, Outcome::Deferred) => rollback_then(Verdict::Deferred {
            retry_after_secs: after_secs,
        }),
        // A deferral is only valid before anything irreversible: after it, a failure.
        (Step::Cutover, Outcome::Deferred) if committed => rollback_then(failed(before_or_after)),
        (Step::Cutover, Outcome::Deferred) => rollback_then(Verdict::Deferred {
            retry_after_secs: after_secs,
        }),
        (Step::Verify, Outcome::Failure) => rollback_then(failed(gate)),
        (Step::Verify, Outcome::Deferred) if ctx.verify_deadline_passed => {
            rollback_then(failed(gate))
        }
        (Step::Verify, Outcome::Deferred) => Next::Again {
            step: Step::Verify,
            after_secs,
        },
        (Step::Rollback, Outcome::Deferred) if ctx.retries_left => Next::Again {
            step: Step::Rollback,
            after_secs,
        },
        // A rollback that fails after the commit and says it cannot give it back is a failure
        // to page for; one that says nothing, or says it can, leaves a host nothing repairs.
        (Step::Rollback, Outcome::Failure | Outcome::Deferred) if !report.reversible => {
            Next::Report(failed(Phase::AfterCommit))
        }
        (Step::Rollback, Outcome::Failure | Outcome::Deferred) => Next::Report(Verdict::Halt),
        // A cleanup that fails and states `reversible: true` committed nothing: roll back, and
        // the host's result is the rollback's: Failed(before_commit) with the cleanup's class.
        // One that states false, or leaves the field out (an old hook), keeps the path below.
        (Step::Cleanup, Outcome::Failure) if report.reversible_stated && !committed => {
            rollback_then(failed(before_or_after))
        }
        (Step::Cleanup, Outcome::Failure | Outcome::Deferred) if ctx.retries_left => Next::Again {
            step: Step::Cleanup,
            after_secs,
        },
        (Step::Cleanup, Outcome::Failure | Outcome::Deferred) => {
            Next::Report(failed(Phase::AfterCommit))
        }
    }
}
