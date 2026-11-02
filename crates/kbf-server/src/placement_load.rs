//! `--placement-load`: whether the farm's placement reads the load the nodes report.
//!
//! kbf builds every scheduler with an explicit load configuration ([`PlacementLoad::config`])
//! instead of taking the scheduler library's default, so that raising the library's revision
//! cannot change where the farm places work: the mode from the flag and the profile switches
//! written out in this file, which the tests pin. The server takes it from this flag; the test
//! farm, the simulation cell and the estimator replay pass [`PlacementLoad::Off`].

use kbf_sched::LoadConfig;

/// What `--placement-load` does with the load.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, clap::ValueEnum)]
pub enum PlacementLoad {
    /// Placement books by the CPUs a lease asks for, as it always has; the load is not read.
    #[default]
    Off,
    /// Placement admits and ranks nodes by the load they report and the load a lease is
    /// predicted to add.
    On,
}

/// What a lease with a learned profile is charged at least of its predicted cores, in
/// thousandths (`f` of the scheduler: a constant-duty lease is charged all of them).
pub const CPU_FLOOR_PERMILLE: u32 = 1000;
/// Whether a key with a learned profile is charged its cores per minute and checked over the
/// next half hour. Read only under `--usage-profiles on`, which gives the scheduler the curves
/// to read; under `off` and `record` it has none, and the switch charges nothing.
pub const PROFILE: bool = true;
/// Whether a key with a learned profile is charged the profile's remaining maximum memory.
pub const PROFILE_MEMORY: bool = true;
/// The least share of its flat cores, in thousandths, a profile charges a lease (`g`).
pub const PROFILE_FLOOR_PERMILLE: u32 = 750;
/// How many minutes beyond the P90 of its key's wall time a new lease is charged for.
pub const EXPECTED_LIFE_DELTA_MINUTES: u32 = 2;
/// The node guard on flat memory, in thousandths: the flat memory charged to the leases of a
/// worker, an action's own included, stays within the worker's memory.
pub const FLAT_MEMORY_SLACK_PERMILLE: u32 = 0;

impl PlacementLoad {
    /// The scheduler's name for this mode.
    #[must_use]
    pub const fn mode(self) -> kbf_sched::PlacementLoad {
        match self {
            Self::Off => kbf_sched::PlacementLoad::Off,
            Self::On => kbf_sched::PlacementLoad::On,
        }
    }

    /// What `Welcome.placement_load` carries: `Some(true)` for `on`, absent for `off` (never
    /// `Some(false)`, so a farm that does not place by load sends the bytes it always did).
    #[must_use]
    pub const fn welcome(self) -> Option<bool> {
        match self {
            Self::On => Some(true),
            Self::Off => None,
        }
    }

    /// The load configuration a scheduler is built with: this mode, the profile switches
    /// written out here (the values the scheduler library has today, which a raise of its
    /// revision does not change under the farm), and the scheduler's own values for every
    /// other setting.
    #[must_use]
    pub fn config(self) -> LoadConfig {
        LoadConfig {
            mode: self.mode(),
            cpu_floor_permille: CPU_FLOOR_PERMILLE,
            profile: PROFILE,
            profile_memory: PROFILE_MEMORY,
            profile_floor_permille: PROFILE_FLOOR_PERMILLE,
            expected_life_delta_minutes: Some(EXPECTED_LIFE_DELTA_MINUTES),
            flat_memory_slack_permille: Some(FLAT_MEMORY_SLACK_PERMILLE),
            ..LoadConfig::default()
        }
    }
}

#[cfg(test)]
mod tests;
