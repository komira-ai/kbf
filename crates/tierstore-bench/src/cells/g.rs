//! Cell G, the tree read.
//!
//! Quantity: files per second and p50/p99 of a read of the 18,105-file tree through the
//! instance, at 1, 16, 32, 64 and 128 in flight, with baselines (the hot store directly, a
//! loopback static server, the local file system). Content is synthetic, generated from
//! `ctx.config.seed` with the real file sizes; the manifest holds digests only. Which process
//! saturates is recorded (CPU per process).

use crate::cells::{CellContext, CellPlan};
use crate::error::BenchError;
use crate::result::CellResult;

/// Runs the cell: one [`CellResult`] per arm, passes interleaved, pass 0 discarded. Not built.
///
/// # Errors
/// [`BenchError::NotYet`] today.
pub async fn run(_ctx: &CellContext, _plan: &CellPlan) -> Result<Vec<CellResult>, BenchError> {
    Err(BenchError::NotYet("cell g"))
}
