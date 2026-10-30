//! Cell C, copy lag.
//!
//! Quantity: from the acknowledgement of a put until the object is on the cold store, as a
//! distribution, the time to drain a burst, and the backlog; healthy, and stopped then restored.
//! The poll is one prefix LIST of the cold test bucket per second at most
//! (`ctx.config.rates.list_poll_rps`), plus the instance's journal and usage state; never a
//! per-key HEAD.

use crate::cells::{CellContext, CellPlan};
use crate::error::BenchError;
use crate::result::CellResult;

/// Runs the cell: one [`CellResult`] per arm, passes interleaved, pass 0 discarded. Not built.
///
/// # Errors
/// [`BenchError::NotYet`] today.
pub async fn run(_ctx: &CellContext, _plan: &CellPlan) -> Result<Vec<CellResult>, BenchError> {
    Err(BenchError::NotYet("cell c"))
}
