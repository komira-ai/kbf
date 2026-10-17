//! `GET /metrics` on the operator API listener: the cache's upload-path metrics
//! ([`kbf_front::metrics`]), the write tier's when one is attached, and the fill of the
//! node's one cache when the process has one (`kbf-all-in-one`,
//! [`kbf_storage::local_cache`]), in the Prometheus text format, for a scraper or for
//! `curl` while an upload is slow (`docs/api.md#get-metrics`).
//!
//! A read, open like `GET /v1/nodes` and `/readyz`: it names no blob, build or node,
//! only counts and latencies. Every listener that serves the cache (REAPI and the
//! daemons' worker listener) counts into the same families.

use std::fmt::Write as _;
use std::sync::Arc;

use axum::Router;
use axum::extract::State;
use axum::http::header::CONTENT_TYPE;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use kbf_front::{Cache, MetaLog};
use kbf_objstore::ObjectStore;
use kbf_storage::local_cache::{Fill, LocalCache};
use kbf_tls::TlsSource;

/// The media type of the answer: the Prometheus text exposition format.
pub const TEXT_FORMAT: &str = "text/plain; version=0.0.4; charset=utf-8";

/// What a scrape reads: the cache, and the node's one cache if the process has one.
struct Sources<M, O> {
    cache: Arc<Cache<M, O>>,
    local: Option<Arc<LocalCache>>,
    tls: Arc<[TlsSource]>,
}

impl<M, O> Clone for Sources<M, O> {
    fn clone(&self) -> Self {
        Self {
            cache: Arc::clone(&self.cache),
            local: self.local.clone(),
            tls: Arc::clone(&self.tls),
        }
    }
}

/// The `/metrics` route over `cache`'s metrics, `local`'s fill, and the reloads of the
/// listeners' `tls` files.
pub fn router<M, O>(
    cache: Arc<Cache<M, O>>,
    local: Option<Arc<LocalCache>>,
    tls: Vec<TlsSource>,
) -> Router
where
    M: MetaLog,
    O: ObjectStore + 'static,
{
    Router::new()
        .route("/metrics", get(metrics::<M, O>))
        .with_state(Sources {
            cache,
            local,
            tls: tls.into(),
        })
}

async fn metrics<M, O>(State(sources): State<Sources<M, O>>) -> Response
where
    M: MetaLog,
    O: ObjectStore + 'static,
{
    let mut text = sources.cache.render_metrics();
    if let Some(local) = sources.local {
        render_fill(&mut text, local.fill());
    }
    kbf_tls::render_metrics(sources.tls.iter(), &mut text);
    ([(CONTENT_TYPE, TEXT_FORMAT)], text).into_response()
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
