//! The per-key summary: what is kept, how a run enters it, what it estimates.

use crate::key::{Basis, Day};
use crate::run::{Bounds, Discard, Observed, Outcome, Run, memory_max};
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
/// Days a floor must also have stood before it halves.
pub const DECAY_DAYS: u32 = 7;
/// Two own-limit kill events within this many runs make a key raise-only.
pub const KILL_WINDOW_RUNS: u64 = 256;
/// Days a key stays raise-only after its second kill event.
pub const RAISE_ONLY_DAYS: u32 = 7;
/// CPU is never booked below this in this phase.
pub const CPU_MIN_MILLIS: u32 = 1000;
/// A CPU sample is kept only when the node's CPU pressure was below this (tenths of a
/// percent) when the run started: under contention wall inflates and the ratio lies.
pub const CPU_PRESSURE_MAX_PERMILLE: u16 = 100;
/// A key is mixed when the largest of its last 32 samples is over this many times their
/// median.
pub const MIXED_RATIO: u64 = 4;
/// The latest day a summary accepts (about 2,700 years of days): later days are
/// clamped, and a stored row beyond it is corrupt.
pub const MAX_DAY: u32 = 1_000_000;
/// The most samples a stored row may claim (far beyond any real count).
pub(crate) const MAX_SAMPLES: u64 = 1 << 48;

pub(crate) const MEMORY_STEP: u64 = 256 * MIB;
const CHUNK_RUNS: u64 = 32;
/// Eight full chunks and the one being filled.
const SLOTS: usize = 9;
const RING: usize = 32;
/// Days kept in `day_max`: today and the seven before.
const DAYS: usize = 8;
const SLACK: u64 = 128 * MIB;
const CPU_STEP: u32 = 250;
/// A success this close to its memory limit is treated as having hit it.
const NEAR_LIMIT_PERCENT: u128 = 95;

fn round_up(value: u64, step: u64) -> u64 {
    value.div_ceil(step).saturating_mul(step)
}

/// What an own-limit kill does to a floor: the new floor, and whether the kill opens a
/// new kill event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KillFloor {
    /// The floor after the kill: never below the one before it.
    pub floor: u64,
    /// Whether the kill raised the floor.
    pub raised: bool,
    /// Whether the kill is a new event. A kill of an attempt booked below the floor
    /// already in force was granted before that floor was set, so it belongs to the
    /// event that set it.
    pub new_event: bool,
}

/// The floor after an own-limit kill of an attempt booked at `booked`, when `current`
/// is the floor in force (0: none), the caller's ladder retries at `raised`, and the
/// largest live node that could run the attempt has `host_memory`. The retry books the
/// ladder's next rung; with none given, the first step above the booking. The floor
/// stays strictly below `host_memory` ([`floor_ceiling`]): a booking at a node's whole
/// memory owns the node, and that stays the digest floor's business.
#[must_use]
pub fn kill_floor(current: u64, booked: u64, raised: Option<u64>, host_memory: u64) -> KillFloor {
    let above = round_up(booked.saturating_add(1), MEMORY_STEP);
    let candidate = raised
        .unwrap_or(0)
        .max(above)
        .min(floor_ceiling(host_memory));
    KillFloor {
        floor: current.max(candidate),
        raised: candidate > current,
        new_event: booked >= current,
    }
}

/// The largest key floor under a node of `host_memory` bytes: the biggest 256 MiB step
/// strictly below it (0 for a node of no memory).
#[must_use]
pub fn floor_ceiling(host_memory: u64) -> u64 {
    host_memory.saturating_sub(1) / MEMORY_STEP * MEMORY_STEP
}

/// Everything the estimator keeps for one key (about 700 bytes). Fields are crate
/// private; [`Summary::encode`] and [`Summary::decode`] are the storage form.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Summary {
    pub(crate) basis: Basis,
    /// Memory samples seen (kills and censored successes included).
    pub(crate) n_total: u64,
    /// Maximum of each chunk of 32 uncensored memory samples, slot `chunk % 9`.
    pub(crate) chunk_max: [u64; SLOTS],
    /// Maximum of each chunk's censored samples (`min(peak, booked)` of a run that hit
    /// its limit): they hold an estimate and are never given headroom.
    pub(crate) chunk_held: [u64; SLOTS],
    /// The last 32 samples as contributed, for the dispersion test.
    pub(crate) ring: [u64; RING],
    /// The day each `day_max` slot was last written (slot `day % 8`).
    pub(crate) day_stamp: [u32; DAYS],
    /// Largest sample of that day.
    pub(crate) day_max: [u64; DAYS],
    /// CPU samples seen.
    pub(crate) cpu_n: u64,
    /// The last 32 CPU samples, in millicores.
    pub(crate) cpu_ring: [u32; RING],
    /// The memory a retry after an own-limit kill needs at least (0: none).
    pub(crate) oom_floor: u64,
    /// `n_total` when `oom_floor` was last raised or halved.
    pub(crate) floor_anchor: u64,
    /// The day `oom_floor` was last raised or halved.
    pub(crate) floor_day: Day,
    /// `n_total` at the latest own-limit kill event.
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
            chunk_held: [0; SLOTS],
            ring: [0; RING],
            day_stamp: [0; DAYS],
            day_max: [0; DAYS],
            cpu_n: 0,
            cpu_ring: [0; RING],
            oom_floor: 0,
            floor_anchor: 0,
            floor_day: Day(0),
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

    /// Whether the key may not be lowered on `now` (a clock behind the last sighting
    /// counts as the sighting).
    pub fn raise_only(&self, now: Day) -> bool {
        now.max(self.last_seen) < self.raise_only_until
    }

    /// Whether this summary holds safety state (a floor or a raise-only window) that
    /// must outlive the least-recently-seen eviction.
    pub fn holds_safety_state(&self) -> bool {
        self.oom_floor > 0 || self.raise_only(self.last_seen)
    }

    /// The maximum uncensored memory sample over the last 257 to 288 runs (at least 256).
    pub fn long_max(&self) -> u64 {
        self.chunk_max.iter().copied().fold(0, u64::max)
    }

    /// Whether the last 32 samples are far apart: such a key (a shared or bimodal
    /// shape) is never lowered, and only a kill raises it.
    pub fn mixed(&self) -> bool {
        let kept = self.n_total.min(RING as u64) as usize;
        let mut sorted = self.ring[..kept].to_vec();
        sorted.sort_unstable();
        sorted
            .last()
            .zip(sorted.get(kept / 2))
            .is_some_and(|(max, median)| *max > median.saturating_mul(MIXED_RATIO))
    }

    /// Feeds one finished run in. Safety effects of an own-limit kill (the floor, the
    /// raise-only state) apply whatever the run's wall time. `now` never runs behind the
    /// last sighting.
    pub fn observe(&mut self, run: &Run, now: Day, bounds: &Bounds) -> Observed {
        self.last_seen = self.last_seen.max(now.min(Day(MAX_DAY)));
        let now = self.last_seen;
        let host = bounds.host_memory;
        let trusted = run.wall_us >= MIN_WALL_US;
        let memory = match run.outcome {
            Outcome::Success => match run.peak_memory_bytes {
                Some(peak) if trusted && peak > 0 => {
                    if peak >= host {
                        Err(Discard::OverCap)
                    } else {
                        self.record_success(run, peak, now);
                        self.decay(now);
                        Ok(())
                    }
                }
                _ => Err(Discard::Untrusted),
            },
            Outcome::OwnLimitKill => {
                // The kill's use is rarely known; it died at its limit, which is
                // derived from the booking, not read from the record.
                let limit = memory_max(run.booked_memory_bytes).unwrap_or(0);
                let used = run.peak_memory_bytes.unwrap_or(0);
                let sample = used.max(limit).min(host.saturating_sub(1));
                if sample == 0 {
                    // Nothing booked and nothing measured: not a kill we can act on.
                    Err(Discard::Untrusted)
                } else {
                    self.record_memory(sample, false, now);
                    self.record_kill(run, now, bounds);
                    Ok(())
                }
            }
            Outcome::NodePressureKill | Outcome::Failed | Outcome::Lost | Outcome::Cancelled => {
                Err(Discard::NotASample)
            }
        };
        let cpu = run.outcome == Outcome::Success
            && trusted
            && run.cpu_usage_us > 0
            && run
                .node_cpu_pressure_permille
                .is_some_and(|p| p < CPU_PRESSURE_MAX_PERMILLE);
        if cpu {
            self.record_cpu(run);
        }
        Observed { memory, cpu }
    }

    /// A success that touched its memory limit says only that the booking was not
    /// enough to be killed at: it contributes `min(peak, booked)`, which holds an
    /// estimate but never raises it.
    fn record_success(&mut self, run: &Run, peak: u64, now: Day) {
        let near_limit = memory_max(run.booked_memory_bytes)
            .is_some_and(|limit| u128::from(peak) * 100 >= u128::from(limit) * NEAR_LIMIT_PERCENT);
        if run.limit_hits > 0 || run.swap_peak_bytes > 0 || near_limit {
            self.record_memory(peak.min(run.booked_memory_bytes), true, now);
        } else {
            self.record_memory(peak, false, now);
        }
    }

    fn record_memory(&mut self, sample: u64, held: bool, now: Day) {
        let slot = ((self.n_total / CHUNK_RUNS) % SLOTS as u64) as usize;
        if self.n_total.is_multiple_of(CHUNK_RUNS) {
            self.chunk_max[slot] = 0;
            self.chunk_held[slot] = 0;
        }
        let chunk = if held {
            &mut self.chunk_held[slot]
        } else {
            &mut self.chunk_max[slot]
        };
        *chunk = (*chunk).max(sample);
        self.ring[(self.n_total % RING as u64) as usize] = sample;
        let day = (now.0 % DAYS as u32) as usize;
        if self.day_stamp[day] != now.0 {
            self.day_stamp[day] = now.0;
            self.day_max[day] = 0;
        }
        self.day_max[day] = self.day_max[day].max(sample);
        self.n_total += 1;
    }

    fn record_cpu(&mut self, run: &Run) {
        let millis = u128::from(run.cpu_usage_us) * 1000 / u128::from(run.wall_us);
        let slot = (self.cpu_n % RING as u64) as usize;
        // Capped to u32 first, so the cast cannot truncate.
        self.cpu_ring[slot] = millis.min(u128::from(u32::MAX)) as u32;
        self.cpu_n += 1;
    }

    /// An own-limit kill. A kill of an attempt booked below the floor already in force
    /// was granted before that floor was set: it belongs to the event that set it, so
    /// 50 concurrent operations killed together are one event, not 50.
    fn record_kill(&mut self, run: &Run, now: Day, bounds: &Bounds) {
        let kill = kill_floor(
            self.oom_floor,
            run.booked_memory_bytes,
            run.raised_memory_bytes,
            bounds.host_memory,
        );
        self.oom_floor = kill.floor;
        if kill.raised || kill.new_event {
            self.floor_anchor = self.n_total;
            self.floor_day = now;
        }
        if kill.new_event {
            if self
                .last_kill_n
                .is_some_and(|p| self.n_total - p < KILL_WINDOW_RUNS)
            {
                let until = Day(now.0.saturating_add(RAISE_ONLY_DAYS));
                self.raise_only_until = self.raise_only_until.max(until);
            }
            self.last_kill_n = Some(self.n_total);
        }
    }

    /// The largest sample of today and the seven days before.
    fn recent_day_max(&self, now: Day) -> u64 {
        self.day_stamp
            .iter()
            .zip(self.day_max)
            .filter(|(stamp, _)| {
                now.0
                    .checked_sub(**stamp)
                    .is_some_and(|age| age < DAYS as u32)
            })
            .map(|(_, max)| max)
            .fold(0, u64::max)
    }

    /// Halves the floor once the heavy mode has been absent for 256 runs and 7 days; a
    /// raise-only key therefore keeps its floor.
    fn decay(&mut self, now: Day) {
        let quiet = self.n_total - self.floor_anchor >= DECAY_RUNS
            && now.0.saturating_sub(self.floor_day.0) >= DECAY_DAYS;
        let heaviest = self.long_max().max(self.recent_day_max(now));
        let light = heaviest.saturating_mul(2) < self.oom_floor;
        // No raise-only test is needed: a raise-only window ends 7 days after the
        // kill event that opened it, and that event stamped `floor_day`.
        if self.oom_floor > 0 && quiet && light {
            let half = round_up(self.oom_floor / 2, MEMORY_STEP);
            self.oom_floor = if half < MIN_MEMORY { 0 } else { half };
            self.floor_anchor = self.n_total;
            self.floor_day = now;
        }
    }

    /// Forgets the samples but keeps the safety state (floor, its day, raise-only), as
    /// an idle summary does: the floor then needs 256 new runs and 7 days to decay.
    pub(crate) fn strip_samples(&mut self) {
        let basis = self.basis;
        let kept = Self {
            oom_floor: self.oom_floor,
            floor_day: self.floor_day,
            raise_only_until: self.raise_only_until,
            ..Self::new(basis, self.last_seen)
        };
        *self = kept;
    }

    /// Whether this summary can answer: enough samples, or a floor to honour.
    pub(crate) fn usable(&self) -> bool {
        self.n_total >= MIN_SAMPLES || self.oom_floor > 0
    }

    /// The floor, kept strictly below the host.
    pub(crate) fn floor(&self, bounds: &Bounds) -> u64 {
        self.oom_floor.min(bounds.floor_ceiling())
    }

    /// The memory estimate; `raise_only` is the key's state over the level pair. It can
    /// reach the host: the caller turns that into [`crate::Fallback::OverCap`].
    pub(crate) fn memory(&self, bounds: &Bounds, raise_only: bool) -> u64 {
        let floor = self.floor(bounds);
        let memory = if self.mixed() {
            // Never lowered, raised only by a kill.
            bounds.default_memory.max(floor)
        } else {
            let max = self.long_max();
            let held = self.chunk_held.iter().copied().fold(0, u64::max);
            let sized = max.saturating_add(max.div_ceil(5)).saturating_add(SLACK);
            let memory = round_up(sized.max(held).max(floor), MEMORY_STEP);
            if self.n_total < N_MIN || raise_only {
                memory.max(bounds.default_memory)
            } else {
                memory
            }
        };
        memory.max(MIN_MEMORY)
    }

    /// The CPU estimate in millicores: P90 of the recent parallelism, rounded up to
    /// 250 m, between 1000 m and the node's CPUs.
    pub(crate) fn cpu(&self, bounds: &Bounds) -> u32 {
        let node = bounds.node_cpu_millis;
        let low = CPU_MIN_MILLIS.min(node);
        let kept = self.cpu_n.min(RING as u64) as usize;
        let mut sorted = self.cpu_ring[..kept].to_vec();
        sorted.sort_unstable();
        // Nearest rank: the ceil(0.9 * kept)-th smallest; no samples is 0.
        let rank = (kept * 9).div_ceil(10);
        let p90 = sorted.get(rank.saturating_sub(1)).copied().unwrap_or(0);
        (p90.div_ceil(CPU_STEP).saturating_mul(CPU_STEP)).clamp(low, node)
    }
}
