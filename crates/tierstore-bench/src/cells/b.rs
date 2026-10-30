//! Cell B, cold read with copy-back.
//!
//! Quantity: the time of a read of an object that is on the cold store only, the first read
//! (which copies it back to the hot store), the second read, and a read of the cold store
//! directly. Cold-only objects are made by multipart completion. Time to first byte of a 128 MiB
//! miss; for a ranged 4 KiB read of 128 MiB, bytes copied per byte read. The cold reads are
//! labelled "recently written, likely cache-warm; a lower bound on a real miss".

use crate::cells::{CellContext, CellPlan};
use crate::error::BenchError;
use crate::result::CellResult;

/// Runs the cell: one [`CellResult`] per arm, passes interleaved, pass 0 discarded. Not built.
///
/// # Errors
/// [`BenchError::NotYet`] today.
pub async fn run(_ctx: &CellContext, _plan: &CellPlan) -> Result<Vec<CellResult>, BenchError> {
    Err(BenchError::NotYet("cell b"))
}
