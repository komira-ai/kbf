//! The size estimator: predicts the CPU and memory an action will use from what
//! earlier runs of similar actions used.
//!
//! This is a pure crate: no async runtime, network, clock, randomness or hashed
//! collections. The layering test in `kbf-integration-test` and the lists in `clippy.toml` enforce it.
//! Time enters as a [`Day`] argument; the crate has its own small input types
//! ([`Run`], [`Bounds`], [`Hint`]) and depends on no other kbf crate. Nothing calls it
//! yet: the server adapts its run records to these types in a later change.
//!
//! # What it computes
//!
//! A [`Summaries`] map holds one small [`Summary`] per [`Key`]. Every finished run is
//! fed to [`Summaries::observe`]; [`Summaries::estimate`] answers with an [`Estimate`]
//! or a [`Fallback`] (the caller then books what it books today).
//!
//! * **Key.** `(instance, platform class, basis, shape, cfg)`. `basis` (cgroup or Mac
//!   footprint) is part of the key, so the two instruments never feed each other. Level
//!   L1 is the full key; L2 drops `cfg`. A run updates both; the estimate uses the first
//!   level with at least [`N_MIN`] samples, else the first with at least
//!   [`MIN_SAMPLES`], else there is no estimate.
//! * **Samples.** A success with a non-zero measured peak (zero is not a measurement)
//!   and a wall time of at least one second, or an own-limit memory kill. A kill is a
//!   sample of at least the limit derived from its booking (`1.5 x booked + 512 MiB`,
//!   [`memory_max`]), not of a recorded limit field. Failed non-OOM runs, lost or
//!   cancelled runs and node-pressure kills are not samples. A success that hit its
//!   limit (`memory.events` `max` above zero, any swap, or a peak within 5% of the
//!   limit) is censored: it contributes `min(peak, booked)` with no headroom, so it can
//!   hold an estimate at the booking but never raise it; only a kill raises past the
//!   booking. CPU samples are successful runs with CPU time that started below
//!   [`CPU_PRESSURE_MAX_PERMILLE`] node CPU pressure.
//! * **Memory.** `round_up_256MiB(max(1.2 * M + 128 MiB, held, oom_floor))` in integer
//!   arithmetic (`M + ceil(M / 5)`, so results match on every architecture), where `M`
//!   is the maximum uncensored sample over the last 256 or more runs (nine chunk
//!   maxima of 32 runs: the eight latest full chunks plus the current one, a span of
//!   257 to 288 runs) and `held` the largest censored one; at least 1 GiB. The
//!   statistic is the maximum, not a percentile. With fewer than [`N_MIN`] samples, or
//!   while raise-only, the estimate may only raise (it is at least the default
//!   booking). A *mixed* key (the largest of its last 32 samples over four times their
//!   median) is never lowered and raised only by kills: its estimate is the default
//!   booking or its floor.
//! * **Never a whole host.** A booking at or above the memory of the largest matching
//!   live node ([`Bounds::host_memory`]) owns the host. The crate's key floors stay
//!   strictly below it, and an estimate (with the client's hint) that would reach it is
//!   [`Fallback::OverCap`]: the caller books cold and the ladder and digest floors
//!   decide.
//! * **CPU.** `round_up_250m(P90(cpu_usage / wall))`, clamped to `[1000 m, node cpus]`;
//!   1000 m with no CPU sample. It never goes below one core in this phase.
//! * **Hints.** The final memory and CPU are the larger of the estimate and the client's
//!   hint: an estimate never gives less than the client asked for.
//! * **Safety.** An own-limit kill sets `oom_floor` to at least the next rung the caller
//!   books on the retry (and above the booking it died at). Kills are counted as
//!   events: a kill of an attempt booked below the floor already in force was granted
//!   before it and belongs to the same event, so a burst of concurrent kills is one
//!   event. The floor halves only after 256 runs and 7 days since it was last set, and
//!   only while the largest sample of the last 256 runs and of the last 8 days is
//!   under half of it; a heavy mode that recurs never lets it go. Two kill events
//!   within 256 runs make the key raise-only for 7 days (floor and samples kept, never
//!   below the default, no decay, no fallback to "no estimate"). Safety state does not
//!   expire by idleness: a summary idle over 60 days gives [`Fallback::Stale`] for
//!   its samples, but [`Summaries::key_floor`] still returns its floor and
//!   [`Summaries::expire`] keeps the floor and drops only the samples. Keys holding a
//!   floor live in a separate table the least-recently-seen eviction never touches
//!   (per-instance quota on the main table); if that table is full the key stays in
//!   the main table and [`Summaries::unpinned`] counts the refusal. Missing, stale or
//!   undecodable summaries are a [`Fallback`], never a guess. Time is an input and
//!   never runs backward inside a summary.
//! * **Caller duties and known limits.**
//!   1. Apply `max(key_floor, ..)` to the booking on every path, including
//!      [`Fallback::Stale`], [`Fallback::OverCap`] and any other fallback:
//!      [`Summaries::key_floor`] still returns the floor there.
//!   2. The crate lowers an estimate below the default once a key has [`N_MIN`] samples,
//!      unless the key is raise-only or mixed. "Never lower below the default" in a
//!      phase that must not is therefore the caller's guarantee: the adapter clamps the
//!      estimate to at least the default or passes it raise-only.
//!   3. A floor never leaves by idleness (`expire` strips samples only), so the safety
//!      table fills slowly, over months. [`Summaries::unpinned`] counts the refusals and
//!      is the signal. An idle horizon for floors (for example 365 days) is a follow-up,
//!      not done here. The safety table holds at most a quarter of its slots per
//!      instance.
//!   4. [`Summaries::insert_encoded`] replaces the in-memory row unconditionally; do not
//!      load a stored row over a newer in-memory floor.
//! * **Calibration.** The test `the_kill_rate_stays_within_the_declared_bound` draws
//!   peaks from a lognormal around 1 GiB (sigma in rows) and from the same with a 3%
//!   tail four times larger, gives the crate `n` samples, and counts how often the next
//!   run needs more than the limit (1.5 x booking + 512 MiB) of the booking it gets, in
//!   10,000 trials per cell. Declared bound, per mille:
//!
//!   | sigma, tail | n=3 | n=4 | n=5 | n=10 | n=32 |
//!   |---|---|---|---|---|---|
//!   | 0.1 | 1 | 1 | 1 | 1 | 1 |
//!   | 0.3 | 1 | 1 | 2 | 2 | 1 |
//!   | 0.6 | 2 | 2 | 18 | 8 | 3 |
//!   | 1.0 | 35 | 34 | 63 | 38 | 36 |
//!   | 0.1, 3% x4 | 1 | 1 | 30 | 27 | 12 |
//!   | 0.3, 3% x4 | 2 | 2 | 21 | 17 | 9 |
//!   | 0.6, 3% x4 | 10 | 8 | 32 | 18 | 9 |
//!   | 1.0, 3% x4 | 43 | 41 | 68 | 45 | 43 |
//!
//!   The jump at n=5 is the first lowering below the default (a tail run after five
//!   quiet ones); the n=3 and n=4 columns are the default booking.
//! * **Storage.** [`Summary::encode`] writes a versioned, checksummed, fixed-size
//!   record (counters and days beyond sane bounds are corrupt); [`Summary::decode`] returns `None` for anything else, and a key whose row
//!   does not decode starts cold.

#![deny(clippy::disallowed_methods, clippy::disallowed_types)]
#![forbid(unsafe_code)]

mod codec;
mod key;
mod run;
mod store;
mod summary;
#[cfg(test)]
mod tests;

pub use key::{Basis, Cfg, Day, Instance, Key, Level, PlatformClass, Shape};
pub use run::{Bounds, Discard, Hint, Observed, Outcome, Run, memory_max};
pub use store::{
    Estimate, Fallback, MAX_KEYS, MAX_KEYS_PER_INSTANCE, MAX_PINNED, STALE_DAYS, Summaries,
};
pub use summary::{
    CPU_MIN_MILLIS, CPU_PRESSURE_MAX_PERMILLE, DECAY_DAYS, DECAY_RUNS, KILL_WINDOW_RUNS, MAX_DAY,
    MIN_MEMORY, MIN_SAMPLES, MIN_WALL_US, MIXED_RATIO, N_MIN, RAISE_ONLY_DAYS, Summary,
};

/// One mebibyte.
pub const MIB: u64 = 1 << 20;
/// One gibibyte.
pub const GIB: u64 = 1 << 30;
