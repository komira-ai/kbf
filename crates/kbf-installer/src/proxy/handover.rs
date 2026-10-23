//! What the update API is made of when the proxy verb hosts it: every seam of
//! [`Parts`](super::api::core::Parts) bound to its real implementation in one function,
//! [`parts`].
//!
//! | seam | here |
//! |---|---|
//! | `status` | [`JsonFileStatus`] over the root half's `status.json` (with [`HandoverConfig::local`], the proxy's own `releases.json`), read when a call needs it (a file that is not there yet is an unreadable status, not a failure to start) |
//! | `orders` | the [`OrderSpool`] of `orders/`, opened here, once; with [`HandoverConfig::local`] none is opened and the writer refuses every order |
//! | `requests` | [`Idempotency`] in the proxy's state directory (`requests.json`) |
//! | `restart` | the [`RunningBinary`] captured at start |
//! | `clock`, `hook` | the system clock; no crash points |
//! | `config` | the confirm window of `installer.toml`, and a staging deadline sized by its `fetch_timeout_s` ([`staging_deadline`]) |
//! | `compat` | [`StubGate`]: everything is compatible until the gate (R1, R4) is built |
//! | `supervisor` | [`Handover`](super::handover_swap::Handover): the swap, built over the proxy's gate once the verb has bound the proxy and filled [`HandoverConfig::gate`] |
//!
//! **One spool per `orders/` directory.** [`OrderSpool::open`] removes the half-written files
//! of an earlier run, so a second spool over the same directory, in this process or another,
//! would delete the file the first is writing. [`parts`] opens the only one. The swap writes
//! its `confirm` and `revert` orders through the same counter, so the supervisor takes a clone
//! of that spool (a clone shares the counter and the lock) for its `SpoolRootOrders`, and never
//! opens `orders/` itself.
//!
//! **Before the proxy is bound.** [`parts`] runs before `Proxy::bind` (so that a spool error is
//! a usage error before anything is bound), and the swap needs the proxy's gate, which only
//! exists after it. The supervisor [`parts`] returns therefore waits for [`HandoverConfig::gate`],
//! a slot the verb fills right after the bind: until then its status is idle and a `POST` is
//! refused ("proxy not bound yet", [`start_blocked`](super::api::traits::Supervisor::start_blocked)), nothing written. It is built
//! on its first call after the fill, which is `Core::new` asking for the status, so an update a
//! predecessor left in flight is resumed before the API serves. See
//! [`super::handover_swap`].
//!
//! **Fail fast, on purpose.** An order spool that cannot be opened ends the start
//! ([`HandoverError::Orders`]), and with it the data plane: the verb exits 2, which the unit
//! does not restart. A host whose `orders/` the service user cannot use is a host where
//! the update API could never work, and a proxy that serves while its update route is dead
//! hides that until the first update. The alternative is to serve and answer 503 on the update
//! route only; take it by returning a `Parts` over a spool that refuses, not by skipping the
//! error here.
//!
//! **Without a spool.** A proxy that keeps its own copies of `kbf` (`--kbf-binary`,
//! [`HandoverConfig::local`]) owns no `orders/` and opens none, so there is nothing to fail:
//! the swap's orders are the [`Local::orders`] over the proxy's `releases.json`, which
//! `status_file` names, and the API's writer is one that refuses every order with the reason.
//! The swap is then also started by the binary watch ([`super::binary_watch`]): [`parts_shared`]
//! hands it a clone of the supervisor (the same swap, see [`Handover`]) beside the copies.

use std::io;
use std::path::PathBuf;
use std::sync::Arc;

use kbf_update::Source;

use super::api::core::{Config, NoCrash, Parts, StagingDeadline};
use super::api::idempotency::Idempotency;
use super::api::traits::{
    JsonFileStatus, OrderError, OrderWriter, StubGate, Supervisor, SystemClock,
};
use super::binary_watch::Wiring;
use super::handover_swap::{GateSlot, Handover, SlotAddrs};
use super::model::{Locator, Verb};
use super::orders::{OrderSpool, SpoolError};
use super::releases::Local;
use super::running_binary::RunningBinary;
use crate::CommitId;

/// What [`parts`] is built from.
#[derive(Clone, Debug)]
pub struct HandoverConfig {
    /// The proxy's state directory: `requests.json` (the API's request map) and `orders.json`
    /// (the spool's counter) are written here.
    pub state_dir: PathBuf,
    /// The order spool, `orders/`: owned by the service user, read by the root half. Not opened
    /// when [`local`](Self::local) is set.
    pub orders_dir: PathBuf,
    /// The root half's `status.json`; with [`local`](Self::local), the proxy's own
    /// `releases.json`, which has the same shape.
    pub status_file: PathBuf,
    /// The proxy's own copies of `kbf` and its orders over them (`--kbf-binary`). When set, the
    /// swap's orders are [`Local::orders`] (the staged and installed releases are read from, and
    /// confirm and revert are written to, `status_file`), no order spool is opened, and the
    /// update API's order writer refuses. When not, the root half's spool and `status.json` are
    /// used as ever.
    pub local: Option<Local>,
    /// The confirm window the orders ask for (`confirm_window_s` of `installer.toml`).
    pub confirm_window_s: u32,
    /// How long the root half's fetch may take (`fetch_timeout_s` of `installer.toml`).
    pub fetch_timeout_s: u32,
    /// The loopback ports of the two children's admin and operator listeners, by slot.
    pub slots: SlotAddrs,
    /// `installer.toml`: the `database` the swap's migrator brings forward before a candidate
    /// starts.
    pub installer_toml: PathBuf,
    /// Set once by the verb right after the bind; the supervisor waits for it.
    pub gate: GateSlot,
}

/// Why [`parts`] could not build the API's seams.
#[derive(Debug, thiserror::Error)]
pub enum HandoverError {
    /// `orders/` or the counter cannot be opened: a directory the service user cannot write,
    /// or a counter another build wrote. The operator's to fix; it ends the start (see the
    /// module doc, "Fail fast").
    #[error("the order spool at {}: {source}", dir.display())]
    Orders { dir: PathBuf, source: SpoolError },
    /// The running program cannot be identified, so `restart_pending` has no answer.
    #[error("cannot tell which program is running: {0}")]
    Binary(io::Error),
}

/// What the root half needs beyond a fetch (verify, install, probe the release, find the
/// order on its next pass), added to `fetch_timeout_s` to size the staging deadline.
const STAGING_MARGIN_S: u64 = 300;

/// How long an accepted update waits for its release to be staged: the fetch's own limit plus
/// [`STAGING_MARGIN_S`], at most [`StagingDeadline::MAX_S`] (the deadline counts from the order
/// being written and a fetch under way does not move it, so a deadline at the fetch limit would
/// cut and revert a fetch that was about to succeed).
#[must_use]
pub fn staging_deadline(fetch_timeout_s: u32) -> StagingDeadline {
    let secs = (u64::from(fetch_timeout_s) + STAGING_MARGIN_S).min(StagingDeadline::MAX_S);
    // `from_secs` refuses only 0 and more than MAX_S, and `secs` is neither.
    StagingDeadline::from_secs(secs).unwrap_or(StagingDeadline::DEFAULT)
}

/// The seams of the update API, real where there is an implementation. Opens the order spool
/// (unless the proxy keeps its own copies, [`HandoverConfig::local`]: there is none then) and
/// the request map; reads no file (`status.json` is read when a call needs it).
///
/// # Errors
/// [`HandoverError`]: the order spool cannot be opened, or the running program cannot be
/// identified.
pub fn parts(cfg: &HandoverConfig) -> Result<Parts, HandoverError> {
    parts_shared(cfg).map(|(parts, _)| parts)
}

/// [`parts`], and for a proxy that keeps its own copies ([`HandoverConfig::local`]) the
/// [`Wiring`] of the binary watch: its copies and a clone of the supervisor the API holds, so
/// that an update the watch starts is the one the API reports. `None` without copies.
///
/// # Errors
/// As [`parts`].
pub fn parts_shared(cfg: &HandoverConfig) -> Result<(Parts, Option<Wiring>), HandoverError> {
    match &cfg.local {
        Some(local) => {
            let binary = RunningBinary::capture().map_err(HandoverError::Binary)?;
            let swap = Handover::local(cfg, Arc::clone(&local.orders));
            let wiring = Wiring {
                local: local.clone(),
                swap: swap.clone(),
            };
            let parts = wired(cfg, Box::new(swap), Box::new(NoOrders), binary);
            Ok((parts, Some(wiring)))
        }
        None => {
            let spool = OrderSpool::open(&cfg.orders_dir, &cfg.state_dir).map_err(|source| {
                HandoverError::Orders {
                    dir: cfg.orders_dir.clone(),
                    source,
                }
            })?;
            let binary = RunningBinary::capture().map_err(HandoverError::Binary)?;
            Ok((assemble(cfg, spool, binary), None))
        }
    }
}

/// The update API's order writer when the proxy keeps its own copies: there is no order spool
/// to write to, so every order is refused, with the reason.
struct NoOrders;

/// Why [`NoOrders`] refuses.
const NO_ORDERS: &str =
    "this proxy installs from its own copy of kbf (--kbf-binary): there is no order spool";

impl OrderWriter for NoOrders {
    fn write(
        &mut self,
        _verb: Verb,
        _commit: &CommitId,
        _window_s: u32,
    ) -> Result<Locator, OrderError> {
        Err(OrderError(NO_ORDERS.to_owned()))
    }

    fn write_update(
        &mut self,
        _commit: &CommitId,
        _window_s: u32,
        _source: Source,
    ) -> Result<Locator, OrderError> {
        Err(OrderError(NO_ORDERS.to_owned()))
    }
}

/// [`parts`] over a spool and a binary identity it is given.
fn assemble(cfg: &HandoverConfig, spool: OrderSpool, binary: RunningBinary) -> Parts {
    let supervisor = Handover::new(cfg, spool.clone());
    wired(cfg, Box::new(supervisor), Box::new(spool), binary)
}

/// The seams that do not depend on where the orders go.
fn wired(
    cfg: &HandoverConfig,
    supervisor: Box<dyn Supervisor>,
    orders: Box<dyn OrderWriter>,
    binary: RunningBinary,
) -> Parts {
    Parts {
        supervisor,
        orders,
        status: Box::new(JsonFileStatus::new(&cfg.status_file)),
        compat: Box::new(StubGate),
        clock: Arc::new(SystemClock::new()),
        restart: Box::new(binary),
        requests: Idempotency::open(&cfg.state_dir),
        hook: Box::new(NoCrash),
        config: Config {
            window_s: cfg.confirm_window_s,
            staging_deadline: staging_deadline(cfg.fetch_timeout_s),
        },
    }
}

#[cfg(test)]
#[path = "handover_tests.rs"]
mod tests;
