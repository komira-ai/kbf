//! The golden trace hashes of the scheduler simulations, one file for every family.
//!
//! Each simulation test binary keeps its own lines (named `<family>/...`) in
//! `tests/golden/trace-hashes.txt`: the SHA-256 of the full trace of each pinned
//! scenario and seed. The binary checks them in an ordinary test and rewrites them in an
//! ignored one:
//!
//! ```text
//! cargo test -p kbf-sched -- --ignored regenerate_golden_hashes
//! ```
//!
//! Regenerate only on purpose. A change that only moves code must leave every hash as
//! it is; one that alters a decision, or the trace text, moves them, and the pull
//! request says why.

use kbf_sim::golden::GoldenFile;

/// The lines of `family` in the golden file.
pub fn golden(family: &str) -> GoldenFile {
    GoldenFile::new(
        concat!(env!("CARGO_MANIFEST_DIR"), "/tests/golden/trace-hashes.txt"),
        family,
        "cargo test -p kbf-sched -- --ignored regenerate_golden_hashes",
    )
}
