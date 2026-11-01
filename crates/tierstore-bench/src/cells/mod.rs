//! The cells: each module measures one quantity and returns one [`CellResult`] per arm.
//!
//! Every cell is `pub async fn run(ctx: &CellContext, plan: &CellPlan) -> Result<Vec<CellResult>,
//! BenchError>`, a thin call of `run_with` (the same with the machine, the fault proxy and a
//! limit on requests given, for tests) that builds [`engine::Arm`]s and hands them to
//! [`engine::run_cell`]. The pieces:
//! - [`engine`]: passes, arms, the load gate, the ledger, the samples and the result files;
//! - [`putcell`] (`cold-put`, `a1`), [`cold_read`] (with [`streamget`]), [`g`] (with [`tree`] and
//!   [`fileserver`]), [`b`], [`c`]: the arms of each cell;
//! - [`modes`]: the faults an arm is run under; [`budget`]: the rate cap and the byte ledger;
//!   [`stats`] and [`content`]: percentiles and seeded bytes.
//!
//! Rules every cell keeps:
//! - Every request goes through [`crate::s3::BenchS3`] or [`streamget::StreamGet`], so it is
//!   timed, logged and held to the rate cap.
//! - Pass 0 of each arm is a warm-up: it is run, reported with `discarded: true`, and left out of
//!   every figure. Arms are interleaved across passes and the order is recorded in `notes`.
//! - The cell reads a [`crate::gate::LoadSample`] before each pass, over a quiet window, and
//!   checks it against `ctx.config.gate`; a trip ends the run (`gate_tripped` on the empty pass).
//!   Inside a pass the load is recorded (`pass_load`) and not judged.
//! - Bytes written to the cold store are counted against `ctx.config.bytes`; a cell is refused
//!   before it sends anything when its cost is more than what is left, and a request-rate cap
//!   (`ctx.config.rates`) holds whatever a cell asks for.
//! - p99 is filled in only from the pooled samples of quiet passes, 1,000 requests at least.
//! - Samples (one row per counted request) are appended to `<result_dir>/samples.csv`
//!   ([`crate::result::Sample`]) and the results go to `<result_dir>/<cell>-<arm>-a<N>.json`, `N`
//!   being the run's attempt (`run --attempt`, 1 unless set). A run never writes over a result
//!   file: it stops, before it sends anything, if one is there.
//! - The cold store's state (healing or not, its drives, when it was read) is read again before
//!   every pass and stamped into the results; and the cold store guard ([`coldguard`]) stops the
//!   run, as the load gate does, when the p99 latency or the error share of the harness's own
//!   cold requests over a sliding window is over its limit.
//! - What a cell wrote to a store, it deletes after the pass or at the end, also when the run
//!   stops early.
//!
//! The fault proxy is a process of its own: a cell takes it as a [`engine::FaultSwitch`]
//! ([`run_with_faults`]), which [`crate::faultfile::FileSwitch`] implements over the proxy's
//! control and counters files (`run --fault-control`). Without one only the healthy arms are
//! run.

use std::sync::Arc;

use clap::ValueEnum;
use serde::{Deserialize, Serialize};

use crate::cells::budget::Campaign;
use crate::cells::engine::{FaultSwitch, ProcLoad};
use crate::coldstate::{ColdMeter, ColdStateSource};
use crate::config::RunConfig;
use crate::error::BenchError;
use crate::requestlog::RequestLog;
use crate::result::{CellResult, Provenance};

pub mod a1;
pub mod armfilter;
pub mod b;
pub mod budget;
pub mod c;
pub mod cold_put;
pub mod cold_read;
pub mod coldguard;
pub mod content;
pub mod engine;
#[cfg(test)]
mod engine_gaps_tests;
#[cfg(test)]
mod engine_record_tests;
#[cfg(test)]
mod engine_window_tests;
pub mod fileserver;
pub mod g;
pub mod modes;
pub mod putcell;
pub mod stats;
pub mod streamget;
#[cfg(test)]
pub(crate) mod testkit;
pub mod tree;

/// The cells. `ColdPut`, `ColdRead` and `G` are the first stage (the cold tier on the measured
/// path); `A1`, `B` and `C` need a hot tier and are the second.
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
    /// The campaign the run belongs to: its ledger of bytes written to the cold store.
    pub campaign: Campaign,
    /// Where the cold store's state is read from, before every pass.
    pub cold_state: Arc<dyn ColdStateSource>,
    /// Where the cold store guard reads the store's request counter and state at the ends of
    /// every pass, to judge the requests that came from elsewhere and the drives. `None` for a
    /// run without the cold admin key (and for the tests that do not need it): the guard then
    /// keeps to the bench's own requests.
    pub cold_meter: Option<Arc<dyn ColdMeter>>,
    /// The roles whose requests the cold store's request counter counts (see
    /// [`crate::coldserver`]): their requests are the harness's own count.
    pub cold_server: crate::coldserver::ColdServer,
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
    /// Whether the put cells add their open-loop arms (`run --open-loop`). The other cells have
    /// none and ignore it.
    pub open_loop: bool,
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
    run_with_faults(id, ctx, plan, None).await
}

/// Runs one cell with a fault proxy to drive. `cold-put`, `cold-read`, `a1` and `c` run their
/// fault arms through it (the cold store they reach must be behind the proxy); `b` and `g` have
/// no fault arms and ignore it. Without one only the healthy arms run.
///
/// # Errors
/// What the cell's own `run_with` returns, and what the switch returns.
pub async fn run_with_faults(
    id: CellId,
    ctx: &CellContext,
    plan: &CellPlan,
    faults: Option<Arc<dyn FaultSwitch>>,
) -> Result<Vec<CellResult>, BenchError> {
    let load = || Arc::new(ProcLoad::new());
    match id {
        CellId::ColdPut => cold_put::run_with(ctx, plan, load(), faults, None).await,
        CellId::ColdRead => cold_read::run_with(ctx, plan, load(), faults, None).await,
        CellId::A1 => a1::run_with(ctx, plan, load(), faults, None).await,
        CellId::B => b::run_with(ctx, plan, load(), None).await,
        CellId::C => {
            c::run_with(
                ctx,
                plan,
                load(),
                faults,
                c::polling(ctx.config.rates.list_poll_rps),
                None,
            )
            .await
        }
        CellId::G => g::run_with(ctx, plan, load(), None).await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cells::testkit;

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

    /// Catches a cell id that runs another cell's module: each of the six, with a run that
    /// lacks what it needs, says its own name or its own missing thing, and `g` (which would
    /// build a tree of 344 MB if it got further) stops at the result directory.
    #[tokio::test]
    async fn each_cell_id_reaches_its_own_module() {
        use crate::config::ConfigError;
        use crate::result::Window;
        use crate::role::Role;

        let dir = kbf_testkit::dir::TestDir::under(std::env::temp_dir(), "tb-cells-dispatch");
        let plan = CellPlan {
            passes: 2,
            in_flight: vec![1],
            object_bytes: vec![],
            open_loop: true,
        };
        let bare = testkit::ctx(dir.path(), &[], Window::Busy);
        for (id, name) in [
            (CellId::ColdPut, "cold-put"),
            (CellId::ColdRead, "cold-read"),
            (CellId::A1, "a1"),
        ] {
            let err = run(id, &bare, &plan).await.unwrap_err();
            assert!(matches!(err, BenchError::Setup(_)), "{name}: {err}");
            assert!(err.to_string().starts_with(name), "{name}: {err}");
        }
        let err = run(CellId::B, &bare, &plan).await.unwrap_err();
        assert!(
            matches!(
                err,
                BenchError::Config(ConfigError::Missing {
                    role: Role::Instance,
                    ..
                })
            ),
            "{err}"
        );
        let instance = [(Role::Instance, "http://127.0.0.1:9")];
        let only_instance = testkit::ctx(dir.path(), &instance, Window::Busy);
        let err = run(CellId::C, &only_instance, &plan).await.unwrap_err();
        assert!(
            matches!(
                err,
                BenchError::Config(ConfigError::Missing {
                    role: Role::Cold,
                    ..
                })
            ),
            "{err}"
        );
        let mut blocked = testkit::ctx(dir.path(), &[], Window::Busy);
        let file = dir.path().join("a-file");
        std::fs::write(&file, "x").unwrap();
        blocked.config.result_dir = file.join("out");
        let err = run(CellId::G, &blocked, &plan).await.unwrap_err();
        assert!(matches!(err, BenchError::Io { .. }), "{err}");
    }
}
