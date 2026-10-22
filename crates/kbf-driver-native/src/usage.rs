//! What a lease cost, as the macOS driver reports it: a [`ResourceUsage`] with
//! `basis = MacFootprint` and a `footprint` block, never a cgroup block.
//!
//! The numbers come from `proc_pid_rusage` ([`crate::procs::rusage`]), read for every
//! process of the lease on each memory poll ([`Meter::sample`]):
//!
//! - **Peak memory** is the sum, over every process the lease ran that a poll saw, of
//!   that process's `ri_lifetime_max_phys_footprint`: a high-water mark the kernel
//!   keeps, not a sample of the poll. Processes peak at different moments, so the sum is
//!   an upper bound of the tree's simultaneous peak, not the peak itself.
//! - **CPU** is the sum, over the same processes, of the last `ri_user_time` and
//!   `ri_system_time` a poll read (mach absolute time, converted with the machine's
//!   timebase).
//! - **Wall** is the lease's action start to the end of its watch.
//!
//! What is undercounted, honestly. Both figures are lower bounds on what the lease used:
//!
//! - A process that started and exited between two polls was never read, so its memory
//!   and CPU are missing. The first poll runs when the action starts and the default
//!   interval is 250 ms, so an action that ends within one interval reports almost
//!   nothing for peak and CPU (its wall time is still exact).
//! - A process's CPU after the poll that last read it is missing. One last read runs
//!   before the watch ends the processes, which closes that for any process still alive
//!   when the leader exits.
//! - The action's leader is reaped by the runtime, which gets no `rusage` for it, so its
//!   figures stop at the poll that last read it. `RUSAGE_CHILDREN` of the daemon is not
//!   used: it counts every child the daemon reaped, other leases' included.
//! - A pid the kernel reuses for a new process whose counters are not below the old
//!   one's within a lease is taken for the same process.
//!
//! The pure accounting here is tested on every platform with a fake process table; only
//! the reading ([`crate::procs::rusage`]) and the timebase are macOS.

use std::collections::BTreeMap;

use std::sync::Mutex;

use kbf_proto::worker::{FootprintUsage, ResourceUsage, UsageBasis};

use crate::procs::Rusage;

/// What the kernel keeps for one process over its life.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Lifetime {
    /// `ri_lifetime_max_phys_footprint`: the most physical footprint the process held,
    /// in bytes.
    pub max_footprint: u64,
    /// `ri_user_time`, in mach absolute time units.
    pub user_ticks: u64,
    /// `ri_system_time`, in mach absolute time units.
    pub system_ticks: u64,
}

impl Lifetime {
    fn cpu(self) -> u64 {
        self.user_ticks.saturating_add(self.system_ticks)
    }
}

/// The processes of one lease that polls have read, and what they last read of each.
#[derive(Debug, Default)]
pub struct Meter {
    /// The last reading of each pid still being followed.
    live: BTreeMap<i32, Lifetime>,
    /// Readings of earlier processes that held a pid the lease then saw a different
    /// process hold (the counters of one process never go down).
    retired: Vec<Lifetime>,
}

impl Meter {
    /// Reads every pid of `pids` with `read`, records what it gave of each, and returns
    /// their footprints now, summed. A pid `read` has nothing for (gone, or not readable)
    /// keeps what it had and adds nothing to the sum.
    pub fn poll(
        &mut self,
        pids: impl IntoIterator<Item = i32>,
        read: impl Fn(i32) -> Option<Rusage>,
    ) -> u64 {
        let mut now = 0_u64;
        for pid in pids {
            let Some(r) = read(pid) else {
                continue;
            };
            now = now.saturating_add(r.footprint);
            if let Some(lifetime) = r.lifetime {
                self.record(pid, lifetime);
            }
        }
        now
    }

    /// Records `now` as the latest reading of `pid`. A reading below the one before
    /// (less memory at the high-water mark, or less CPU) cannot be the same process: the
    /// pid was reused, and the earlier process is kept apart.
    pub fn record(&mut self, pid: i32, now: Lifetime) {
        if let Some(before) = self.live.insert(pid, now)
            && (now.max_footprint < before.max_footprint || now.cpu() < before.cpu())
        {
            self.retired.push(before);
        }
    }

    /// The usage to report after `wall`, with `timebase` (numerator, denominator) turning
    /// mach ticks into nanoseconds. Peak and CPU are absent when no process was ever read.
    #[must_use]
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub fn usage(&self, wall: std::time::Duration, timebase: (u32, u32)) -> ResourceUsage {
        let all = || self.live.values().chain(&self.retired);
        let read = all().next().is_some();
        let sum = |f: fn(&Lifetime) -> u64| all().map(f).fold(0_u64, u64::saturating_add);
        let micros = |ticks| ticks_to_micros(ticks, timebase);
        let wall_micros = u64::try_from(wall.as_micros()).unwrap_or(u64::MAX);
        let (peak, user, system) = (
            sum(|l| l.max_footprint),
            micros(sum(|l| l.user_ticks)),
            micros(sum(|l| l.system_ticks)),
        );
        ResourceUsage {
            cpu_user_micros: user,
            cpu_system_micros: system,
            peak_memory_bytes: peak,
            wall_micros,
            cgroup: None,
            footprint: Some(FootprintUsage {
                peak_footprint_bytes: read.then_some(peak),
                cpu_user_micros: read.then_some(user),
                cpu_system_micros: read.then_some(system),
                wall_micros: Some(wall_micros),
            }),
            basis: UsageBasis::MacFootprint.into(),
            node_cpu_pressure_centi: None,
            booked_millicpus: None,
        }
    }
}

/// `ticks` of mach absolute time as microseconds, with the machine's timebase
/// (`numer / denom` nanoseconds per tick). A zero denominator reads as 1.
#[must_use]
pub fn ticks_to_micros(ticks: u64, (numer, denom): (u32, u32)) -> u64 {
    let nanos = u128::from(ticks) * u128::from(numer) / u128::from(denom.max(1));
    u64::try_from(nanos / 1000).unwrap_or(u64::MAX)
}

/// The usage of a lease that ran for `wall`, as the platform reports it: on macOS what
/// `meter` read; elsewhere nothing (the native driver on Linux has no footprint basis,
/// and no cgroup of its own to report).
#[cfg(target_os = "macos")]
pub fn report(meter: &Mutex<Meter>, wall: std::time::Duration) -> Option<ResourceUsage> {
    let meter = meter
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    Some(meter.usage(wall, crate::procs::timebase()))
}

/// See the macOS [`report`]: nothing on this platform.
#[cfg(not(target_os = "macos"))]
pub fn report(_meter: &Mutex<Meter>, _wall: std::time::Duration) -> Option<ResourceUsage> {
    None
}

#[cfg(test)]
#[path = "usage_tests.rs"]
mod tests;
