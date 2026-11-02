//! `--usage-profiles`: whether the farm learns what its leases use, for the scheduler's
//! admission by load. `off`, the default, learns nothing; `record` is for learning and
//! storing; `on` is for learning, storing and handing the curves to the scheduler.
//!
//! `record` learns and stores the profiles from the runs the farm hears of
//! ([`crate::farm_js::usage`]) and gives the scheduler no curve, so it places as `off` does. `on`
//! learns and stores the same and gives the scheduler the curves, so an action with a
//! prediction and a curve is charged by the curve and `on` can place work where `record` would
//! queue it. `off` does none of it and writes no row. A value
//! other than `off` also moves the start line ([`UsageProfiles::start_line`]) and the gauge
//! `kbf_usage_profiles_mode{mode}` ([`render`]). The mode itself is [`kbf_usage::UsageMode`]; this type is its flag spelling.

use std::fmt::Write as _;

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
        format!("usage profiles: {}", self.name())
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
        let mode = self.usage_profiles;
        if mode == UsageProfiles::Off {
            return Ok(());
        }
        if self.scheduler_core != SchedulerCore::Js {
            return Err(UsageProfilesError::NeedsJsCore(mode.name()));
        }
        if self.state_store.is_none() {
            return Err(UsageProfilesError::NeedsStateStore(mode.name()));
        }
        if mode == UsageProfiles::On {
            if self.placement_load != PlacementLoad::On {
                return Err(UsageProfilesError::OnNeedsPlacementLoad);
            }
            if self.placement_predictions != PlacementPredictions::On {
                return Err(UsageProfilesError::OnNeedsPredictions);
            }
        }
        Ok(())
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
