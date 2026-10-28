//! `--scheduler-core`: which core drives the farm's scheduling. Only `legacy`, the
//! driver the server has always had, is built; `js` is a value of the flag that the
//! server refuses at start (`ConfigError::SchedulerCoreUnavailable`), so that a unit
//! file written for the later core fails loudly and is never taken for the first.
//!
//! The core a server runs is logged once at start ([`SchedulerCore::start_line`]) and
//! exported as the gauge `kbf_scheduler_core{core}` ([`render`]).

use std::fmt::Write as _;

use clap::ValueEnum;

/// What `--scheduler-core` selects.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, ValueEnum)]
pub enum SchedulerCore {
    /// The scheduler driver the server has always had
    #[default]
    Legacy,
    /// Not available yet: a server given it refuses to start
    Js,
}

impl SchedulerCore {
    /// The core every server runs in this release: the only one a start accepts.
    pub const RUNNING: Self = Self::Legacy;

    /// The core a `Listeners` built by a test names. Always `Legacy` in this build.
    #[must_use]
    pub const fn under_test() -> Self {
        Self::Legacy
    }

    /// The value the flag takes and the gauge's `core` label carries.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Legacy => "legacy",
            Self::Js => "js",
        }
    }

    /// Whether this release can run the core.
    #[must_use]
    pub const fn available(self) -> bool {
        matches!(self, Self::Legacy)
    }

    /// The line a start logs: the core it runs.
    #[must_use]
    pub fn start_line(self) -> String {
        format!("scheduler core: {}", self.name())
    }
}

/// Appends the gauge `kbf_scheduler_core{core}` to `out`: 1 for `running`, 0 for each
/// other core, so a dashboard can tell a core that is off from a server that does not
/// report one.
pub(crate) fn render(out: &mut String, running: SchedulerCore) {
    let name = "kbf_scheduler_core";
    let _ = writeln!(
        out,
        "# HELP {name} The scheduler core the server runs: 1 for it, 0 for the others\n\
         # TYPE {name} gauge"
    );
    for core in SchedulerCore::value_variants() {
        let value = u8::from(*core == running);
        let _ = writeln!(out, "{name}{{core=\"{}\"}} {value}", core.name());
    }
}

#[cfg(test)]
mod tests;
