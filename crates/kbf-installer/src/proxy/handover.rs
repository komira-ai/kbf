//! What the update API is made of when the proxy verb hosts it: every seam of
//! [`Parts`](super::api::core::Parts) bound to its real implementation in one function,
//! [`parts`].
//!
//! | seam | here |
//! |---|---|
//! | `status` | [`JsonFileStatus`] over the root half's `status.json`, read when a call needs it (a file that is not there yet is an unreadable status, not a failure to start) |
//! | `orders` | the [`OrderSpool`] of `orders/`, opened here, once |
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

use std::io;
use std::path::PathBuf;
use std::sync::Arc;

use super::api::core::{Config, NoCrash, Parts, StagingDeadline};
use super::api::idempotency::Idempotency;
use super::api::traits::{JsonFileStatus, StubGate, SystemClock};
use super::handover_swap::{GateSlot, Handover, SlotAddrs};
use super::orders::{OrderSpool, SpoolError};
use super::running_binary::RunningBinary;

/// What [`parts`] is built from.
#[derive(Clone, Debug)]
pub struct HandoverConfig {
    /// The proxy's state directory: `requests.json` (the API's request map) and `orders.json`
    /// (the spool's counter) are written here.
    pub state_dir: PathBuf,
    /// The order spool, `orders/`: owned by the service user, read by the root half.
    pub orders_dir: PathBuf,
    /// The root half's `status.json`.
    pub status_file: PathBuf,
    /// The confirm window the orders ask for (`confirm_window_s` of `installer.toml`).
    pub confirm_window_s: u32,
    /// How long the root half's fetch may take (`fetch_timeout_s` of `installer.toml`).
    pub fetch_timeout_s: u32,
    /// The loopback ports of the two children's admin and operator listeners, by slot.
    pub slots: SlotAddrs,
    /// The operator token the swap presents to a child's operator API (`desired`).
    pub operator_token_file: PathBuf,
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
/// and the request map; reads no file (`status.json` is read when a call needs it).
///
/// # Errors
/// [`HandoverError`]: the order spool cannot be opened, or the running program cannot be
/// identified.
pub fn parts(cfg: &HandoverConfig) -> Result<Parts, HandoverError> {
    let spool = OrderSpool::open(&cfg.orders_dir, &cfg.state_dir).map_err(|source| {
        HandoverError::Orders {
            dir: cfg.orders_dir.clone(),
            source,
        }
    })?;
    let binary = RunningBinary::capture().map_err(HandoverError::Binary)?;
    Ok(assemble(cfg, spool, binary))
}

/// [`parts`] over a spool and a binary identity it is given.
fn assemble(cfg: &HandoverConfig, spool: OrderSpool, binary: RunningBinary) -> Parts {
    Parts {
        supervisor: Box::new(Handover::new(cfg, spool.clone())),
        orders: Box::new(spool),
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
