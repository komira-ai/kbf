//! The per-key summary: what is kept, how a run enters it, what it estimates.

use crate::key::{Basis, Day};
use crate::run::{Bounds, Discard, Observed, Outcome, Run};
use crate::{GIB, MIB};

/// The smallest memory estimate.
pub const MIN_MEMORY: u64 = GIB;
/// Samples before the estimate may go below the default booking.
pub const N_MIN: u64 = 5;
/// Samples before there is an estimate at all (unless a kill left a floor).
pub const MIN_SAMPLES: u64 = 2;
/// A run shorter than this (one second) is not a trusted measurement.
pub const MIN_WALL_US: u64 = 1_000_000;
/// Runs a floor must have gone unchallenged by the heavy mode before it halves; also the
/// span of the long maximum.
pub const DECAY_RUNS: u64 = CHUNK_RUNS * 8;
/// Two own-limit kills within this many runs make a key raise-only.
pub const KILL_WINDOW_RUNS: u64 = 256;
/// Days a key stays raise-only after its second kill.
pub const RAISE_ONLY_DAYS: u32 = 7;
/// CPU is never booked below this in this phase.
pub const CPU_MIN_MILLIS: u32 = 1000;
/// A CPU sample is kept only when the node's CPU pressure was below this (tenths of a
/// percent) when the run started: under contention wall inflates and the ratio lies.
pub const CPU_PRESSURE_MAX_PERMILLE: u16 = 100;

const CHUNK_RUNS: u64 = 32;
/// Eight full chunks and the one being filled.
const SLOTS: usize = 9;
const CPU_RING: usize = 32;
const MEMORY_STEP: u64 = 256 * MIB;
const SLACK: u64 = 128 * MIB;
const CPU_STEP: u32 = 250;

fn round_up(value: u64, step: u64) -> u64 {
    value.div_ceil(step).saturating_mul(step)
}

/// Everything the estimator keeps for one key (about 250 bytes). Fields are crate
/// private; [`Summary::encode`] and [`Summary::decode`] are the storage form.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Summary {
    pub(crate) basis: Basis,
    /// Memory samples seen (kills included).
    pub(crate) n_total: u64,
    /// Maximum of each chunk of 32 memory samples, slot `chunk % 9`.
    pub(crate) chunk_max: [u64; SLOTS],
    /// CPU samples seen.
    pub(crate) cpu_n: u64,
    /// The last 32 CPU samples, in millicores.
    pub(crate) cpu_ring: [u32; CPU_RING],
    /// The memory a retry after an own-limit kill needs at least (0: none).
    pub(crate) oom_floor: u64,
    /// `n_total` when `oom_floor` was last raised or halved.
    pub(crate) floor_anchor: u64,
    /// `n_total` at the latest own-limit kill.
    pub(crate) last_kill_n: Option<u64>,
    /// The key is raise-only on every day before this one.
    pub(crate) raise_only_until: Day,
    pub(crate) last_seen: Day,
}

impl Summary {
    /// An empty summary for `basis`, first seen on `now`.
    pub fn new(basis: Basis, now: Day) -> Self {
        Self {
            basis,
            n_total: 0,
            chunk_max: [0; SLOTS],
            cpu_n: 0,
            cpu_ring: [0; CPU_RING],
            oom_floor: 0,
            floor_anchor: 0,
            last_kill_n: None,
            raise_only_until: Day(0),
            last_seen: now,
        }
    }

    /// The instrument this summary's samples come from.
    pub fn basis(&self) -> Basis {
        self.basis
    }

    /// Memory samples seen.
    pub fn samples(&self) -> u64 {
        self.n_total
    }

    /// The memory floor an own-limit kill left (0: none).
    pub fn oom_floor(&self) -> u64 {
        self.oom_floor
    }

    /// The last day this summary saw a run.
    pub fn last_seen(&self) -> Day {
        self.last_seen
    }

    /// Whether the key may not be lowered on `now`.
    pub fn raise_only(&self, now: Day) -> bool {
        now < self.raise_only_until
    }

    /// The maximum memory sample over the last 257 to 288 runs (at least 256).
    pub fn long_max(&self) -> u64 {
        self.chunk_max.iter().copied().fold(0, u64::max)
    }

    /// Feeds one finished run in. Safety effects of an own-limit kill (the floor, the
    /// raise-only state) apply whatever the run's wall time.
    pub fn observe(&mut self, run: &Run, now: Day, bounds: &Bounds) -> Observed {
        self.last_seen = self.last_seen.max(now);
        let cap = bounds.memory_cap;
        let trusted = run.wall_us >= MIN_WALL_US;
        let memory = match run.outcome {
            Outcome::Success => match run.peak_memory_bytes {
                Some(peak) if trusted => {
                    if peak > cap {
                        Err(Discard::OverCap)
                    } else {
                        self.record_memory(peak);
                        self.decay(now);
                        Ok(())
                    }
                }
                _ => Err(Discard::Untrusted),
            },
            Outcome::OwnLimitKill => {
                let used = run.peak_memory_bytes.unwrap_or(0);
                self.record_memory(used.max(run.limit_bytes).min(cap));
                self.record_kill(run, now, cap);
                Ok(())
            }
            Outcome::NodePressureKill | Outcome::Failed | Outcome::Lost | Outcome::Cancelled => {
                Err(Discard::NotASample)
            }
        };
        let cpu = run.outcome == Outcome::Success
            && trusted
            && run
                .node_cpu_pressure_permille
                .is_some_and(|p| p < CPU_PRESSURE_MAX_PERMILLE);
        if cpu {
            self.record_cpu(run);
        }
        Observed { memory, cpu }
    }

    fn record_memory(&mut self, sample: u64) {
        let index = self.n_total / CHUNK_RUNS;
        let slot = (index % SLOTS as u64) as usize;
        if self.n_total.is_multiple_of(CHUNK_RUNS) {
            self.chunk_max[slot] = 0;
        }
        self.chunk_max[slot] = self.chunk_max[slot].max(sample);
        self.n_total += 1;
    }

    fn record_cpu(&mut self, run: &Run) {
        let millis = u128::from(run.cpu_usage_us) * 1000 / u128::from(run.wall_us);
        let slot = (self.cpu_n % CPU_RING as u64) as usize;
        // Capped to u32 first, so the cast cannot truncate.
        self.cpu_ring[slot] = millis.min(u128::from(u32::MAX)) as u32;
        self.cpu_n += 1;
    }

    fn record_kill(&mut self, run: &Run, now: Day, cap: u64) {
        // The retry books the ladder's next rung; if the caller did not say, the first
        // step above the booking the run died at.
        let above = round_up(run.booked_memory_bytes.saturating_add(1), MEMORY_STEP);
        let candidate = run.raised_memory_bytes.unwrap_or(0).max(above).min(cap);
        self.oom_floor = self.oom_floor.max(candidate);
        self.floor_anchor = self.n_total;
        if self
            .last_kill_n
            .is_some_and(|p| self.n_total - p < KILL_WINDOW_RUNS)
        {
            let until = Day(now.0.saturating_add(RAISE_ONLY_DAYS));
            self.raise_only_until = self.raise_only_until.max(until);
        }
        self.last_kill_n = Some(self.n_total);
    }

    /// Halves the floor once the heavy mode has been absent for 256 runs.
    fn decay(&mut self, now: Day) {
        let quiet = self.n_total - self.floor_anchor >= DECAY_RUNS;
        let light = self.long_max().saturating_mul(2) < self.oom_floor;
        if self.oom_floor > 0 && !self.raise_only(now) && quiet && light {
            let half = round_up(self.oom_floor / 2, MEMORY_STEP);
            self.oom_floor = if half < MIN_MEMORY { 0 } else { half };
            self.floor_anchor = self.n_total;
        }
    }

    /// Whether this summary can answer: enough samples, or a floor to honour.
    pub(crate) fn usable(&self) -> bool {
        self.n_total >= MIN_SAMPLES || self.oom_floor > 0
    }

    /// The floor, never above what a node offers.
    pub(crate) fn floor(&self, bounds: &Bounds) -> u64 {
        self.oom_floor.min(bounds.memory_cap)
    }

    /// The memory estimate; `raise_only` is the key's state at the level pair.
    pub(crate) fn memory(&self, bounds: &Bounds, raise_only: bool) -> u64 {
        let cap = bounds.memory_cap;
        let max = self.long_max();
        let sized = max.saturating_add(max.div_ceil(5)).saturating_add(SLACK);
        let mut memory = round_up(sized.max(self.floor(bounds)), MEMORY_STEP);
        if self.n_total < N_MIN || raise_only {
            memory = memory.max(bounds.default_memory);
        }
        memory.clamp(MIN_MEMORY.min(cap), cap)
    }

    /// The CPU estimate in millicores: P90 of the recent parallelism, rounded up to
    /// 250 m, between 1000 m and the node's CPUs.
    pub(crate) fn cpu(&self, bounds: &Bounds) -> u32 {
        let node = bounds.node_cpu_millis;
        let low = CPU_MIN_MILLIS.min(node);
        let kept = self.cpu_n.min(CPU_RING as u64) as usize;
        let mut sorted = self.cpu_ring[..kept].to_vec();
        sorted.sort_unstable();
        // Nearest rank: the ceil(0.9 * kept)-th smallest; no samples is 0.
        let rank = (kept * 9).div_ceil(10);
        let p90 = sorted.get(rank.saturating_sub(1)).copied().unwrap_or(0);
        (p90.div_ceil(CPU_STEP).saturating_mul(CPU_STEP)).clamp(low, node)
    }
}
