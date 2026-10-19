//! The active child's operator `/metrics`, fetched for the proxy's own `GET /metrics` to
//! append (design note decision C11).
//!
//! The fetch is one bound stream like any operator call: it takes a permit on the active
//! slot (so a retire waits for it), goes through the same pool, and carries the caller's
//! `Authorization` and the proxy's identity headers. It never waits: while a hold is in
//! force, with no active slot, or on any failure (a refused dial, a status other than 200, a
//! body over [`MAX_BYTES`], no complete answer within [`DEADLINE`]) it gives nothing, the
//! proxy's own series are served alone, and the failure is counted in
//! `kbf_proxy_child_metrics_failed_total`.

use std::time::Duration;

use http::header::AUTHORIZATION;
use http::{HeaderMap, HeaderValue, Method, Request, StatusCode, Version};

use crate::core::Core;
use crate::identity::forward_headers;
use crate::metrics::Metrics;
use crate::route::Target;
use crate::slot::Bind;

/// The largest child body appended.
pub(crate) const MAX_BYTES: usize = 4 << 20;
/// The whole fetch, connect to last byte.
pub(crate) const DEADLINE: Duration = Duration::from_secs(2);

/// The child's `/metrics` body, or nothing (counted as a failure).
pub(crate) async fn fetch(core: &Core, authorization: Option<HeaderValue>) -> Option<Vec<u8>> {
    let body = tokio::time::timeout(DEADLINE, ask(core, authorization))
        .await
        .ok()
        .flatten();
    if body.is_none() {
        Metrics::inc(&core.metrics.child_metrics_failed);
    }
    body
}

async fn ask(core: &Core, authorization: Option<HeaderValue>) -> Option<Vec<u8>> {
    let Bind::Bound(permit) = core.slots.bind() else {
        return None;
    };
    let pool = core
        .pool
        .target(permit.slot(), permit.generation(), Target::Operator);
    let (_reservation, mut send) = crate::pool::TargetPool::acquire(&pool).await.ok()?;
    let mut headers = HeaderMap::new();
    if let Some(value) = authorization {
        headers.insert(AUTHORIZATION, value);
    }
    forward_headers(&mut headers, None, &core.security.secret).ok()?;
    let mut request = Request::new(());
    *request.method_mut() = Method::GET;
    *request.uri_mut() = http::Uri::from_static("http://localhost/metrics");
    *request.headers_mut() = headers;
    *request.version_mut() = Version::HTTP_2;
    let (response, _) = send.send_request(request, true).ok()?;
    let response = response.await.ok()?;
    if response.status() != StatusCode::OK {
        return None;
    }
    let mut body = response.into_body();
    let mut out = Vec::new();
    while let Some(chunk) = body.data().await {
        let chunk = chunk.ok()?;
        // Credit is returned before the cap is judged: the stream is dropped either way.
        let _ = body.flow_control().release_capacity(chunk.len());
        if out.len() + chunk.len() > MAX_BYTES {
            return None;
        }
        out.extend_from_slice(&chunk);
    }
    drop(permit);
    Some(out)
}
