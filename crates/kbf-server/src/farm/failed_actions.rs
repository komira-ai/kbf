//! `kbf_execute_failed_actions_total{class}` on `GET /metrics`: the operations this
//! process finished as failed, by the failure class of `docs/design/failure-classes.md`
//! (section 1). The invocation record carries a class per attempt; this is the metric a
//! gate reads.
//!
//! An operation is counted once, when the farm settles it (not once per caller waiting
//! on it, and not once per rerun), by what its callers are told:
//!
//! - `action`: it ran and exited non-zero, ran past its timeout, or was killed at the
//!   largest node's memory;
//! - `request`: the action cannot run as written (answered INVALID_ARGUMENT);
//! - `farm`: the farm could not run it (a farm fault past its reruns, a lost input
//!   blob, an operation no live worker could run), which callers are told as INTERNAL
//!   or FAILED_PRECONDITION;
//! - `ambiguous`: never the final answer (section 1.1: an ambiguous result is rerun
//!   and resolves to farm or action), and no code finishes an operation with it yet, so
//!   the series stays 0. It is written so that a gate reading it sees the series.
//!
//! All four series are written from the start of a farm, so an absent series means the
//! metric was removed, not that nothing failed. The counts start again from 0 when the
//! process restarts, as a counter may.

use std::fmt::Write as _;

use kbf_types::{Failure, Outcome};

use super::{Detail, Farm};

/// The family's name.
const NAME: &str = "kbf_execute_failed_actions_total";

/// A failure class of `docs/design/failure-classes.md`, section 1.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Class {
    /// The farm could not run the action.
    Farm,
    /// The action cannot run as written, on any node.
    Request,
    /// The action ran and failed, or needs more than the farm offers.
    Action,
    /// It failed in a way the farm cannot attribute; rerun, never final.
    Ambiguous,
}

impl Class {
    /// Every class, in the order the series are written.
    const ALL: [Self; 4] = [Self::Farm, Self::Request, Self::Action, Self::Ambiguous];

    /// The value of the `class` label: a fixed set.
    const fn label(self) -> &'static str {
        match self {
            Self::Farm => "farm",
            Self::Request => "request",
            Self::Action => "action",
            Self::Ambiguous => "ambiguous",
        }
    }

    const fn slot(self) -> usize {
        self as usize
    }
}

/// How many operations a farm finished as failed, per class.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct FailedActions([u64; 4]);

impl FailedActions {
    /// Counts one failed operation of `class`.
    pub(crate) fn count(&mut self, class: Class) {
        self.0[class.slot()] += 1;
    }

    /// The count of `class`.
    #[cfg(test)]
    pub(crate) fn of(&self, class: Class) -> u64 {
        self.0[class.slot()]
    }

    /// Appends the family, one series per class, to `out`.
    pub(crate) fn render(&self, out: &mut String) {
        let _ = writeln!(
            out,
            "# HELP {NAME} Operations finished as failed, per failure class, since the \
             process started\n# TYPE {NAME} counter"
        );
        for class in Class::ALL {
            let _ = writeln!(
                out,
                "{NAME}{{class=\"{}\"}} {}",
                class.label(),
                self.0[class.slot()]
            );
        }
    }
}

impl Farm {
    /// The operations this process settled as failed, per class.
    #[must_use]
    pub(crate) fn failed_actions(&self) -> FailedActions {
        js_arm!(self, views::failed_actions());
        self.lock().side.failed
    }
}

/// The class an operation finishes in, given its scheduler `outcome` and the report's
/// `detail` if one was kept; `None` for one that did not fail. Follows
/// `finished_of`, which decides what the callers are told.
pub(crate) fn class_of(outcome: Outcome, detail: Option<&Detail>) -> Option<Class> {
    match (detail, outcome) {
        (Some(Detail::Ran(pending)), _) => (pending.result.exit_code != 0).then_some(Class::Action),
        (_, Outcome::Failed(Failure::Timeout)) => Some(Class::Action),
        (Some(Detail::Invalid(_)), _) => Some(Class::Request),
        (_, Outcome::Failed(Failure::OutOfMemory)) => Some(Class::Action),
        // Everything else is the farm's: a fault past its reruns, an input blob the CAS lost
        // since the front checked it, a result the process never saw.
        _ => Some(Class::Farm),
    }
}

#[cfg(test)]
mod tests;
