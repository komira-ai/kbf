//! Cell cold-read, reads from the cold store.
//!
//! Quantity: the time to first byte and to the last byte of a read of an object that is on the
//! cold store only, and of a ranged 4 KiB read of a 128 MiB object, with the cold store healthy,
//! slow and stopped (through the fault proxy).
//!
//! Arms, each labelled with its path: `direct` (this tool against the test cold bucket) and
//! `instance` (through the instance). Objects are written by the cell itself under a key prefix
//! of its own, through the cold path only (multipart through the instance, so the hot tier does
//! not hold them), and read after the write has settled; the reads are labelled "recently
//! written, likely cache-warm on the store; a lower bound on a real cold read". Requests in
//! flight 1, 16, 64. Counts the cold reads the instance makes with the proxy's `request_count`.
//! Deletes what it wrote.

use crate::cells::{CellContext, CellPlan};
use crate::error::BenchError;
use crate::result::CellResult;

/// Runs the cell: one [`CellResult`] per arm, passes interleaved, pass 0 discarded. Not built.
///
/// # Errors
/// [`BenchError::NotYet`] today.
pub async fn run(_ctx: &CellContext, _plan: &CellPlan) -> Result<Vec<CellResult>, BenchError> {
    Err(BenchError::NotYet("cell cold-read"))
}
