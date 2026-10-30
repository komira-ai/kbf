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
pub mod budget;
pub mod c;
pub mod cold_put;
pub mod cold_read;
pub mod content;
pub mod engine;
pub mod g;
pub mod modes;
pub mod putcell;
pub mod stats;
pub mod streamget;
#[cfg(test)]
pub(crate) mod testkit;

/// The cells. `ColdPut`, `ColdRead` and `G` are the first stage (the instance with the cold
/// tier on the measured path); `A1`, `B` and `C` need a hot tier and are the second.
///
/// Whether the instance can run with a cold tier only: at the pinned tag (v0.0.37) it cannot.
/// `--hot` is a required flag (`memory`, `dir:<path>` or `s3:<bucket>`), and there is no
/// cold-only mode. So the cold cells measure two paths and say which in the arm label:
/// - `direct`: this tool against the test cold bucket, with no instance, the floor;
/// - `instance`: the instance in front of the same bucket, with a `dir:` hot tier of its own on
///   the same host. A multipart upload goes to the cold tier part by part and does not touch
///   the hot tier, so `cold-put` through the instance measures the cold path only when it
///   uploads in parts; a plain put is acknowledged from the hot tier and belongs to `a1`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum CellId {
    /// Put-to-ack straight to the cold store, healthy, slow and stopped (through the fault
    /// proxy).
    ColdPut,
    /// Reads of objects that are on the cold store only, healthy, slow and stopped.
    ColdRead,
    /// Put-to-ack with a hot tier, cold healthy, slow and stopped.
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
    pub const ALL: [CellId; 6] = [
        CellId::ColdPut,
        CellId::ColdRead,
        CellId::G,
        CellId::A1,
        CellId::B,
        CellId::C,
    ];

    /// The name a result file and a flag use.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            CellId::ColdPut => "cold-put",
            CellId::ColdRead => "cold-read",
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
        CellId::ColdPut => cold_put::run(ctx, plan).await,
        CellId::ColdRead => cold_read::run(ctx, plan).await,
        CellId::A1 => a1::run(ctx, plan).await,
        CellId::B => b::run(ctx, plan).await,
        CellId::C => c::run(ctx, plan).await,
        CellId::G => g::run(ctx, plan).await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Catches a cell whose result-file name, flag name and JSON name differ, or two cells with
    /// one name.
    #[test]
    fn a_cell_has_one_name_in_flag_json_and_file() {
        let mut seen = std::collections::BTreeSet::new();
        for id in CellId::ALL {
            let name = id.as_str();
            assert!(seen.insert(name), "{name} twice");
            assert_eq!(serde_json::to_string(&id).unwrap(), format!("\"{name}\""));
            let flag = id.to_possible_value().unwrap();
            assert_eq!(flag.get_name(), name);
        }
        assert_eq!(seen.len(), 6);
    }
}
