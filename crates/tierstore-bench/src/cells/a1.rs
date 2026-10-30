//! Cell A1, put-to-ack.
//!
//! Quantity: the time from sending a put to the instance until it acknowledges, p50, p95 and
//! p99, with the cold store healthy, slow (added latency per request, a bandwidth cap) and
//! stopped (refused, blackholed), the last two through the fault proxy ([`crate::proxy`]).
//! Needs a hot tier, so it is the second stage. Arms: object size (4 KiB, 64 KiB, 1 MiB, 16 MiB,
//! 128 MiB), requests in flight (1, 16, 64),
//! the hot store alone as a baseline, and the proxy in pass mode as a control on every proxied
//! arm. Closed loop, plus an open-loop arm so a stall is not coordinated omission. Writes
//! under a key prefix of its own in the instance's test bucket and deletes what it wrote.

use crate::cells::{CellContext, CellPlan};
use crate::error::BenchError;
use crate::result::CellResult;

/// Runs the cell: one [`CellResult`] per arm, passes interleaved, pass 0 discarded. Not built.
///
/// # Errors
/// [`BenchError::NotYet`] today.
pub async fn run(_ctx: &CellContext, _plan: &CellPlan) -> Result<Vec<CellResult>, BenchError> {
    Err(BenchError::NotYet("cell a1"))
}
