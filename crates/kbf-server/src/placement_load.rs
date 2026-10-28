//! `--placement-load`: whether the farm's placement reads the load the nodes report.
//!
//! kbf builds every scheduler with an explicit load configuration ([`PlacementLoad::config`])
//! instead of taking the scheduler library's default, so that raising the library's revision
//! cannot change where the farm places work. The server takes it from this flag; the test
//! farm, the simulation cell and the estimator replay pass [`PlacementLoad::Off`].

use kbf_sched::LoadConfig;

/// What `--placement-load` does with the load.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, clap::ValueEnum)]
pub enum PlacementLoad {
    /// Placement books by the CPUs a lease asks for, as it always has; the load is not read.
    #[default]
    Off,
    /// Placement is as `off`, and the scheduler counts what the load rule would have decided.
    Shadow,
    /// Placement admits and ranks nodes by the load they report and the load a lease is
    /// predicted to add.
    On,
}

impl PlacementLoad {
    /// The scheduler's name for this mode.
    #[must_use]
    pub const fn mode(self) -> kbf_sched::PlacementLoad {
        match self {
            Self::Off => kbf_sched::PlacementLoad::Off,
            Self::Shadow => kbf_sched::PlacementLoad::Shadow,
            Self::On => kbf_sched::PlacementLoad::On,
        }
    }

    /// The load configuration a scheduler is built with: this mode, and the scheduler's own
    /// values for every other setting.
    #[must_use]
    pub fn config(self) -> LoadConfig {
        LoadConfig {
            mode: self.mode(),
            ..LoadConfig::default()
        }
    }
}

#[cfg(test)]
mod tests;
