//! What the update API is made of when the proxy verb hosts it: every seam of
//! [`Parts`](super::api::core::Parts) bound to its real implementation in one function,
//! [`parts`], and the one seam that has no implementation yet bound to a refusal.
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
//! | `supervisor` | `NotWired`: no swap runs behind the API yet |
//!
//! **One spool per `orders/` directory.** [`OrderSpool::open`] removes the half-written files
//! of an earlier run, so a second spool over the same directory, in this process or another,
//! would delete the file the first is writing. [`parts`] opens the only one. The swap writes
//! its `confirm` and `revert` orders through the same counter, so the supervisor that
//! replaces `NotWired` takes a clone of that spool (a clone shares the counter and the lock)
//! for its `SpoolRootOrders`, and never opens `orders/` itself.
//!
//! **What the API does without a driver.** A `GET` answers the status of "no update". A `POST`
//! is refused with 503 `handover_unavailable` before its request id is saved or an order
//! written ([`Supervisor::start_blocked`]), so the root half does not fetch and stage a
//! release that nothing can take over. Nothing here changes what the proxy serves.
//!
//! **Fail fast, on purpose.** An order spool that cannot be opened ends the start
//! ([`HandoverError::Orders`]), and with it the data plane: the verb exits 2, which the unit
//! does not restart. While no driver is wired the spool is not even written to, so this is a
//! choice and not a need: a host whose `orders/` the service user cannot use is a host where
//! the update API could never work, and a proxy that serves while its update route is dead
//! hides that until the first update. The alternative is to serve and answer 503 on the update
//! route only; take it by returning a `Parts` over a spool that refuses, not by skipping the
//! error here.
//!
//! **What the swap adapter's PR changes here.**
//! - The adapter needs the proxy's gate, which the verb builds after it has bound its
//!   listeners, and [`parts`] is called before the bind (so that a spool error is a usage error
//!   before anything is bound). The adapter's PR builds the parts after the gate exists, or
//!   passes the gate in [`HandoverConfig`]. `Core::new` asks the supervisor for its status
//!   (the resume of an update in flight), so the supervisor must exist before the `Core`.
//! - [`HandoverConfig`] holds only what the seams above read. The adapter's PR adds what it
//!   needs besides: the slot addresses, the proxy secret and the token path.

use std::io;
use std::path::PathBuf;
use std::sync::Arc;

use super::api::core::{Config, NoCrash, Parts, StagingDeadline};
use super::api::idempotency::Idempotency;
use super::api::traits::{
    JsonFileStatus, Retired, StubGate, Supervisor, SupervisorError, SupervisorStatus, SystemClock,
    UpdateSpec,
};
use super::model::Phase;
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
        // The swap's supervisor goes here, with `spool.clone()` for its `SpoolRootOrders`
        // (before `spool` moves into `orders`).
        supervisor: Box::new(NotWired),
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

/// The words of every refusal of a handover that has no driver.
pub const NOT_WIRED: &str = "handover driver not wired";

/// The supervisor of an API with no swap behind it: it has no update and starts none. A
/// `POST` is refused up front ([`Supervisor::start_blocked`]); `start` itself, which only an
/// update accepted by an earlier process that had a driver could reach, fails, and the API
/// ends that update `failed` at step `start` and reverts its staged release.
struct NotWired;

impl Supervisor for NotWired {
    fn start_blocked(&self) -> Option<String> {
        Some(NOT_WIRED.to_owned())
    }

    fn start(&mut self, _spec: &UpdateSpec) -> Result<(), SupervisorError> {
        Err(SupervisorError::Failed(NOT_WIRED.to_owned()))
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

#[cfg(test)]
#[path = "handover_tests.rs"]
mod tests;
