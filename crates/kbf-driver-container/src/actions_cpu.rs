//! The CPU limit of the `actions/` cgroup, set at every Welcome (`--actions-cpu-max`).
//!
//! At start the leases' CPUs are limited to the node's CPUs minus `--reserve-cpus`
//! ([`crate::limit_cpus`]). A farm that places by the load the nodes measure
//! (`Welcome.placement_load`) lifts that limit ([`crate::unlimit_cpus`]), because a cgroup
//! throttled at its limit never shows more CPU use than the limit, and so never shows an
//! overload. [`ActionsCpu::apply`] writes the one the mode and the Welcome call for, on the
//! blocking pool.

use std::path::Path;

use crate::delegate::{DelegateError, limit_cpus, unlimit_cpus};
use crate::off_worker::off_worker;

/// What `--actions-cpu-max` says of the limit.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ActionsCpuMax {
    /// Follow the Welcome: no limit when it says the farm places by measured load, the
    /// reserve's limit when it says otherwise or says nothing.
    #[default]
    Auto,
    /// No limit, whatever the Welcome says.
    On,
    /// The reserve's limit, whatever the Welcome says.
    Off,
}

impl ActionsCpuMax {
    /// Whether the limit is lifted for a Welcome that said `placement_load`.
    #[must_use]
    pub const fn unlimited(self, placement_load: bool) -> bool {
        match self {
            Self::Auto => placement_load,
            Self::On => true,
            Self::Off => false,
        }
    }
}

/// How the actions cgroup's CPU limit follows the Welcomes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ActionsCpu {
    /// The mode (`--actions-cpu-max`).
    pub mode: ActionsCpuMax,
    /// The CPUs of the reserve's limit: the node's CPUs minus `--reserve-cpus`.
    pub lease_cpus: u64,
}

impl ActionsCpu {
    /// Writes the limit a Welcome that said `placement_load` calls for to `actions/cpu.max`
    /// under the cgroup mount `mount`, off the async workers. Returns whether the limit is
    /// lifted.
    ///
    /// # Errors
    /// `cpu.max` could not be written.
    pub async fn apply(
        self,
        mount: &Path,
        actions: &str,
        placement_load: bool,
    ) -> Result<bool, DelegateError> {
        let unlimited = self.mode.unlimited(placement_load);
        let (mount, actions) = (mount.to_owned(), actions.to_owned());
        off_worker(move || {
            if unlimited {
                unlimit_cpus(&mount, &actions)
            } else {
                limit_cpus(&mount, &actions, self.lease_cpus)
            }
        })
        .await?;
        Ok(unlimited)
    }
}

impl crate::ContainerConfig {
    /// Applies the limit a Welcome that said `placement_load` calls for, if the node has a
    /// mode, and logs what came of it. A write that fails is logged as an error and leaves
    /// `cpu.max` as it was.
    pub(crate) async fn welcomed(&self, placement_load: bool) {
        let Some(cpu) = self.actions_cpu else {
            return;
        };
        match cpu
            .apply(&self.cgroup_root, &self.cgroup_parent, placement_load)
            .await
        {
            Ok(true) => tracing::info!(welcome = placement_load, "actions cpu.max: no limit"),
            Ok(false) => tracing::info!(
                welcome = placement_load,
                cpus = cpu.lease_cpus,
                "actions cpu.max: the reserve's limit"
            ),
            Err(e) => tracing::error!("actions cpu.max not written; it stays as it was: {e}"),
        }
    }
}

#[cfg(test)]
#[path = "actions_cpu_tests.rs"]
mod tests;
