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
//! - **CPU** is the leader's reaped total ([`crate::leader`]: `wait4`'s `rusage`, the CPU
//!   of the leader and of every descendant its chain of parents reaped, exact) combined
//!   with what the polls read of the processes the reap does not hold ([`Meter::usage`]
//!   says how). A poll reads `ri_user_time`, `ri_system_time` and the `ri_child_*` pair
//!   (mach absolute time, converted with the machine's timebase).
//! - **Wall** is the lease's action start to the end of its watch.
//!
//! What is still undercounted, honestly. Peak memory is a lower bound; CPU is exact for
//! everything the leader's chain reaped, and a lower bound for the rest:
//!
//! - A process that started and exited between two polls was never read, so its memory
//!   is missing. Its CPU is not, if the leader's chain reaped it.
//! - A process that left the leader's chain (its parent exited first, so the system
//!   reaped it) and exited between two polls has its CPU after the poll that last read it
//!   missing. One last read runs before the watch ends the processes, which closes that
//!   for any process still alive when the leader exits.
//!   A process that outlived its parent by less than a poll interval, and exited within
//!   it, was last read under that parent and is taken to have been reaped by it: when
//!   it was not, all of its own CPU is missing, not only what it used after the last
//!   poll. Only the window is bounded (both exit within one poll interval), not the
//!   amount ([`Meter::usage`] says why it is not added).
//! - A pid the kernel reuses for a new process whose counters are not below the old
//!   one's within a lease is taken for the same process.
//!
//! `RUSAGE_CHILDREN` of the daemon is not used: it counts every child the daemon reaped,
//! other leases' included; the leader's own `wait4` is per lease.
//!
//! The pure accounting here is tested on every platform with a fake process table; only
//! the reading ([`crate::procs::rusage`]) and the timebase are macOS.

use std::collections::{BTreeMap, BTreeSet};

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
    /// `ri_child_user_time`: user CPU of the descendants this process reaped, in mach
    /// absolute time units (each counted with what it reaped in turn).
    pub child_user_ticks: u64,
    /// `ri_child_system_time`, likewise.
    pub child_system_ticks: u64,
    /// The process's parent when it was read; 0 when unknown.
    pub ppid: i32,
}

impl Lifetime {
    /// The CPU of the process and of the descendants it has reaped, in ticks.
    fn cpu(self) -> u64 {
        self.own().saturating_add(self.reaped_children())
    }

    fn own(self) -> u64 {
        self.user_ticks.saturating_add(self.system_ticks)
    }

    fn reaped_children(self) -> u64 {
        self.child_user_ticks
            .saturating_add(self.child_system_ticks)
    }

    /// (user, system) ticks of the process and its reaped descendants.
    fn split(self) -> (u64, u64) {
        (
            self.user_ticks.saturating_add(self.child_user_ticks),
            self.system_ticks.saturating_add(self.child_system_ticks),
        )
    }
}

/// What reaping the lease's leader gave of its CPU, in microseconds: the leader's own and
/// that of every descendant it reaped.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Reaped {
    pub user_micros: u64,
    pub system_micros: u64,
}

/// A reading and the poll that made it.
#[derive(Clone, Copy, Debug)]
struct Seen {
    life: Lifetime,
    /// The number of the poll (1 for the first) that last read the process.
    poll: u64,
}

/// The leader's reap, and the polls made before it.
#[derive(Clone, Copy, Debug)]
struct Reap {
    leader: i32,
    /// The polls done when the leader was reaped: the number of the last poll that could
    /// see it alive.
    after_poll: u64,
    total: Reaped,
}

/// The processes of one lease that polls have read, and what they last read of each.
#[derive(Debug, Default)]
pub struct Meter {
    /// The last reading of each pid still being followed.
    live: BTreeMap<i32, Seen>,
    /// Readings of earlier processes that held a pid the lease then saw a different
    /// process hold (the counters of one process never go down).
    retired: Vec<(i32, Seen)>,
    /// How many polls have been made.
    polls: u64,
    /// The leader's reap, once it happened.
    reap: Option<Reap>,
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
        self.polls += 1;
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
    /// pid was reused, and the earlier process is kept apart. A reading whose parent is
    /// unknown (0: the table lookup lost the race with the process's exit) keeps the
    /// parent the reading before gave.
    pub fn record(&mut self, pid: i32, now: Lifetime) {
        let mut now = Seen {
            life: now,
            poll: self.polls,
        };
        if let Some(before) = self.live.get(&pid).copied() {
            if now.life.max_footprint < before.life.max_footprint
                || now.life.cpu() < before.life.cpu()
            {
                self.retired.push((pid, before));
            } else if now.life.ppid == 0 {
                now.life.ppid = before.life.ppid;
            }
        }
        self.live.insert(pid, now);
    }

    /// Records that the lease's leader, `leader`, was reaped with `total`: its own CPU and
    /// that of every descendant it reaped, from the kernel's own accounting at the reap.
    /// Call it when the reap happens: the polls made before it saw the leader alive, the
    /// polls after it did not.
    pub fn reaped(&mut self, leader: i32, total: Reaped) {
        self.reap = Some(Reap {
            leader,
            after_poll: self.polls,
            total,
        });
    }

    /// The pids read at poll `last` that the leader's chain cannot reap, once the leader
    /// is known: those whose parent, as that very poll read it, is neither the leader nor
    /// a process of the same poll, and everything below them. They were alive at the
    /// poll, so the parent is not stale; a parent of 0 (unknown) is taken to be in the
    /// chain. Empty without a leader.
    fn orphans_at(&self, last: u64, leader: Option<i32>) -> BTreeSet<i32> {
        let Some(leader) = leader else {
            return BTreeSet::new();
        };
        let at_last: BTreeMap<i32, i32> = self
            .live
            .iter()
            .chain(self.retired.iter().map(|(p, s)| (p, s)))
            .filter(|(_, s)| s.poll == last)
            .map(|(&pid, s)| (pid, s.life.ppid))
            .collect();
        let mut orphans = BTreeSet::new();
        loop {
            let before = orphans.len();
            for (&pid, &ppid) in &at_last {
                let outside = ppid != 0 && ppid != leader && !at_last.contains_key(&ppid);
                if pid != leader && (outside || orphans.contains(&ppid)) {
                    orphans.insert(pid);
                }
            }
            if orphans.len() == before {
                return orphans;
            }
        }
    }

    /// The usage to report after `wall`, with `timebase` (numerator, denominator) turning
    /// mach ticks into nanoseconds. Peak and CPU are absent when no process was ever read.
    ///
    /// The CPU is `max(reaped, alive) + orphans + after`, in microseconds:
    /// - `reaped`: what the leader's reap gave. It holds every descendant the leader's
    ///   chain of parents reaped, the leader's last tick and all the children that ran
    ///   and exited between two polls; exact for those.
    /// - `alive`: the CPU, own and reaped children's, of the processes the last poll before
    ///   the reap read, but not the orphans (below). Each is a lower bound of the same CPU
    ///   `reaped` counts when the leader's chain reaped them, so the larger of the two is
    ///   taken, never the sum (a killed leader's children may or may not have been reaped
    ///   into `reaped`). With no reap, that last poll is the last one made.
    /// - `orphans`: a process whose parent is no process of the lease: reparented to the
    ///   system, its CPU is in nobody's reaped total, so it is added to the larger of the
    ///   two, once. Two kinds. One alive at the last poll before the reap whose parent,
    ///   as that poll read it (so not stale), is neither the leader nor a process of that
    ///   poll, with everything below it. One that left before that poll, last read with a
    ///   parent no poll saw. A process that left before that poll under a parent some
    ///   poll saw is taken to be inside the parent's reaped children (and so in `reaped`),
    ///   not added again. That reading of the parent can be stale: if the parent exited
    ///   and the child outlived it within one poll interval, all of the child's own CPU
    ///   is in no total (its last reading is skipped whole), and nothing a poll saw tells
    ///   it from a child reaped by its parent first (the
    ///   other way round would add every short-lived child on top of `reaped`).
    /// - `after`: the processes last read after the reap: alive when the leader was gone,
    ///   so in no reaped total.
    #[must_use]
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub fn usage(&self, wall: std::time::Duration, timebase: (u32, u32)) -> ResourceUsage {
        let all = || {
            self.live
                .iter()
                .chain(self.retired.iter().map(|(p, s)| (p, s)))
        };
        let sampled = all().next().is_some();
        let read = sampled || self.reap.is_some();
        let sum = |f: fn(&Lifetime) -> u64| {
            all()
                .map(|(_, s)| f(&s.life))
                .fold(0_u64, u64::saturating_add)
        };
        let micros = |ticks| ticks_to_micros(ticks, timebase);
        let wall_micros = u64::try_from(wall.as_micros()).unwrap_or(u64::MAX);
        let peak = sum(|l| l.max_footprint);

        let last = self.reap.map_or(self.polls, |r| r.after_poll);
        let leader = self.reap.map(|r| r.leader);
        let polled: BTreeSet<i32> = all().map(|(&pid, _)| pid).chain(leader).collect();
        let orphans = self.orphans_at(last, leader);
        let (mut alive, mut extra) = ((0_u64, 0_u64), (0_u64, 0_u64));
        for (&pid, seen) in all() {
            let (user, system) = seen.life.split();
            let into = if seen.poll > last {
                &mut extra
            } else if seen.poll == last {
                if orphans.contains(&pid) {
                    &mut extra
                } else {
                    &mut alive
                }
            } else if Some(pid) == leader || polled.contains(&seen.life.ppid) {
                continue;
            } else {
                &mut extra
            };
            into.0 = into.0.saturating_add(user);
            into.1 = into.1.saturating_add(system);
        }
        let alive = (micros(alive.0), micros(alive.1));
        let reaped = self
            .reap
            .map_or((0, 0), |r| (r.total.user_micros, r.total.system_micros));
        let base = if reaped.0.saturating_add(reaped.1) >= alive.0.saturating_add(alive.1) {
            reaped
        } else {
            alive
        };
        let (user, system) = (
            base.0.saturating_add(micros(extra.0)),
            base.1.saturating_add(micros(extra.1)),
        );
        ResourceUsage {
            cpu_user_micros: user,
            cpu_system_micros: system,
            peak_memory_bytes: peak,
            wall_micros,
            cgroup: None,
            footprint: Some(FootprintUsage {
                peak_footprint_bytes: sampled.then_some(peak),
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

/// Records on `meter` that the leader `leader` was reaped with `reaped`
/// ([`Meter::reaped`]).
pub fn record_reap(meter: &Mutex<Meter>, leader: i32, reaped: crate::leader::Reaped) {
    meter
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .reaped(
            leader,
            Reaped {
                user_micros: reaped.user_micros,
                system_micros: reaped.system_micros,
            },
        );
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
