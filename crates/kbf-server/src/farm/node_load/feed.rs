//! What the lists of accepted node-load readings come to, as the node view shows it, and the
//! conversion of an accepted reading into the load the scheduler takes.

use kbf_proto::Vetted;
use kbf_proto::worker::{LeaseId as WireLease, NodeLoad};
use kbf_types::LeaseId;
use kbf_types::sched::usage::{self, LeaseClock, LeaseMinute};

use crate::fleet::{DroppedView, ListsView};

/// What the lists of one daemon process's readings have come to since it began: the records
/// kept, the records dropped by reason, and the minutes of the readings the accept rule
/// refused. The counts of a daemon that restarts begin again.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Lists {
    clocks: u64,
    minutes: u64,
    invalid: u64,
    duplicate: u64,
    over_cap: u64,
    refused_minutes: u64,
}

impl Lists {
    /// Counts the records of an accepted reading's lists: those kept and those dropped.
    pub(super) fn took(&mut self, vetted: &Vetted) {
        self.clocks = self.clocks.saturating_add(count(vetted.clocks.len()));
        self.minutes = self.minutes.saturating_add(count(vetted.minutes.len()));
        let drops = vetted.drops;
        self.invalid = self.invalid.saturating_add(u64::from(drops.invalid));
        self.duplicate = self.duplicate.saturating_add(u64::from(drops.duplicate));
        self.over_cap = self.over_cap.saturating_add(u64::from(drops.over_cap));
    }

    /// Counts the `minutes` of a reading the accept rule refused.
    pub(super) fn refused(&mut self, minutes: usize) {
        self.refused_minutes = self.refused_minutes.saturating_add(count(minutes));
    }

    /// The counts as the node view shows them.
    pub(super) fn view(&self) -> ListsView {
        ListsView {
            clocks: self.clocks,
            minutes: self.minutes,
            dropped: DroppedView {
                invalid: self.invalid,
                duplicate: self.duplicate,
                over_cap: self.over_cap,
            },
            refused_minutes: self.refused_minutes,
        }
    }
}

fn count(n: usize) -> u64 {
    u64::try_from(n).unwrap_or(u64::MAX)
}

fn lease(id: &WireLease) -> LeaseId {
    LeaseId::new(id.term, id.seq)
}

/// `load` as the scheduler takes it, with the lists of `vetted` (what `NodeLoad::vet` kept
/// of `load`'s own): the scalars are copied, the two telemetry counters the daemon may leave
/// out read as 0 (the scheduler's fields are plain numbers and only shown), and a record that
/// names no lease is left out (`vet` has dropped each, so none is).
///
/// A free function: both types are foreign here, so `From` cannot be written. Whatever else
/// hands a daemon's load to the scheduler calls this one.
pub(crate) fn sched_load(load: &NodeLoad, vetted: &Vetted) -> usage::NodeLoad {
    usage::NodeLoad {
        seq: load.seq,
        mono_usec: load.mono_usec,
        cpu_usage_usec: load.cpu_usage_usec,
        memory_working_set_bytes: load.memory_working_set_bytes,
        memory_active_file_bytes: load.memory_active_file_bytes,
        covered: vetted.covered.iter().map(lease).collect(),
        actions_cpu_limited: load.actions_cpu_limited,
        cpu_throttled_usec: load.cpu_throttled_usec.unwrap_or(0),
        cpu_pressure_some_total_usec: load.cpu_pressure_some_total_usec.unwrap_or(0),
        lease_clocks: vetted
            .clocks
            .iter()
            .filter_map(|c| {
                Some(LeaseClock {
                    lease: lease(c.lease.as_ref()?),
                    started_mono_usec: c.started_mono_usec,
                })
            })
            .collect(),
        minutes: vetted
            .minutes
            .iter()
            .filter_map(|m| {
                Some(LeaseMinute {
                    lease: lease(m.lease.as_ref()?),
                    minute: m.minute,
                    cpu_mean_millis: m.cpu_mean_millis,
                    cpu_peak_window_millis: m.cpu_peak_window_millis,
                    ws_max_bytes: m.ws_max_bytes,
                })
            })
            .collect(),
    }
}

#[cfg(test)]
mod tests;
