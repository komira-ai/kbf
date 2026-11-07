//! `--usage-profiles`: whether the farm learns what its leases use. Only `off`, the default,
//! is accepted: `record` and `on` were run by the js scheduler core, which was removed, and
//! the flag still parses them so that a start given one is refused with a message that says
//! so ([`UsageProfilesError::JsCoreRemoved`]) instead of a parse error.
//!
//! The mode is logged at start ([`UsageProfiles::start_line`]) and exported as the gauge
//! `kbf_usage_profiles_mode{mode}` ([`render`]). The mode itself is [`kbf_usage::UsageMode`];
//! this type is its flag spelling.

use std::fmt::Write as _;

use clap::ValueEnum;
use kbf_usage::UsageMode;

use crate::Args;

/// What `--usage-profiles` selects.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, ValueEnum)]
pub enum UsageProfiles {
    /// Nothing is learned or stored
    #[default]
    Off,
    /// Refused at start: the core that ran it was removed
    Record,
    /// Refused at start: the core that ran it was removed
    On,
}

impl UsageProfiles {
    /// The mode the farm's learning code takes.
    #[must_use]
    pub const fn mode(self) -> UsageMode {
        match self {
            Self::Off => UsageMode::Off,
            Self::Record => UsageMode::Record,
            Self::On => UsageMode::On,
        }
    }

    /// The value the flag takes and the gauge's `mode` label carries.
    #[must_use]
    pub fn name(self) -> &'static str {
        self.mode().as_str()
    }

    /// The line a start logs: the mode it runs.
    #[must_use]
    pub fn start_line(self) -> String {
        format!("usage profiles: {}", self.name())
    }
}

/// A `--usage-profiles` that the server cannot run.
#[derive(Debug, thiserror::Error)]
pub enum UsageProfilesError {
    /// `record` or `on`, which the removed js core ran.
    #[error(
        "--usage-profiles {0} needs the js scheduler core, which was removed: scheduling runs \
         in the job service; leave the flag out or use --usage-profiles off"
    )]
    JsCoreRemoved(&'static str),
}

impl Args {
    /// Refuses a `--usage-profiles` other than `off`.
    ///
    /// # Errors
    /// [`UsageProfilesError::JsCoreRemoved`]: `record` or `on`.
    pub(crate) fn usage_profiles_fit(&self) -> Result<(), UsageProfilesError> {
        match self.usage_profiles {
            UsageProfiles::Off => Ok(()),
            mode => Err(UsageProfilesError::JsCoreRemoved(mode.name())),
        }
    }
}

/// Appends the gauge `kbf_usage_profiles_mode{mode}` to `out`: 1 for `running`, 0 for each
/// other mode.
pub(crate) fn render(out: &mut String, running: UsageProfiles) {
    let name = "kbf_usage_profiles_mode";
    let _ = writeln!(
        out,
        "# HELP {name} The usage profiles mode the server runs: 1 for it, 0 for the others\n\
         # TYPE {name} gauge"
    );
    for mode in UsageProfiles::value_variants() {
        let value = u8::from(*mode == running);
        let _ = writeln!(out, "{name}{{mode=\"{}\"}} {value}", mode.name());
    }
}

#[cfg(test)]
mod tests;
