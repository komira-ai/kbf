//! The cells: each module measures one quantity and returns one [`CellResult`] per
//! arm.
//!
//! A cell owner fills `run` in its module, keeping its signature:
//!
//! ```text
//! pub async fn run(ctx: &CellContext, plan: &CellPlan) -> Result<Vec<CellResult>, BenchError>
//! ```
//!
//! Rules every cell keeps :
//! - Every request goes through [`crate::s3::BenchS3`], so it is timed and logged.
//! - Pass 0 of each arm is a warm-up: it is run, reported with `discarded: true`, and left out of
//!   every figure. Arms are interleaved across passes and the order is recorded in `notes`.
//! - Before each pass and every few seconds in it the cell reads a [`crate::gate::LoadSample`]
//!   and checks it against `ctx.config.gate`; a trip aborts the pass (`gate_tripped`) and its
//!   samples are `Window::Busy` whatever the run's window.
//! - Bytes written to the cold store are counted against `ctx.config.bytes`; a cell refuses to
//!   start when what is left is less than its own cost, and a request-rate cap
//!   (`ctx.config.rates`) holds whatever a cell asks for.
//! - p99 is filled in only from the pooled samples of quiet passes, 1,000 requests at least.
//! - Samples are appended to `<result_dir>/samples.csv` ([`crate::result::Sample`]) and the
//!   results to `<result_dir>/<cell>-<arm>.json`.

use std::sync::Arc;

use clap::ValueEnum;
use serde::{Deserialize, Serialize};

use crate::config::RunConfig;
use crate::error::BenchError;
use crate::requestlog::RequestLog;
use crate::result::{CellResult, Provenance};

pub mod a1;
pub mod b;
pub mod c;
pub mod g;

/// The cells of the first stage.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum CellId {
    /// Put-to-ack, cold healthy, slow and stopped (through the fault proxy).
    A1,
    /// Read of a hot miss, with copy-back.
    B,
    /// Copy lag, drain time and backlog.
    C,
    /// The 18,105-file tree read, with baselines.
    G,
}

impl CellId {
    /// Every cell, in order.
    pub const ALL: [CellId; 4] = [CellId::A1, CellId::B, CellId::C, CellId::G];

    /// The name a result file and a flag use.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            CellId::A1 => "a1",
            CellId::B => "b",
            CellId::C => "c",
            CellId::G => "g",
        }
    }
}

/// What every cell is given.
pub struct CellContext {
    /// The resolved configuration: stores, gate, caps, window, seed.
    pub config: RunConfig,
    /// Where every request is recorded.
    pub log: Arc<RequestLog>,
    /// Where this run's numbers come from; copied into every result.
    pub provenance: Provenance,
}

/// What the `run` verb asks of a cell; a cell reads the parts it has.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CellPlan {
    /// Passes per arm, the warm-up included.
    pub passes: u32,
    /// Requests in flight, one arm per value.
    pub in_flight: Vec<u32>,
    /// Object sizes in bytes, one arm per value (cell G reads a tree and ignores them).
    pub object_bytes: Vec<u64>,
}

/// Runs one cell.
///
/// # Errors
/// What the cell's own `run` returns.
pub async fn run(
    id: CellId,
    ctx: &CellContext,
    plan: &CellPlan,
) -> Result<Vec<CellResult>, BenchError> {
    match id {
        CellId::A1 => a1::run(ctx, plan).await,
        CellId::B => b::run(ctx, plan).await,
        CellId::C => c::run(ctx, plan).await,
        CellId::G => g::run(ctx, plan).await,
    }
}
