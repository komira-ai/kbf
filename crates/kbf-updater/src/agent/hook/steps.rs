//! The five steps over the ports and the record (`docs/design/host-updates.md`, section 4.3).
//! Not written yet: every step ends with exit 1 and the reason `not_implemented`.

use std::fmt::Display;
use std::io::Write;

use update_hook::{Class, Invocation, Outcome, Params, Report};

use super::HookWorld;
use super::record::Store;
use crate::agent::flags::Timing;
use crate::version::Version;

/// How a step ended: the exit code and the last line.
#[derive(Debug, PartialEq, Eq)]
pub struct Done {
    pub outcome: Outcome,
    pub report: Report,
}

impl Done {
    /// Failure. `reason` is a token of `[a-z0-9_]`.
    pub fn failed(reason: &str, class: Class, detail: impl Display) -> Done {
        Done {
            outcome: Outcome::Failure,
            report: Report {
                class,
                reason: Some(reason.to_owned()),
                detail: Some(detail.to_string()),
                ..Report::default()
            },
        }
    }
}

/// What a run needs besides the world: who is updating, to what, and where the log goes.
pub struct Run<'a> {
    pub store: &'a Store,
    pub params: &'a Params,
    pub channel: &'a str,
    pub timing: Timing,
    pub target: Version,
    pub digest: String,
    pub err: &'a mut dyn Write,
}

/// One run of one step.
pub struct Hook<'a> {
    run: Run<'a>,
}

impl<'a> Hook<'a> {
    pub fn new(_world: &'a HookWorld, run: Run<'a>) -> Hook<'a> {
        Hook { run }
    }

    /// Runs the step `inv` names and says how it ended.
    pub fn step(&mut self, _inv: &Invocation) -> Done {
        let _ = writeln!(
            self.run.err,
            "kbf update-hook: the steps are not written yet"
        );
        Done::failed(
            "not_implemented",
            Class::Release,
            "the steps are not written yet",
        )
    }
}
