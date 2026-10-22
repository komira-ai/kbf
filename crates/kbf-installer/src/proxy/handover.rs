//! What the update API is made of when the proxy verb hosts it, the path of a proxy without
//! `--kbf-binary`: every seam of [`Parts`](super::api::core::Parts) bound to its real
//! implementation in one function, [`parts`]. A proxy that keeps its own copies of `kbf` does
//! not come here at all (see [`super::assemble`]).
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
//! | `supervisor` | [`Handover`]: the swap, built over the proxy's gate once the verb has bound the proxy and filled [`HandoverConfig::gate`] |
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
//! refused ("proxy not bound yet", [`start_blocked`](super::kinds::Supervisor::start_blocked)),
//! nothing written. It is built on its first call after the fill, which is `Core::new` asking
//! for the status, so an update a predecessor left in flight is resumed before the API serves.
//! See [`super::handover_swap`].
//!
//! **Fail fast, on purpose.** An order spool that cannot be opened ends the start
//! ([`HandoverError::Orders`]), and with it the data plane: the verb exits 2, which the unit
//! does not restart. A host whose `orders/` the service user cannot use is a host where
//! the update API could never work, and a proxy that serves while its update route is dead
//! hides that until the first update. The alternative is to serve and answer 503 on the update
//! route only; take it by returning a `Parts` over a spool that refuses, not by skipping the
//! error here.
//!
//! **The seam with the verb.** [`legacy`] is the one thing the rest of the proxy calls: it builds
//! the seams and hands them back as an [`Assembled`] whose route starts the API's actor and
//! answers through the API's handler. Deleting the update API deletes this module and that one
//! call.

use std::io;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;

use super::api::actor::ActorTiming;
use super::api::core::{Config, Core, NoCrash, Parts, StagingDeadline};
use super::api::handler::{Handler, HandlerRequest};
use super::api::idempotency::Idempotency;
use super::api::traits::{JsonFileStatus, StubGate, SystemClock};
use super::assemble::{Answer, Answered, AssembleError, Assembled, Call, Pending, Route};
use super::handover_swap::{Handover, Orders};
use super::orders::{OrderSpool, SpoolError};
use super::root_orders::SpoolRootOrders;
use super::running_binary::RunningBinary;

pub use super::handover_swap::HandoverConfig;

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

impl From<HandoverError> for AssembleError {
    /// An order spool or state directory the service user cannot use is the operator's to fix:
    /// exit 2, which `kbf-proxy.service` does not restart. A program that cannot be identified
    /// is not.
    fn from(e: HandoverError) -> Self {
        let text = format!("update API: {e}");
        match e {
            HandoverError::Orders { .. } => AssembleError::Usage(text),
            HandoverError::Binary(_) => AssembleError::Failed(text),
        }
    }
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

impl Handover {
    /// A handover whose orders go through `spool` (a clone of the spool the API writes with, so
    /// that the swap's orders and the API's take one counter) and whose staged and installed
    /// releases are read from `cfg.status_file`, the root half's `status.json`.
    #[must_use]
    pub fn new(cfg: &HandoverConfig, spool: OrderSpool) -> Handover {
        let orders: Orders = Arc::new(SpoolRootOrders::new(spool, &cfg.status_file));
        Handover::over(cfg, orders)
    }

    /// [`new`](Self::new) with its ports replaced by `seams`.
    #[cfg(test)]
    pub(super) fn with_seams(
        cfg: &HandoverConfig,
        spool: OrderSpool,
        seams: super::handover_swap::Seams,
    ) -> Handover {
        Handover::new(cfg, spool).seamed(seams)
    }
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
    Ok(parts_over(cfg, spool, binary))
}

/// [`parts`] over a spool and a binary identity it is given.
fn parts_over(cfg: &HandoverConfig, spool: OrderSpool, binary: RunningBinary) -> Parts {
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

/// The update API's route over `parts`, not started: [`legacy`] after [`parts`], split so that a
/// test can change the seams between the two.
pub fn api_over(parts: Parts, cfg: &HandoverConfig) -> Box<dyn Pending> {
    Box::new(Api {
        parts,
        status_file: cfg.status_file.clone(),
        orders_dir: cfg.orders_dir.clone(),
    })
}

/// The update API as the path without `--kbf-binary` serves it: the seams of [`parts`], and
/// nothing running yet. The swap behind it is the API's own (it holds the handover), and no
/// binary watch is wired.
///
/// # Errors
/// [`AssembleError`]: as [`parts`]'s [`HandoverError`] maps (see its `From`).
pub fn legacy(cfg: &HandoverConfig) -> Result<Assembled, AssembleError> {
    Ok(Assembled {
        watch: None,
        pending: api_over(parts(cfg)?, cfg),
    })
}

/// The update API before it runs.
struct Api {
    parts: Parts,
    status_file: PathBuf,
    orders_dir: PathBuf,
}

impl Pending for Api {
    /// What the API will do: the files it reads and writes, its staging deadline, and whether a
    /// `POST` is taken now and, if not, why.
    fn note(&self) -> String {
        let driver = self
            .parts
            .supervisor
            .start_blocked()
            .map_or("handover ready".to_owned(), |why| {
                format!("a POST is refused: {why}")
            });
        format!(
            "update API on (status {}, orders {}, staging deadline {} s); {driver}",
            self.status_file.display(),
            self.orders_dir.display(),
            self.parts.config.staging_deadline.secs(),
        )
    }

    /// Starts the API's actor over the seams. If it dies, `on_death` stops the proxy.
    fn start(self: Box<Self>, on_death: Box<dyn FnOnce() + Send>) -> io::Result<Route> {
        let handler = Handler::start(Core::new(self.parts), ActorTiming::default(), on_death)?;
        Ok(Route::new(Bridge(handler)))
    }
}

/// The update API behind the proxy's update route: the [`Handler`] of the API, called with the
/// proxy's request and answering with the proxy's reply (the fields are the same, and so are
/// the header types). It checks nothing: the proxy authenticated the caller before it called.
struct Bridge(Handler);

#[async_trait]
impl Answer for Bridge {
    async fn answer(&self, call: Call) -> Answered {
        let reply = self
            .0
            .handle(HandlerRequest {
                method: call.method,
                path: call.path,
                body: call.body,
            })
            .await;
        Answered {
            status: reply.status,
            headers: reply.headers,
            body: reply.body,
        }
    }
}

#[cfg(test)]
#[path = "handover_tests.rs"]
mod tests;
