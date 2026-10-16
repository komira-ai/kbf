//! `GET /metrics` on the operator API listener: the cache's upload-path metrics
//! ([`kbf_front::metrics`]), the write tier's when one is attached, the lost-blob
//! families read from the metadata state ([`kbf_front::Losses`]), and the fill of the
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

/// The media type of the answer: the Prometheus text exposition format.
pub const TEXT_FORMAT: &str = "text/plain; version=0.0.4; charset=utf-8";

/// What a scrape reads: the cache, and the node's one cache if the process has one.
struct Sources<M, O> {
    cache: Arc<Cache<M, O>>,
    local: Option<Arc<LocalCache>>,
}

impl<M, O> Clone for Sources<M, O> {
    fn clone(&self) -> Self {
        Self {
            cache: Arc::clone(&self.cache),
            local: self.local.clone(),
        }
    }
}

/// The `/metrics` route over `cache`'s metrics, and `local`'s fill.
pub fn router<M, O>(cache: Arc<Cache<M, O>>, local: Option<Arc<LocalCache>>) -> Router
where
    M: MetaLog,
    O: ObjectStore + 'static,
{
    Router::new()
        .route("/metrics", get(metrics::<M, O>))
        .with_state(Sources { cache, local })
}

async fn metrics<M, O>(State(sources): State<Sources<M, O>>) -> Response
where
    M: MetaLog,
    O: ObjectStore + 'static,
{
    let mut text = sources.cache.render_metrics();
    // Read from the metadata state now; a log that cannot answer leaves them out of
    // this scrape rather than failing it.
    let losses = sources.cache.losses().await;
    losses.iter().for_each(|l| l.render(&mut text));
    if let Some(local) = sources.local {
        render_fill(&mut text, local.fill());
    }
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
