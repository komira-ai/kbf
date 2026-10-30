//! `report`: one table from the result JSON and CSV files of a result directory.
//!
//! To fill: read every `<cell>-<arm>.json` ([`crate::result::CellResult::from_json`]), pool the
//! samples of the non-discarded passes, and print per cell and arm n, failures, p50, p95, and
//! p99 where the pooled quiet sample allows it (with a bootstrap 95% interval and the spread of
//! the per-pass p99), with the window of each pass and the provenance. Percentiles are nearest
//! rank. A p99 from fewer than 1,000 quiet requests is not printed.

use std::path::Path;

use crate::error::BenchError;

/// Renders the table for `result_dir`. Not built.
///
/// # Errors
/// [`BenchError::NotYet`] today.
pub fn render(_result_dir: &Path) -> Result<String, BenchError> {
    Err(BenchError::NotYet("report"))
}
