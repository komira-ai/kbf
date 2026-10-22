//! What the proxy verb needs to supervise the swap, built in one place, and what answers on the
//! update route (`/v1/update/frontend*`) in the meantime.
//!
//! The verb asks for an [`Assembled`] ([`assemble`]) and takes from it the binary watch's seams
//! and the update route, not yet started:
//!
//! - **With `--kbf-binary`** ([`HandoverConfig::local`] is set; [`local`]): the swap is a
//!   [`Handover`] over the proxy's own copies of `kbf` ([`Local`]), shared with the binary watch
//!   ([`Wiring`]), which is what starts a swap. The update API is not built: nothing in this
//!   path opens an order spool or reads the root half's files, and the route answers every call
//!   with 410 ([`Gone`]), because the package manager is what updates `kbf` there, except
//!   `GET /v1/proxy/swap`, which reports the state of the swap
//!   ([`SwapStatus`](super::swap_status::SwapStatus)) from the same swap and the same
//!   [`WatchStatus`] the binary watch publishes to. The swap's `abort`, `rollback` and
//!   `force_retire` are then called by nothing: a drain that never idles cannot be seen or ended
//!   through the route, only by restarting the proxy.
//! - **Without it**: the update API as it has always been, built by [`super::handover::legacy`].
//!   That call is the whole of what this module knows about the API, so a build without the API
//!   is this module without that arm. The API does not know `/v1/proxy/swap`: it answers 404.
//!
//! The route is a [`Route`]: the proxy's [`UpdateHandler`] over an [`Answer`], which takes a
//! [`Call`] and gives an [`Answered`], plain values, so that nothing but this module names the
//! proxy's update types. It starts in two steps because the verb builds the seams before the
//! proxy is bound (a seam the service user cannot use is a usage error before anything is bound)
//! and starts what serves the route after: [`Pending::note`] says what the route will do,
//! [`Pending::start`] starts it.

use std::io;
use std::sync::Arc;

use async_trait::async_trait;
use hyper::header::{HeaderName, HeaderValue};
use kbf_proxy::control::{UpdateHandler, UpdateReply, UpdateRequest};

use super::binary_watch::{WatchStatus, Wiring};
use super::handover_swap::{Handover, HandoverConfig};
use super::releases::Local;
use super::runtime::SystemClock;
use super::swap_status::{SwapStatus, boot_id};

/// The body of the 410 every call on the update route gets when the proxy keeps its own copies
/// of `kbf`.
pub const GONE: &str = r#"{"error":"the update API is retired; kbf is updated by its package"}"#;

/// One call on the update route, as plain values.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Call {
    /// `GET` or `POST`.
    pub method: String,
    /// The path: `/v1/update/frontend` and what follows it, or `/v1/proxy/swap`.
    pub path: String,
    /// The JSON body.
    pub body: Vec<u8>,
}

/// The answer to a [`Call`], as plain values.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Answered {
    /// HTTP status.
    pub status: u16,
    /// Response headers other than `Content-Type`; the body is always JSON.
    pub headers: Vec<(HeaderName, HeaderValue)>,
    /// JSON body.
    pub body: Vec<u8>,
}

/// What answers the calls on the update route. The proxy authenticated the caller before it
/// called.
#[async_trait]
pub trait Answer: Send + Sync {
    async fn answer(&self, call: Call) -> Answered;
}

/// The update route as the proxy serves it.
pub struct Route(Box<dyn Answer>);

impl Route {
    #[must_use]
    pub fn new(answer: impl Answer + 'static) -> Route {
        Route(Box::new(answer))
    }
}

impl UpdateHandler for Route {
    async fn handle(&self, request: UpdateRequest) -> UpdateReply {
        let answered = self
            .0
            .answer(Call {
                method: request.method,
                path: request.path,
                body: request.body,
            })
            .await;
        UpdateReply {
            status: answered.status,
            headers: answered.headers,
            body: answered.body,
        }
    }
}

/// The route of a proxy that updates from its own copies: every call is answered 410 with
/// [`GONE`]. Nothing is read or written.
pub struct Gone;

#[async_trait]
impl Answer for Gone {
    async fn answer(&self, _call: Call) -> Answered {
        Answered {
            status: 410,
            headers: Vec::new(),
            body: GONE.as_bytes().to_vec(),
        }
    }
}

/// The update route before it runs.
pub trait Pending {
    /// What the verb says on the standard error about the route.
    fn note(&self) -> String;

    /// Starts whatever serves the route. `on_death` stops the proxy if that dies.
    ///
    /// # Errors
    /// The thread that serves the route could not be started.
    fn start(self: Box<Self>, on_death: Box<dyn FnOnce() + Send>) -> io::Result<Route>;
}

/// What the verb takes from [`assemble`].
pub struct Assembled {
    /// The binary watch's seams: the proxy's copies and the swap it starts through. Set exactly
    /// when the proxy keeps its own copies.
    pub watch: Option<Wiring>,
    /// The update route.
    pub pending: Box<dyn Pending>,
}

/// Why the seams could not be built.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum AssembleError {
    /// Something the operator has to fix (a spool the service user cannot use): the verb exits
    /// 2, which the unit does not restart.
    #[error("{0}")]
    Usage(String),
    /// Something that may pass: the verb exits 1.
    #[error("{0}")]
    Failed(String),
}

/// The seams of the swap and the update route for `cfg`: [`local`] when the proxy keeps its own
/// copies, else the update API's.
///
/// # Errors
/// [`AssembleError`]: the update API's seams could not be built (only without copies).
pub fn assemble(cfg: &HandoverConfig) -> Result<Assembled, AssembleError> {
    match &cfg.local {
        Some(copies) => Ok(local(cfg, copies)),
        None => super::handover::legacy(cfg),
    }
}

/// The swap over the proxy's own copies of `kbf`, for the binary watch to start, and the route
/// that answers 410 and the swap's state. No update API, no order spool, no file of the root
/// half. The binary watch and the route share one [`WatchStatus`], and the route reads the
/// swap through a clone of the one the watch starts.
#[must_use]
pub fn local(cfg: &HandoverConfig, copies: &Local) -> Assembled {
    let swap = Handover::local(cfg, Arc::clone(&copies.orders));
    let status = WatchStatus::default();
    let boot = boot_id(&SystemClock::new(), std::process::id());
    let route = SwapStatus::new(
        Arc::new(swap.clone()),
        Arc::clone(&copies.orders),
        status.clone(),
        boot,
    );
    let note = format!(
        "update API retired, /v1/update/* answers 410; the swap starts when the kbf at {} is \
         replaced (status {})",
        copies.binary.display(),
        cfg.status_file.display()
    );
    Assembled {
        watch: Some(Wiring {
            local: copies.clone(),
            swap,
            status,
        }),
        pending: Box::new(Retired { note, route }),
    }
}

/// The route of [`local`]: [`SwapStatus`], started with nothing to start.
struct Retired {
    note: String,
    route: SwapStatus,
}

impl Pending for Retired {
    fn note(&self) -> String {
        self.note.clone()
    }

    fn start(self: Box<Self>, _on_death: Box<dyn FnOnce() + Send>) -> io::Result<Route> {
        Ok(Route::new(self.route))
    }
}

#[cfg(test)]
#[path = "assemble_tests.rs"]
mod tests;
