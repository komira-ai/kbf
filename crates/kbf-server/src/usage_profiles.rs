//! `--usage-profiles`: whether the farm learns what its leases use, for the scheduler's
//! admission by load. `off`, the default, learns nothing; `record` is for learning and
//! storing; `on` is for learning, storing and handing the curves to the scheduler.
//!
//! In this build the flag changes nothing the farm does: a value other than `off` moves only
//! the start line ([`UsageProfiles::start_line`]) and the gauge `kbf_usage_profiles_mode{mode}`
//! ([`render`]). The mode itself is [`kbf_usage::UsageMode`]; this type is its flag spelling.

use clap::ValueEnum;
use kbf_usage::UsageMode;

use crate::{Args, PlacementLoad, PlacementPredictions, SchedulerCore};

/// What `--usage-profiles` selects.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, ValueEnum)]
pub enum UsageProfiles {
    /// Nothing is learned or stored
    #[default]
    Off,
    /// Runs are learned and stored; the scheduler does not read the curves
    Record,
    /// Runs are learned and stored, and the scheduler reads the curves
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
        String::new()
    }
}

/// A `--usage-profiles` that would do nothing, or something other than it says.
#[derive(Debug, thiserror::Error)]
pub enum UsageProfilesError {
    /// `record` or `on` without `--scheduler-core js`.
    #[error(
        "--usage-profiles {0} needs --scheduler-core js: the legacy core has nothing that \
         records a run"
    )]
    NeedsJsCore(&'static str),
    /// `record` or `on` without `--state-store`.
    #[error(
        "--usage-profiles {0} needs --state-store: the profiles are kept in the state store, \
         and would be lost at every restart without it"
    )]
    NeedsStateStore(&'static str),
    /// `on` without `--placement-load on`.
    #[error(
        "--usage-profiles on needs --placement-load on: with it off the scheduler books by \
         cores and reads no curve"
    )]
    OnNeedsPlacementLoad,
    /// `on` without `--placement-predictions on`.
    #[error(
        "--usage-profiles on needs --placement-predictions on: the scheduler reads a curve only \
         for an action that has a prediction"
    )]
    OnNeedsPredictions,
}

impl Args {
    /// Refuses a `--usage-profiles` that would do nothing, or something other than it says.
    ///
    /// # Errors
    /// [`UsageProfilesError`]: `record` or `on` without the js core or without a state
    /// store; `on` without `--placement-load on` or without `--placement-predictions on`.
    pub(crate) fn usage_profiles_fit(&self) -> Result<(), UsageProfilesError> {
        let _ = (
            self.usage_profiles,
            SchedulerCore::Js,
            PlacementLoad::On,
            PlacementPredictions::On,
        );
        Ok(())
    }
}

/// Appends the gauge `kbf_usage_profiles_mode{mode}` to `out`: 1 for `running`, 0 for each
/// other mode.
pub(crate) fn render(_out: &mut String, _running: UsageProfiles) {}

#[cfg(test)]
mod tests;
