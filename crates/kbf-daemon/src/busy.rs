//! How busy the node's CPUs were: the CPU time the actions cgroup used between two
//! heartbeats, which the server's CPU admission reads (`Heartbeat.busy_millis`).
//!
//! The source is `usage_usec` in the actions cgroup's `cpu.stat`: a counter of the CPU
//! microseconds that cgroup and everything under it ever used, which only grows (it
//! keeps what lease cgroups used before they were removed). [`CpuMeter::sample`] is
//! called once per heartbeat and returns the growth since the previous call, scaled to
//! CPU-milliseconds per second. It is the last window's rate, not an average over
//! several: the server pairs each reading with a cap on leases admitted since it, and a
//! smoothed rate would keep admitting for several windows after the node filled up.
//!
//! Measurement only, like the usage a result carries: a counter that cannot be read or
//! that went backwards (the cgroup was made again) gives no reading for that window, and
//! the next window starts from the new value. No reading is "unknown", never "idle".

use std::path::PathBuf;
use std::time::Duration;

use crate::clock::Moment;

/// The `usage_usec` of a `cpu.stat`; `None` if there is no such line or it is not a
/// number.
#[must_use]
pub fn usage_usec(text: &str) -> Option<u64> {
    text.lines().find_map(|line| {
        let mut words = line.split_whitespace();
        (words.next()? == "usage_usec").then(|| words.next()?.parse().ok())?
    })
}

/// CPU-milliseconds per second for `used_usec` of CPU time over `elapsed`; `None` for a
/// zero `elapsed`, which has no rate.
#[must_use]
pub fn busy_millis(used_usec: u64, elapsed: Duration) -> Option<u64> {
    let elapsed_usec = elapsed.as_micros();
    if elapsed_usec == 0 {
        return None;
    }
    // used / elapsed CPU-seconds per second, times 1000.
    let millis = u128::from(used_usec) * 1_000 / elapsed_usec;
    Some(u64::try_from(millis).unwrap_or(u64::MAX))
}

/// Reads the actions cgroup's CPU counter once per heartbeat.
#[derive(Debug)]
pub struct CpuMeter {
    stat: PathBuf,
    /// The counter and when it was read, at the previous call.
    last: Option<(u64, Moment)>,
}

impl CpuMeter {
    /// A meter of the `cpu.stat` file at `stat`.
    #[must_use]
    pub fn new(stat: PathBuf) -> Self {
        Self { stat, last: None }
    }

    /// The busy rate since the previous call, read at `now`; `None` for the first call,
    /// when the file cannot be read or has no counter, when the counter is lower than
    /// before, or when no time has passed.
    pub fn sample(&mut self, now: Moment) -> Option<u64> {
        let read = std::fs::read_to_string(&self.stat)
            .ok()
            .and_then(|text| usage_usec(&text));
        let before = std::mem::replace(&mut self.last, read.map(|usage| (usage, now)));
        let ((was, then), usage) = before.zip(read)?;
        busy_millis(usage.checked_sub(was)?, now.saturating_duration_since(then))
    }
}

#[cfg(test)]
#[path = "busy_tests.rs"]
mod tests;
