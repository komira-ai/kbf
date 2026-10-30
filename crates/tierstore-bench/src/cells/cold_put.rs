//! Cell cold-put, put-to-ack straight to the cold store.
//!
//! Quantity: the time from sending a put to the cold store until it acknowledges, p50, p95 and
//! (from quiet passes of 1,000 requests at least) p99, with the cold store healthy, slow (added
//! latency per request, a bandwidth cap) and stopped (refused, blackholed), the last two through
//! the fault proxy ([`crate::proxy`]).
//!
//! Arms, each labelled with its path: `direct` (this tool against the test cold bucket: the
//! floor) and `instance` (a multipart upload through the instance, which sends parts straight to
//! the cold tier; the instance has no cold-only mode at the pinned tag, see
//! [`crate::cells::CellId`]).
//! Object sizes (4 KiB, 64 KiB, 1 MiB, 16 MiB, 128 MiB; plain put below the multipart minimum on
//! `direct` only), requests in flight (1, 16, 64), and the proxy in pass mode as a control on
//! every proxied arm. Closed loop, plus an open-loop arm so a stall is not coordinated omission.
//! Writes under a key prefix of its own in the test bucket, counts the bytes against
//! `ctx.config.bytes` and deletes what it wrote. A stopped cold store is a result (the failure
//! class and the time to it), not an error of the run.

use crate::cells::{CellContext, CellPlan};
use crate::error::BenchError;
use crate::result::CellResult;

/// Runs the cell: one [`CellResult`] per arm, passes interleaved, pass 0 discarded. Not built.
///
/// # Errors
/// [`BenchError::NotYet`] today.
pub async fn run(_ctx: &CellContext, _plan: &CellPlan) -> Result<Vec<CellResult>, BenchError> {
    Err(BenchError::NotYet("cell cold-put"))
}
