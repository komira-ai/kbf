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
//! * **Samples.** A successful run with a measured peak and a wall time of at least one
//!   second, or an own-limit memory kill (a censored sample: `max(used, limit)`). Failed
//!   runs that are not memory kills, lost or cancelled runs and node-pressure kills are
//!   not samples. CPU samples are successful runs that started below
//!   [`CPU_PRESSURE_MAX_PERMILLE`] node CPU pressure.
//! * **Memory.** `round_up_256MiB(max(1.2 * M + 128 MiB, oom_floor))` where `M` is the
//!   maximum over the last 256 runs (kept as nine chunk maxima of 32 runs: the eight
//!   latest full chunks plus the current one, so the span is 257 to 288 runs; this
//!   contains the last 32), clamped to `[1 GiB, cap]`. The statistic is the maximum, not a
//!   percentile. With fewer than [`N_MIN`] samples the estimate may only raise (it is at
//!   least the default booking).
//! * **CPU.** `round_up_250m(P90(cpu_usage / wall))`, clamped to `[1000 m, node cpus]`;
//!   1000 m with no CPU sample. It never goes below one core in this phase.
//! * **Hints.** The final memory and CPU are the larger of the estimate and the client's
//!   hint: an estimate never gives less than the client asked for.
//! * **Safety.** An own-limit kill sets `oom_floor` to at least the next rung the caller
//!   books on the retry (and above the booking it died at). The floor decays (halves)
//!   only after 256 runs since it was last set, and only while the 256-run maximum is
//!   under half of it; a heavy mode that recurs every few dozen runs never lets it go.
//!   Two kills within 256 runs make the key raise-only for 7 days: no lowering, the
//!   floor and maxima are kept and the estimate stays at least the default booking. A
//!   missing, stale (idle over 60 days) or undecodable summary is [`Fallback`], never a
//!   guess.
//! * **Storage.** [`Summary::encode`] writes a versioned, checksummed, fixed-size
//!   record; [`Summary::decode`] returns `None` for anything else, and a key whose row
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
pub use run::{Bounds, Discard, Hint, Observed, Outcome, Run};
pub use store::{Estimate, Fallback, MAX_KEYS, STALE_DAYS, Summaries};
pub use summary::{
    CPU_MIN_MILLIS, CPU_PRESSURE_MAX_PERMILLE, DECAY_RUNS, KILL_WINDOW_RUNS, MIN_MEMORY,
    MIN_SAMPLES, MIN_WALL_US, N_MIN, RAISE_ONLY_DAYS, Summary,
};

/// One mebibyte.
pub const MIB: u64 = 1 << 20;
/// One gibibyte.
pub const GIB: u64 = 1 << 30;
