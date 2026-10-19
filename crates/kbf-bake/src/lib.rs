//! The gamma bake evaluator (`docs/design/kbf-cicd.md`, section 7).
//!
//! - [`table`]: every threshold, and the text of the page's table each must match
//!   (`tests/doc_table.rs`).
//! - [`eval`]: one scrape in, the decision out, with no clock and no I/O.
//! - [`state`]: the state file that keeps the bake's clock across a restart.
//! - [`drive`]: the scrape loop, which writes the verdict (`kbf_gate::ResultDoc`) first
//!   `RUNNING` and last green, red or extended.
//! - [`updater`]: the phase log (node steps, restarts) the under-replicated and restarts
//!   gates read.
//! - [`cli`]: `kbf-bake run` and `kbf-bake tag-message`.
//!
//! **Operator side only.** The evaluator reads the farm's metrics and must not run on a
//! hosted runner; `crates/kbf-it/tests/repo_lints.rs` refuses its name under `.github/`.
//!
//! **A green bake is blocked** on #951 (failed actions by class) and #954 (a boot counter
//! and a membership gauge): the gates that read them find the series absent, and an absent
//! series is red.

pub mod cli;
pub mod drive;
pub mod eval;
pub mod scrape;
pub mod state;
pub mod table;
pub mod updater;

/// What a waiver of the minimum-load condition is called in the verdict.
pub const WAIVE_MIN_LOAD: &str = "min-load";
