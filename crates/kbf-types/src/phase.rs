//! A node's phase: what it is doing besides running leases.

use crate::LeaseId;

/// What a worker says it is doing besides running leases: the `phase` of its `Hello`
/// (`docs/design/fleet-updates.md` section 3.3). The scheduler offers a worker a lease
/// only while it is [`NodePhase::Serving`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum NodePhase {
    /// It takes leases.
    #[default]
    Serving,
    /// It switches the GUI session to the user of this whole-machine lease, which it
    /// holds, then runs the lease's action there. The switch restarts the daemon: the
    /// scheduler keeps the lease while the worker is silent for up to its session
    /// switch grace (`kbf_sched::fence::SESSION_SWITCH_GRACE`), and sends its `Start`
    /// again to the session that names it after the restart.
    Preparing(LeaseId),
    /// It switches the session back after this whole-machine lease's action ended, or
    /// after the daemon restarted while the action ran (its processes are killed and
    /// the lease fails: its result is lost; it is never run again), and cleans up; the
    /// lease's result is sent when it is done. Kept like
    /// [`NodePhase::Preparing`].
    Restoring(LeaseId),
    /// Its cleanup failed, and why. It takes no lease until an operator repairs it.
    Quarantined(String),
    /// Its memory is under pressure (the kernel's memory stall time is above the
    /// daemon's `--memory-pressure-pause-percent`) while it holds leases: it takes no
    /// new lease until the pressure falls, and keeps the ones it runs. It says so only
    /// while it holds one, so it is never left paused with nothing running.
    MemoryPressure,
    /// The daemon was told to stop and drains: it takes no new lease, runs the ones it
    /// holds to their end, reports them and waits for their acknowledgements, then
    /// exits. The scheduler keeps its leases while it heartbeats, as for any phase that
    /// takes no lease.
    Draining,
}

impl NodePhase {
    /// The whole-machine lease whose session switch is under way, if any.
    #[must_use]
    pub const fn switching(&self) -> Option<LeaseId> {
        match self {
            Self::Preparing(lease) | Self::Restoring(lease) => Some(*lease),
            Self::Serving | Self::Quarantined(_) | Self::MemoryPressure | Self::Draining => None,
        }
    }

    /// The phase as an operator reads it: `serving`, `preparing`, `restoring`,
    /// `quarantined`, `memory_pressure` or `draining`.
    #[must_use]
    pub const fn name(&self) -> &'static str {
        match self {
            Self::Serving => "serving",
            Self::Preparing(_) => "preparing",
            Self::Restoring(_) => "restoring",
            Self::Quarantined(_) => "quarantined",
            Self::MemoryPressure => "memory_pressure",
            Self::Draining => "draining",
        }
    }
}
