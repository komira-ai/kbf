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
//! - [`handoff`]: the file the deploy writes for the evaluator (commit, set hash, generation,
//!   start, reversibility, the commit a rollback goes to).
//! - [`baseline`], [`ledger`]: the 7-day baseline taken at deploy start, from the invocation
//!   records and from the evaluator's own earlier scrapes.
//! - [`rollback`]: the decision, and the call, of the automatic rollback (off by default).
//! - [`cli`]: `kbf-bake run`, `sample`, `baseline` and `tag-message`.
//!
//! **Operator side only.** The evaluator reads the farm's metrics and must not run on a
//! hosted runner; `crates/kbf-it/tests/repo_lints.rs` refuses its name under `.github/`.
//!
//! **A green bake is blocked** on #951 (failed actions by class) and #954 (a boot counter
//! and a membership gauge): the gates that read them find the series absent, and an absent
//! series is red.

pub mod baseline;
pub mod cli;
pub mod drive;
pub mod eval;
pub mod handoff;
pub mod ledger;
pub mod rollback;
pub mod scrape;
pub mod state;
pub mod table;
pub mod updater;

/// What a waiver of the minimum-load condition is called in the verdict.
pub const WAIVE_MIN_LOAD: &str = "min-load";

/// Why a ledger or a baseline file could not be written.
#[derive(Debug, thiserror::Error)]
pub enum WriteError {
    /// The value does not serialise.
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    /// The file system refused.
    #[error("write {path}: {source}")]
    Io {
        /// The path.
        path: String,
        /// The error.
        source: std::io::Error,
    },
}

/// Write `value` to `path` as pretty JSON and a newline, atomically.
pub(crate) fn write_json<T: serde::Serialize>(
    path: &std::path::Path,
    value: &T,
) -> Result<(), WriteError> {
    let mut bytes = serde_json::to_vec_pretty(value)?;
    bytes.push(b'\n');
    kbf_gate::write_atomic(path, &bytes).map_err(|source| WriteError::Io {
        path: path.display().to_string(),
        source,
    })
}
