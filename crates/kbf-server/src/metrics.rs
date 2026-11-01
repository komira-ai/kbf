//! `GET /metrics` on the operator API listener: the cache's upload-path metrics
//! ([`kbf_front::metrics`]), the write tier's when one is attached, the lost-blob
//! families read from the metadata state ([`kbf_front::Losses`]), and the fill of the
//! node's one cache when the process has one (`kbf all-in-one`,
//! [`kbf_storage::local_cache`]), and the storage health of a frontend with a state store
//! ([`crate::storage_health`]), in the Prometheus text format, for a scraper or for
//! `curl` while an upload is slow (`docs/api.md#get-metrics`).
//!
//! A read, open like `GET /v1/nodes` and `/readyz`: it names no blob, build or node,
//! only counts and latencies. Every listener that serves the cache (REAPI and the
//! daemons' worker listener) counts into the same families.

use std::fmt::Write as _;
use std::net::SocketAddr;
use std::sync::Arc;

use axum::Router;
use axum::extract::State;
use axum::http::header::CONTENT_TYPE;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use futures::FutureExt as _;
use kbf_front::{Cache, MetaLog};
use kbf_objstore::ObjectStore;
use kbf_storage::local_cache::{Fill, LocalCache};
use kbf_tls::TlsSource;

use crate::farm::Farm;
use crate::listen::{Binder, Socket};
use crate::members::Members;
use crate::proxy_auth::Caller;
use crate::serve::{ServeError, StorageSocket, bind};
use crate::start::AbortOnDrop;

/// The media type of the answer: the Prometheus text exposition format.
pub const TEXT_FORMAT: &str = "text/plain; version=0.0.4; charset=utf-8";

/// What a scrape adds to the cache's own metrics: the node's one cache's fill if the
/// process has one, build history's counters if the server keeps history, and the pack
/// reads the daemons report if the process has a farm. Not generic: nothing here depends on
/// the cache.
#[derive(Clone)]
struct Extras {
    local: Option<Arc<LocalCache>>,
    history: Option<Arc<crate::history::Stats>>,
    farm: Option<Arc<Farm>>,
}

impl Extras {
    /// Appends what there is to `out`.
    fn render(self, out: &mut String) {
        if let Some(local) = self.local {
            render_fill(out, local.fill());
        }
        if let Some(history) = self.history {
            history.render(out);
        }
        if let Some(farm) = self.farm {
            render_farm(out, &farm);
        }
    }
}

/// Appends what a farm reports: the scheduler core its driver is (so the gauge cannot name
/// another), the pack daemons' and input caches' counts, the scheduler's load counters and each
/// node's load, and the failed actions. The one call every server's `/metrics` makes, so the all-in-one and the
/// frontend pages cannot differ.
pub(crate) fn render_farm(out: &mut String, farm: &Farm) {
    crate::scheduler_core::render(out, farm.scheduler_core());
    crate::usage_profiles::render(out, farm.usage_profiles());
    if let Some(stats) = farm.usage_stats() {
        stats.render(out);
    }
    crate::pack_daemon_metrics::render_farm(out, farm);
    crate::input_cache_metrics::render_farm(out, farm);
    crate::load_metrics::render_farm(out, farm);
    if let Some((ablation, counters)) = farm.ablation() {
        ablation.render(out, &counters);
    }
    farm.failed_actions().render(out);
    farm.refusals().render(out);
}

/// What a scrape reads: the cache, and the extras.
struct Sources<M, O> {
    cache: Arc<Cache<M, O>>,
    extras: Extras,
    members: Option<Arc<Members>>,
    tls: Arc<[TlsSource]>,
}

impl<M, O> Clone for Sources<M, O> {
    fn clone(&self) -> Self {
        Self {
            cache: Arc::clone(&self.cache),
            extras: self.extras.clone(),
            members: self.members.clone(),
            tls: Arc::clone(&self.tls),
        }
    }
}

/// The `/metrics` route over `cache`'s metrics, `local`'s fill, when the server keeps
/// build history its counters, the storage health of `members`, the reloads of the
/// listeners' `tls` files and, given the `farm` its daemons register with, the pack reads
/// they report.
pub fn router<M, O>(
    cache: Arc<Cache<M, O>>,
    local: Option<Arc<LocalCache>>,
    history: Option<Arc<crate::history::Stats>>,
    members: Option<Arc<Members>>,
    tls: Vec<TlsSource>,
    farm: Option<Arc<Farm>>,
) -> Router
where
    M: MetaLog,
    O: ObjectStore + 'static,
{
    Router::new()
        .route("/metrics", get(metrics::<M, O>))
        .with_state(Sources {
            cache,
            extras: Extras {
                local,
                history,
                farm,
            },
            members,
            tls: tls.into(),
        })
}

async fn metrics<M, O>(_gated: Caller, State(sources): State<Sources<M, O>>) -> Response
where
    M: MetaLog,
    O: ObjectStore + 'static,
{
    let mut text = sources.cache.render_metrics();
    // Read from the metadata state now; a log that cannot answer leaves them out of
    // this scrape rather than failing it.
    let losses = sources.cache.losses().await;
    losses.iter().for_each(|l| l.render(&mut text));
    sources.extras.render(&mut text);
    scrape_health(&mut text, sources.members.as_ref()).await;
    kbf_tls::render_metrics(sources.tls.iter(), &mut text);
    ([(CONTENT_TYPE, TEXT_FORMAT)], text).into_response()
}

/// A storage node's `/metrics` listener (`--metrics-listen`): the same route, over the
/// node's cache, on a listener of its own, since the node has no operator API. Plain
/// HTTP on any address, as the frontend's operator API is, and for the same reason: it
/// names no blob, build or node, only counts and latencies. Bound with the node's
/// storage socket ([`NodeMetrics::bind`]) so a taken address fails the start, and served
/// once the cache is open ([`NodeMetrics::serve`]).
pub(crate) struct NodeMetrics {
    listener: tokio::net::TcpListener,
    addr: SocketAddr,
}

impl NodeMetrics {
    /// Binds `addr` through `binder` (so a late interface is waited for out of the start's
    /// one budget), on the runtime that will serve it.
    ///
    /// # Errors
    /// The address cannot be bound, or the runtime cannot register the socket.
    pub fn bind(addr: SocketAddr, binder: &Binder) -> Result<Self, ServeError> {
        let socket = Socket::bind(addr, binder)?;
        let addr = socket.local;
        socket.http().map(|listener| Self { listener, addr })
    }

    /// Serves `/metrics` over `cache` (and `local`'s fill, when the process has one
    /// cache for the daemon half and the store; and the reloads of the listener's
    /// `tls`) until the returned task is dropped: where it listens, and the task.
    pub fn serve<M, O>(
        self,
        cache: Arc<Cache<M, O>>,
        local: Option<Arc<LocalCache>>,
        tls: Option<TlsSource>,
    ) -> (SocketAddr, AbortOnDrop)
    where
        M: MetaLog,
        O: ObjectStore + 'static,
    {
        let routes = router(cache, local, None, None, tls.into_iter().collect(), None);
        // `pending` is its stop, so it ends only when the task is aborted; an error of
        // the accept loop has nothing to be reported to.
        // The node is not behind the proxy: no secret, so `gate_requests` admits a direct caller.
        let serving =
            crate::serve::serve_api(Some(self.listener), routes, std::future::pending(), None);
        (self.addr, AbortOnDrop(tokio::spawn(serving.map(drop))))
    }
}

/// A storage node's two listeners, the storage socket and, when `--metrics-listen` is
/// given, the `/metrics` one, bound through one [`Binder`]: both wait for a late interface
/// out of the same budget, as the frontend's three do.
///
/// # Errors
/// Either address cannot be bound.
pub(crate) fn bind_node_sockets(
    listen: SocketAddr,
    metrics: Option<SocketAddr>,
    binder: &Binder,
) -> Result<(StorageSocket, Option<NodeMetrics>), ServeError> {
    let (incoming, addr) = bind(listen, binder)?;
    let socket = StorageSocket { incoming, addr };
    let metrics = metrics
        .map(|addr| NodeMetrics::bind(addr, binder))
        .transpose()?;
    Ok((socket, metrics))
}

/// Appends the storage health of `members`, when there are any; not generic, so every
/// instance of the handler shares it.
pub(crate) async fn scrape_health(out: &mut String, members: Option<&Arc<Members>>) {
    if let Some(members) = members {
        crate::storage_health::scrape(out, members).await;
    }
}

/// Appends the one cache's gauges to `out`.
fn render_fill(out: &mut String, fill: Fill) {
    let gauges = [
        (
            "kbf_local_cache_input_bytes",
            "Bytes of the input cache's entries, in the node's one cache",
            fill.inputs,
        ),
        (
            "kbf_local_cache_storage_bytes",
            "Bytes the node's local storage holds on the one cache's disk: the write tier's blocks",
            fill.storage,
        ),
        (
            "kbf_local_cache_fill_bytes",
            "What the node's one cache holds on its disk, each byte once: the input cache's entries and local storage's bytes",
            fill.total(),
        ),
    ];
    for (name, help, value) in gauges {
        let _ = writeln!(
            out,
            "# HELP {name} {help}\n# TYPE {name} gauge\n{name} {value}"
        );
    }
}
