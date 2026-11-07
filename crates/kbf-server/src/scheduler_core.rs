//! `--scheduler-core`: which core drives the farm's scheduling. The one core is `legacy`, the
//! driver the server has always had and the default. The flag still takes its value so that a
//! unit file naming it keeps starting; `js` names a core that was removed and is refused at
//! start, before anything opens.
//!
//! The core a server runs is logged once at start ([`SchedulerCore::start_line`]) and
//! exported as the gauge `kbf_scheduler_core{core}` ([`render`]).

use std::fmt::Write as _;

/// The core a server runs, and what `--scheduler-core` selects.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SchedulerCore {
    /// The scheduler driver the server has always had
    #[default]
    Legacy,
}

/// A `--scheduler-core` value the server does not run.
#[derive(Debug, thiserror::Error)]
pub(crate) enum SchedulerCoreError {
    /// `js`, the core that was removed.
    #[error(
        "the js scheduler core was removed: scheduling runs in the job service; use \
         --scheduler-core legacy or leave the flag out"
    )]
    JsRemoved,
    /// Any other word.
    #[error("the one scheduler core is legacy")]
    Unknown,
}

/// The parser of `--scheduler-core`: `legacy`, and a refusal of every other word.
///
/// # Errors
/// [`SchedulerCoreError::JsRemoved`] for `js`, [`SchedulerCoreError::Unknown`] for the rest.
pub(crate) fn parse(value: &str) -> Result<SchedulerCore, SchedulerCoreError> {
    match value {
        "legacy" => Ok(SchedulerCore::Legacy),
        "js" => Err(SchedulerCoreError::JsRemoved),
        _ => Err(SchedulerCoreError::Unknown),
    }
}

impl SchedulerCore {
    /// The value the flag takes and the gauge's `core` label carries.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Legacy => "legacy",
        }
    }

    /// The line a start logs: the core it runs.
    #[must_use]
    pub fn start_line(self) -> String {
        format!("scheduler core: {}", self.name())
    }
}

/// Appends the gauge `kbf_scheduler_core{core}` to `out`: the one series, `legacy`, at 1, so
/// a dashboard can tell a server that runs the core from one that does not report it.
pub(crate) fn render(out: &mut String) {
    let name = "kbf_scheduler_core";
    let _ = writeln!(
        out,
        "# HELP {name} The scheduler core the server runs: 1 for it\n\
         # TYPE {name} gauge"
    );
    let _ = writeln!(out, "{name}{{core=\"{}\"}} 1", SchedulerCore::Legacy.name());
}

#[cfg(test)]
mod tests;
