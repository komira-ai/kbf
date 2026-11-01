//! The load a host's agent reported, as the lease server lists it, in the form the farm
//! takes on a heartbeat.
//!
//! The executor polls the host list on its own cadence (`Config::host_check`) and not on
//! the heartbeat's, so a reading is kept with the time it was first seen and is handed to the
//! farm once, with the first heartbeat after it: the farm drops a reading whose `seq` is not
//! above the last it took, and counts the lists of what it drops as refused. A reading is
//! never repeated to look fresh and never made up: a host the server lists with no load,
//! and a reading older than the farm would believe, send `None`.
//!
//! Stale is the farm's own rule (three windows, a window being the span between the
//! reading and the one before it) plus one poll: the reading may have been
//! taken just before the poll that showed it.

use std::time::Duration;

use kbf_lease_client::{HostLoad, LeaseId};
use kbf_proto::worker::{self, NodeLoad};
use tokio::time::Instant;

/// How many windows without a reading the farm waits before it calls the load stale.
const STALE_AFTER_WINDOWS: u32 = 3;

/// The newest reading of a host, as polled.
pub(super) struct Seen {
    load: HostLoad,
    at: Instant,
    /// The time between this reading and the one before it from the same agent process, on
    /// the agent's clock; `None` for the first reading seen or a new process.
    window: Option<Duration>,
    handed: bool,
}

impl Seen {
    /// What a poll that listed the host with `listed` makes of the reading `before`: a new
    /// reading is kept with `now`, the same one stays as it was, and no load is no reading.
    pub(super) fn after_poll(
        before: Option<Self>,
        listed: Option<&HostLoad>,
        now: Instant,
    ) -> Option<Self> {
        let listed = listed?;
        match before {
            Some(same) if same.load == *listed => Some(same),
            before => {
                let window = before
                    .as_ref()
                    .filter(|b| b.load.seq < listed.seq)
                    .and_then(|b| listed.mono_usec.checked_sub(b.load.mono_usec))
                    .filter(|usec| *usec > 0)
                    .map(Duration::from_micros);
                Some(Self {
                    load: listed.clone(),
                    at: now,
                    window,
                    handed: false,
                })
            }
        }
    }

    /// The reading for the heartbeat at `now`, once: `None` if it was handed already, or is
    /// stale. `poll` is the interval of the host polls, the window assumed for a reading
    /// that has no earlier one.
    pub(super) fn take(&mut self, now: Instant, poll: Duration) -> Option<NodeLoad> {
        if std::mem::replace(&mut self.handed, true) {
            return None;
        }
        let window = self.window.unwrap_or(poll);
        let limit = window * STALE_AFTER_WINDOWS + poll;
        (now.saturating_duration_since(self.at) <= limit).then(|| node_load(&self.load))
    }
}

/// The farm's form of `load`. A lease or minute the farm cannot name (text that is not
/// `term-seq`) is left out alone; a figure the server does not send is absent, not zero.
fn node_load(load: &HostLoad) -> NodeLoad {
    let lease = |text: &str| {
        text.parse::<LeaseId>().ok().map(|id| worker::LeaseId {
            term: id.term(),
            seq: id.sequence(),
        })
    };
    NodeLoad {
        seq: load.seq,
        mono_usec: load.mono_usec,
        cpu_usage_usec: load.cpu_usage_usec,
        memory_working_set_bytes: load.memory_working_set_bytes,
        memory_active_file_bytes: load.memory_active_file_bytes,
        actions_cpu_limited: load.actions_cpu_limited,
        covered: load.covered.iter().filter_map(|l| lease(l)).collect(),
        lease_clocks: load
            .lease_clocks
            .iter()
            .filter_map(|c| {
                Some(worker::LeaseClock {
                    lease: Some(lease(&c.lease)?),
                    started_mono_usec: c.started_mono_usec,
                })
            })
            .collect(),
        minutes: load
            .minutes
            .iter()
            .filter_map(|m| {
                Some(worker::LeaseMinute {
                    lease: Some(lease(&m.lease)?),
                    minute: m.minute,
                    cpu_mean_millis: m.cpu_mean_millis,
                    cpu_peak_window_millis: m.cpu_peak_window_millis,
                    ws_max_bytes: m.ws_max_bytes,
                })
            })
            .collect(),
        cpu_throttled_usec: None,
        cpu_pressure_some_total_usec: None,
    }
}

#[cfg(test)]
#[path = "load_tests.rs"]
mod tests;
