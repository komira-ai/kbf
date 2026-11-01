//! What the node's `actions/` cgroup is doing now: the CPU it has used, the memory it
//! holds that cannot be reclaimed, whether a CPU limit is set, and which leases have run
//! for a whole window. The same reading lists every running lease with its start and
//! what its own cgroup holds ([`LeaseReading`]).
//!
//! The daemon reports a reading every heartbeat and the scheduler places by it. Every
//! number is an integer and every subtraction saturates. The files are read on the
//! blocking pool ([`crate::off_worker`]), never on an async worker, in one call for the
//! node and all its leases.
//!
//! Two kinds of absence: a file the reading cannot do without (`cpu.stat`'s
//! `usage_usec`, `memory.current`, `memory.stat`'s `inactive_file`) that cannot be read
//! or does not parse is a [`LoadError`]; telemetry (`throttled_usec`, the pressure total)
//! is `None` when its file or field is not there. A lease's files are not needed by the
//! reading: a lease with one unreadable or garbled is still listed, with its start and no
//! usage, never a zero.

use std::collections::{BTreeMap, BTreeSet};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::Instant;

use kbf_daemon::{LeaseReading, LeaseUsage, NodeReading};
use kbf_proto::worker;
use kbf_types::LeaseId;

use crate::cgroup_usage::cpu_stat;
use crate::off_worker::off_worker;

/// A monotonic clock in microseconds. Only differences of its readings mean anything.
pub type MonoClock = Arc<dyn Fn() -> u64 + Send + Sync>;

/// One reading of the `actions/` cgroup.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NodeLoad {
    /// 1 for the first reading of this process, plus one for each after.
    pub seq: u64,
    /// The agent's monotonic clock at the read of `cpu.stat`.
    pub mono_usec: u64,
    /// `usage_usec` of `cpu.stat`: cumulative, and it still holds the CPU of
    /// descendants removed since.
    pub cpu_usage_usec: u64,
    /// `memory.current` minus `inactive_file` of `memory.stat`, floored at 0.
    pub working_set_bytes: u64,
    /// `active_file` of `memory.stat`.
    pub active_file_bytes: u64,
    /// The leases that ran for the whole window since the previous reading: started (the
    /// container, after inputs and image) at or before the previous reading's
    /// `mono_usec`, and running now. Empty for the first reading.
    pub covered: BTreeSet<LeaseId>,
    /// Whether `cpu.max` holds a quota. A cgroup without the file has no `cpu`
    /// controller, so no limit.
    pub actions_cpu_limited: bool,
    /// `throttled_usec` of `cpu.stat`, when it has it.
    pub throttled_usec: Option<u64>,
    /// The cumulative `total=` (microseconds) of the `some` line of `cpu.pressure`, when
    /// the kernel has pressure files.
    pub pressure_some_total_usec: Option<u64>,
    /// Every lease running at the reading, in lease order, with its start and what its
    /// cgroup holds. A lease that ended while the files were read is not listed.
    pub leases: Vec<LeaseReading>,
}

impl NodeLoad {
    /// The reading as the daemon takes it: the node's part as a heartbeat sends it, and
    /// the leases beside it.
    pub(crate) fn into_reading(mut self) -> NodeReading {
        let leases = std::mem::take(&mut self.leases);
        NodeReading {
            load: self.into_proto(),
            leases,
        }
    }

    /// The reading as the daemon sends it in a heartbeat.
    pub(crate) fn into_proto(self) -> worker::NodeLoad {
        worker::NodeLoad {
            seq: self.seq,
            mono_usec: self.mono_usec,
            cpu_usage_usec: self.cpu_usage_usec,
            memory_working_set_bytes: self.working_set_bytes,
            covered: self
                .covered
                .into_iter()
                .map(|id| worker::LeaseId {
                    term: id.term,
                    seq: id.seq,
                })
                .collect(),
            actions_cpu_limited: self.actions_cpu_limited,
            cpu_throttled_usec: self.throttled_usec,
            cpu_pressure_some_total_usec: self.pressure_some_total_usec,
            memory_active_file_bytes: self.active_file_bytes,
            lease_clocks: Vec::new(),
            minutes: Vec::new(),
        }
    }
}

/// Why a reading could not be made.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum LoadError {
    /// A file could not be read.
    #[error("{file}: {why}")]
    Read { file: &'static str, why: String },
    /// A file is there but does not say what the reading needs.
    #[error("{file}: {what}")]
    Malformed {
        file: &'static str,
        what: &'static str,
    },
}

fn malformed(file: &'static str, what: &'static str) -> LoadError {
    LoadError::Malformed { file, what }
}

/// `usage_usec` and `throttled_usec` of a `cpu.stat`.
pub(crate) fn parse_cpu_stat(text: &str) -> Result<(u64, Option<u64>), LoadError> {
    let stat = cpu_stat(text);
    let usage = stat
        .usage
        .ok_or_else(|| malformed("cpu.stat", "no usable usage_usec"))?;
    Ok((usage, stat.throttled_usec))
}

/// A `memory.current`: one number.
pub(crate) fn parse_memory_current(text: &str) -> Result<u64, LoadError> {
    text.trim()
        .parse()
        .map_err(|_| malformed("memory.current", "not a number"))
}

/// `inactive_file` of a `memory.stat`.
pub(crate) fn parse_inactive_file(text: &str) -> Result<u64, LoadError> {
    memory_stat_key(text, "inactive_file")
        .ok_or_else(|| malformed("memory.stat", "no usable inactive_file"))
}

/// `active_file` of a `memory.stat`. Strict like [`parse_inactive_file`]: every cgroup v2
/// `memory.stat` has both lines, so a file without one is not the file the reading needs.
pub(crate) fn parse_active_file(text: &str) -> Result<u64, LoadError> {
    memory_stat_key(text, "active_file")
        .ok_or_else(|| malformed("memory.stat", "no usable active_file"))
}

fn memory_stat_key(text: &str, key: &str) -> Option<u64> {
    text.lines().find_map(|line| {
        let mut words = line.split_whitespace();
        (words.next()? == key).then(|| words.next()?.parse().ok())?
    })
}

/// Whether a `cpu.max` (`max 100000` or `<quota> <period>`) holds a quota.
pub(crate) fn parse_cpu_limited(text: &str) -> Result<bool, LoadError> {
    match text.split_whitespace().next() {
        Some("max") => Ok(false),
        Some(quota) if quota.parse::<u64>().is_ok() => Ok(true),
        _ => Err(malformed("cpu.max", "neither max nor a quota")),
    }
}

/// The `total=` of the `some` line of a `cpu.pressure`.
pub(crate) fn parse_pressure_some_total(text: &str) -> Option<u64> {
    let line = text
        .lines()
        .find(|line| line.split_whitespace().next() == Some("some"))?;
    line.split_whitespace()
        .find_map(|field| field.strip_prefix("total="))?
        .parse()
        .ok()
}

/// A window's CPU in millicores: the usage between two `(usage_usec, mono_usec)`
/// samples over the time between them. `None` when time did not move forward or the
/// counter went back.
pub(crate) fn window_millicores(before: (u64, u64), now: (u64, u64)) -> Option<u64> {
    let used = now.0.checked_sub(before.0)?;
    let span = now.1.checked_sub(before.1).filter(|&s| s > 0)?;
    let millis = u128::from(used) * 1000 / u128::from(span);
    Some(u64::try_from(millis).unwrap_or(u64::MAX))
}

fn read(dir: &Path, file: &'static str) -> Result<String, LoadError> {
    std::fs::read_to_string(dir.join(file)).map_err(|e| LoadError::Read {
        file,
        why: e.to_string(),
    })
}

/// Like [`read`], but a file that is not there is `None`.
fn read_optional(dir: &Path, file: &'static str) -> Result<Option<String>, LoadError> {
    match std::fs::read_to_string(dir.join(file)) {
        Ok(text) => Ok(Some(text)),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
        Err(e) => Err(LoadError::Read {
            file,
            why: e.to_string(),
        }),
    }
}

/// Reads the cgroup at `dir`. Blocking. `seq` and `covered` are the caller's to fill.
pub(crate) fn read_load(dir: &Path, clock: &dyn Fn() -> u64) -> Result<NodeLoad, LoadError> {
    let (cpu_usage_usec, throttled_usec) = parse_cpu_stat(&read(dir, "cpu.stat")?)?;
    let mono_usec = clock();
    let current = parse_memory_current(&read(dir, "memory.current")?)?;
    let stat = read(dir, "memory.stat")?;
    let inactive = parse_inactive_file(&stat)?;
    let active_file_bytes = parse_active_file(&stat)?;
    let actions_cpu_limited = match read_optional(dir, "cpu.max")? {
        Some(text) => parse_cpu_limited(&text)?,
        None => false,
    };
    // An unreadable pressure file is telemetry, absent.
    let pressure_some_total_usec = std::fs::read_to_string(dir.join("cpu.pressure"))
        .ok()
        .and_then(|text| parse_pressure_some_total(&text));
    Ok(NodeLoad {
        seq: 0,
        mono_usec,
        cpu_usage_usec,
        working_set_bytes: current.saturating_sub(inactive),
        active_file_bytes,
        covered: BTreeSet::new(),
        actions_cpu_limited,
        throttled_usec,
        pressure_some_total_usec,
        leases: Vec::new(),
    })
}

/// What one lease's cgroup said at a reading. A part is `None` when a file it needs could
/// not be read or does not parse.
struct LeaseRead {
    /// `usage_usec` of `cpu.stat`, with the clock read right after it.
    cpu: Option<(u64, u64)>,
    /// `memory.current` less `inactive_file` of `memory.stat`, floored at 0.
    working_set: Option<u64>,
}

impl LeaseRead {
    /// The cumulative CPU and the working set, when both were read.
    fn usage(&self) -> Option<LeaseUsage> {
        let ((cpu_usage_usec, _), working_set_bytes) = self.cpu.zip(self.working_set)?;
        Some(LeaseUsage {
            cpu_usage_usec,
            working_set_bytes,
        })
    }
}

/// Reads the cgroup of one lease. Blocking. The clock is read right after `cpu.stat` and
/// before the memory files, so the window it closes is the CPU's alone.
fn read_lease(dir: &Path, clock: &dyn Fn() -> u64) -> LeaseRead {
    let cpu = std::fs::read_to_string(dir.join("cpu.stat"))
        .ok()
        .and_then(|text| cpu_stat(&text).usage)
        .map(|usage| (usage, clock()));
    LeaseRead {
        cpu,
        working_set: lease_working_set(dir),
    }
}

fn lease_working_set(dir: &Path) -> Option<u64> {
    let current = std::fs::read_to_string(dir.join("memory.current")).ok()?;
    let stat = std::fs::read_to_string(dir.join("memory.stat")).ok()?;
    let current = parse_memory_current(&current).ok()?;
    let inactive = parse_inactive_file(&stat).ok()?;
    Some(current.saturating_sub(inactive))
}

/// The process's monotonic clock: microseconds since the first call.
pub(crate) fn process_clock() -> MonoClock {
    static BASE: OnceLock<Instant> = OnceLock::new();
    let base = *BASE.get_or_init(Instant::now);
    Arc::new(move || u64::try_from(base.elapsed().as_micros()).unwrap_or(u64::MAX))
}

/// One running lease.
#[derive(Debug)]
struct Running {
    /// Its cgroup.
    dir: PathBuf,
    /// The clock when its container started.
    started: u64,
    /// The last `(usage_usec, mono_usec)` read of its cgroup.
    last: Option<(u64, u64)>,
    /// The most CPU any one window used, in millicores.
    peak: Option<u64>,
}

/// The state between readings.
#[derive(Debug, Default)]
struct Gate {
    previous_mono: Option<u64>,
    seq: u64,
}

/// What the readings are made from: when each running lease started, its windows, and
/// the previous reading.
pub(crate) struct LoadTracker {
    clock: MonoClock,
    running: Mutex<BTreeMap<LeaseId, Running>>,
    /// Peaks of leases that ended, until taken.
    finished: Mutex<BTreeMap<LeaseId, Option<u64>>>,
    /// Held across a reading, so readings are in order, and held by the blocking read for
    /// as long as it runs: a reading given up on (its future dropped) does not let a second
    /// read start on the blocking pool behind a file that does not answer.
    gate: Arc<tokio::sync::Mutex<Gate>>,
}

impl std::fmt::Debug for LoadTracker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LoadTracker").finish_non_exhaustive()
    }
}

impl LoadTracker {
    pub(crate) fn new(clock: MonoClock) -> Self {
        Self {
            clock,
            running: Mutex::new(BTreeMap::new()),
            finished: Mutex::new(BTreeMap::new()),
            gate: Arc::new(tokio::sync::Mutex::new(Gate::default())),
        }
    }

    fn running(&self) -> std::sync::MutexGuard<'_, BTreeMap<LeaseId, Running>> {
        // Each update is one map operation.
        self.running.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn finished(&self) -> std::sync::MutexGuard<'_, BTreeMap<LeaseId, Option<u64>>> {
        self.finished.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The lease's container started now (a start made again replaces the first).
    pub(crate) fn started(&self, id: LeaseId, cgroup: PathBuf) {
        let started = (self.clock)();
        self.running().insert(
            id,
            Running {
                dir: cgroup,
                started,
                last: None,
                peak: None,
            },
        );
    }

    /// The lease's run ended: its peak is kept for [`Self::take_peak`].
    pub(crate) fn ended(&self, id: LeaseId) {
        if let Some(lease) = self.running().remove(&id) {
            self.finished().insert(id, lease.peak);
        }
    }

    /// The most CPU any one window of the lease used, in millicores, once, after it
    /// ended. `None` for a lease that ended before a second reading of it, and for one
    /// that never started.
    pub(crate) fn take_peak(&self, id: LeaseId) -> Option<u64> {
        self.finished().remove(&id)?
    }

    /// Reads the cgroup at `actions` and every running lease's cgroup (`cpu.stat`, then the
    /// clock, then `memory.current` and `memory.stat`) in one blocking call.
    ///
    /// The gate moves into that call and comes back with its result, so it stays held
    /// while the files are read even when this future is dropped: a later reading waits
    /// for the read still running instead of starting another one beside it.
    pub(crate) async fn reading(&self, actions: PathBuf) -> Result<NodeLoad, LoadError> {
        let gate = Arc::clone(&self.gate).lock_owned().await;
        let leases: Vec<(LeaseId, PathBuf)> = self
            .running()
            .iter()
            .map(|(id, lease)| (*id, lease.dir.clone()))
            .collect();
        let clock = Arc::clone(&self.clock);
        let (load, reads, mut gate) = off_worker(move || {
            let load = read_load(&actions, &*clock);
            let reads: Vec<(LeaseId, LeaseRead)> = leases
                .iter()
                .map(|(id, dir)| (*id, read_lease(dir, &*clock)))
                .collect();
            (load, reads, gate)
        })
        .await;
        let mut running = self.running();
        let mut listed = Vec::with_capacity(reads.len());
        for (id, read) in reads {
            // A lease that ended while the files were read is not listed, and its window
            // is not taken.
            let Some(lease) = running.get_mut(&id) else {
                continue;
            };
            // A lease whose `cpu.stat` is not there has no window; it keeps its start.
            if let Some(sample) = read.cpu {
                if let Some(window) = lease.last.and_then(|last| window_millicores(last, sample)) {
                    lease.peak = Some(lease.peak.map_or(window, |p| p.max(window)));
                }
                lease.last = Some(sample);
            }
            listed.push(LeaseReading {
                lease: id,
                started_usec: lease.started,
                usage: read.usage(),
            });
        }
        let mut load = load?;
        load.leases = listed;
        if let Some(previous) = gate.previous_mono {
            load.covered = running
                .iter()
                .filter(|(_, lease)| lease.started <= previous)
                .map(|(id, _)| *id)
                .collect();
        }
        drop(running);
        gate.seq = gate.seq.saturating_add(1);
        load.seq = gate.seq;
        gate.previous_mono = Some(load.mono_usec);
        Ok(load)
    }
}

#[cfg(test)]
#[path = "node_load_tests.rs"]
mod tests;
