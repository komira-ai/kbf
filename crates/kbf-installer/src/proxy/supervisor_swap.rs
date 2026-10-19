//! The handover driver: the real [`Supervisor`] of the update API, over the swap.
//!
//! # The seam
//!
//! [`SwapSupervisor::new`] is the one constructor the proxy verb calls to put a real swap
//! behind `POST /v1/update/frontend` in place of a supervisor that refuses. It takes the
//! ports of the swap ([`Parts`]: the children, the admin client, the proxy's gate, the
//! migrator, the recorder, the root half's orders, the clock) and a [`Config`]; it keeps
//! the [`Supervisor`] trait of the API unchanged.
//!
//! Which port is which, on a host:
//! - `child`: [`super::child_real::ProcChild`];
//! - `admin`: [`super::admin::Admin`] (`admin_protocol` 1, the proxy secret on every call);
//! - `gate`: [`super::gate_real::ProxyGate`];
//! - `migrator`: [`ExpandOnlyDefault`], until the real migrator exists (a one-line change
//!   where [`Parts`] is built);
//! - `recorder`: [`super::record::FileRecorder`] over the proxy's state directory;
//! - `orders`: [`super::root_orders::SpoolRootOrders`] over the order spool the API writes
//!   with, so the `confirm` and `revert` the swap sends take the same counter;
//! - `clock`: [`super::runtime::SystemClock`].
//!
//! # What it does
//!
//! `start` builds a [`super::Plan`] from the [`UpdateSpec`] and runs [`Swap::begin`] then
//! [`Swap::run`] on a thread of its own, named `kbf-swap`: the swap blocks on admin calls
//! and sleeps, and the API's actor must not. It writes the first intent record before it
//! returns, so the status shows the update from the moment `start` answers. `start` is
//! idempotent on the update id.
//!
//! The status is assembled from the intent record the swap thread wrote last (kept in
//! memory beside `update.json`, never ahead of it), the proxy's permits on each slot and
//! the old child's own counts; see the `status` module.
//!
//! `abort` queues a [`super::model::Request::Abort`] and answers with the phase at the time
//! of the request; the swap decides at its next poll whether it can honor it.
//! `force_retire` sends SIGTERM to the draining old child through the `child` port; its
//! open streams end when it exits, and the swap's own drain then finishes.
//!
//! # Restart
//!
//! [`SwapSupervisor::new`] reads `update.json`. An update that was in flight is taken up
//! again with [`Swap::adopt`] on a new swap thread (the children outlive the proxy), and
//! resumes by phase; the record is the source of truth for the swap's phase, whatever the
//! API's request map says. An unreadable record makes every `start` refuse
//! (`refused`) and leaves both children as they are. Nothing is held for a finished update.
//!
//! # What does not work, stated
//!
//! - **Rollback after the switch is refused.** The swap stands the old child down at the
//!   switch, and a child that stood down never serves again (`stand-down` is never
//!   reversed; `resume` answers 409). A rollback would stand the new child down and fail to
//!   bring the old one back. [`Config::rollback_after_switch`] stays off until the child can
//!   resume after a stand-down; the swap's own rollback is tested against fake children only.
//! - **Both children dead on a cold start** needs the child to bump its epoch, which
//!   `--await-takeover` does not do: the swap records `restart` and fails.
//! - **A thread that dies takes the proxy with it** ([`Parts::on_death`]); the restart goes
//!   through the adopt path above, with the startup hold in force until it decides.

mod migrator;
mod status;

#[cfg(test)]
mod core_tests;
#[cfg(test)]
mod migrator_tests;
#[cfg(test)]
mod rig;
#[cfg(test)]
mod tests;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use super::api::traits::{Retired, Supervisor, SupervisorError, SupervisorStatus, UpdateSpec};
use super::model::{Mode, Phase, ProcIdent, Slot, Timing};
use super::ports::{Child, ChildAdmin, Clock, Gate, Migrator, Recorder, RootOrders};
use super::runtime::RequestQueue;

pub use migrator::{ExpandOnlyDefault, RunningSchema};

/// The ports of the swap, owned: the swap thread borrows them for as long as it runs.
pub struct Parts {
    pub child: Arc<dyn Child + Send + Sync>,
    pub admin: Arc<dyn ChildAdmin + Send + Sync>,
    pub gate: Arc<dyn Gate + Send + Sync>,
    pub migrator: Arc<dyn Migrator + Send + Sync>,
    pub recorder: Arc<dyn Recorder + Send + Sync>,
    pub orders: Arc<dyn RootOrders + Send + Sync>,
    pub clock: Arc<dyn Clock + Send + Sync>,
    pub requests: Arc<RequestQueue>,
    pub old_proc: Box<dyn Fn(Slot) -> Option<ProcIdent> + Send + Sync>,
    pub on_death: Box<dyn Fn(&str) + Send + Sync>,
}

/// What the supervisor is told besides its ports.
#[derive(Clone, Debug)]
pub struct Config {
    pub mode: Mode,
    pub timing: Timing,
    pub state_dir: PathBuf,
    pub active: Slot,
    pub rollback_after_switch: bool,
}

/// Placeholder that starts nothing: the tests of the real one are red against it.
#[allow(dead_code)]
pub struct SwapSupervisor {
    parts: Arc<Parts>,
    config: Config,
}

impl SwapSupervisor {
    #[must_use]
    pub fn new(parts: Parts, config: Config) -> SwapSupervisor {
        SwapSupervisor {
            parts: Arc::new(parts),
            config,
        }
    }

    pub fn wait_closed(&self, _limit: Duration) -> bool {
        true
    }
}

impl Supervisor for SwapSupervisor {
    fn start(&mut self, _spec: &UpdateSpec) -> Result<(), SupervisorError> {
        Err(SupervisorError::Failed(
            "the handover driver is not wired".into(),
        ))
    }

    fn abort(&mut self) -> Result<Phase, SupervisorError> {
        Err(SupervisorError::NotOpen)
    }

    fn rollback(&mut self) -> Result<Phase, SupervisorError> {
        Err(SupervisorError::NotOpen)
    }

    fn force_retire(&mut self, _reason: &str) -> Result<Retired, SupervisorError> {
        Err(SupervisorError::NotOpen)
    }

    fn status(&self) -> SupervisorStatus {
        SupervisorStatus::default()
    }
}
