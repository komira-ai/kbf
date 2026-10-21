//! A bound on how many reads one caller has open at once, and on the size of the path
//! and query a read names: the guard of `/ui` and of the read routes the pages use
//! (`docs/design/kbf-ui-v1.md`, R3).
//!
//! - **Bounded concurrency per caller.** A caller holds at most [`PER_CALLER`] requests
//!   of one [`ReadLimit`] at a time; the next is answered `429` with `Retry-After`
//!   before its handler runs, so a browser tab (or a script) that opens many slow reads
//!   cannot take every read the server can serve. A request is held until its answer's
//!   body has been sent or dropped, not only until the handler returns: a blob streams
//!   after its handler is done.
//! - **Who a caller is.** The node id the proxy vouched for ([`Trust::Proxied`]), else
//!   the client address the proxy reported (`x-kbf-client-addr`, [`ClientAddr`], again only
//!   of a proxied call), else the peer's IP address. Behind the proxy every connection
//!   comes from the proxy's address and an open `/ui` request names no node, so the
//!   client address is what gives each browser its own bound; a node that carries an
//!   identity has its own too. Without the proxy's header (an older proxy) browsers fall
//!   back to sharing the proxy's address. A direct call that sends the header is not
//!   believed: the key stays its peer address.
//! - **Two limits, not one.** `/ui` and the `/v1` read routes each have their own
//!   [`ReadLimit`], so a page holding its reads open cannot starve the API and the
//!   reverse.
//! - **A path that is a path.** A target longer than [`MAX_PATH_BYTES`] (or a query
//!   longer than [`MAX_QUERY_BYTES`]) is `414`; a segment longer than
//!   [`MAX_SEGMENT_BYTES`], or one that decodes to a control character, is `400`. The
//!   routes parse their own parameters strictly; this keeps a hostile one from reaching
//!   them at all, and from being copied into a page, a log line or an upstream read.
//! - **Nothing is remembered.** The table holds the callers with a request open now and
//!   is empty when the server is idle: its size is bounded by the requests in flight.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{Context, Poll};

use axum::body::{Body, Bytes};
use axum::extract::{ConnectInfo, Request, State};
use axum::http::header::RETRY_AFTER;
use axum::http::{HeaderValue, StatusCode};
use axum::middleware::Next;
use axum::response::Response;
use http_body::{Frame, SizeHint};

use crate::proxy_auth::{ClientAddr, Trust};

/// The requests one caller may have open at once in one [`ReadLimit`].
pub const PER_CALLER: usize = 4;

/// The longest request path, as sent (percent-encoded), in bytes.
pub const MAX_PATH_BYTES: usize = 2048;

/// The longest query, as sent, in bytes.
pub const MAX_QUERY_BYTES: usize = 4096;

/// The longest path segment, as sent, in bytes: an id, a digest or a key is far shorter.
pub const MAX_SEGMENT_BYTES: usize = 256;

/// The `Retry-After` of a `429`, in seconds.
const RETRY_AFTER_SECONDS: &str = "1";

/// Who a request is from, for the bound.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum Caller {
    /// A node the proxy named.
    Node(String),
    /// A peer address.
    Peer(IpAddr),
}

/// How many requests each caller has open now, and the most one may have.
#[derive(Debug)]
pub struct ReadLimit {
    per_caller: usize,
    open: Mutex<HashMap<Caller, usize>>,
}

/// One open request of a caller; the slot returns when it is dropped.
struct Held {
    limit: Arc<ReadLimit>,
    caller: Caller,
}

impl Drop for Held {
    fn drop(&mut self) {
        let mut open = self
            .limit
            .open
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        // The entry exists while this slot does; an absent one counts as none left.
        let left = open.get_mut(&self.caller).map_or(0, |n| {
            *n -= 1;
            *n
        });
        if left == 0 {
            open.remove(&self.caller);
        }
    }
}

impl ReadLimit {
    /// A limit of `per_caller` open requests for each caller.
    #[must_use]
    pub fn new(per_caller: usize) -> Arc<Self> {
        Arc::new(Self {
            per_caller,
            open: Mutex::default(),
        })
    }

    /// The callers with a request open now.
    #[must_use]
    pub fn callers(&self) -> usize {
        self.open
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }

    /// A slot for `caller`, or `None` when it holds [`Self::per_caller`] already.
    fn hold(self: &Arc<Self>, caller: Caller) -> Option<Held> {
        let mut open = self.open.lock().unwrap_or_else(PoisonError::into_inner);
        let n = open.get(&caller).copied().unwrap_or(0);
        if n >= self.per_caller {
            return None;
        }
        open.insert(caller.clone(), n + 1);
        Some(Held {
            limit: Arc::clone(self),
            caller,
        })
    }
}

/// Who `request` is from: the node the proxy named, else the client address the proxy
/// reported, else its peer's address (the unspecified address for a request that has none,
/// which only a miswired router makes). The first two are believed only of a
/// [`Trust::Proxied`] call, whatever the extensions hold.
fn caller_of(request: &Request) -> Caller {
    let extensions = request.extensions();
    if let Some(Trust::Proxied(identity)) = extensions.get::<Trust>() {
        if let Some(node) = &identity.node_id {
            return Caller::Node(node.clone());
        }
        if let Some(ClientAddr(ip)) = extensions.get::<ClientAddr>() {
            return Caller::Peer(*ip);
        }
    }
    let peer = extensions
        .get::<ConnectInfo<SocketAddr>>()
        .map_or(IpAddr::from([0, 0, 0, 0]), |c| c.0.ip().to_canonical());
    Caller::Peer(peer)
}

/// Why `request`'s path or query is refused, if it is.
fn invalid(request: &Request) -> Option<(StatusCode, &'static str)> {
    let uri = request.uri();
    let path = uri.path();
    if path.len() > MAX_PATH_BYTES {
        return Some((StatusCode::URI_TOO_LONG, "the path is too long"));
    }
    if uri.query().is_some_and(|q| q.len() > MAX_QUERY_BYTES) {
        return Some((StatusCode::URI_TOO_LONG, "the query is too long"));
    }
    for segment in path.split('/') {
        if segment.len() > MAX_SEGMENT_BYTES {
            return Some((StatusCode::BAD_REQUEST, "a path segment is too long"));
        }
        let decoded = percent_encoding::percent_decode_str(segment);
        if decoded.into_iter().any(|b| b.is_ascii_control()) {
            return Some((
                StatusCode::BAD_REQUEST,
                "a path segment holds a control character",
            ));
        }
    }
    None
}

/// The middleware: validates the target, takes a slot for the caller (`429` when it has
/// none free), runs the request, and keeps the slot until the answer's body is done.
pub async fn limited(
    State(limit): State<Arc<ReadLimit>>,
    request: Request,
    next: Next,
) -> Response {
    if let Some((code, why)) = invalid(&request) {
        return crate::api::error(code, why);
    }
    let Some(held) = limit.hold(caller_of(&request)) else {
        let mut busy = crate::api::error(
            StatusCode::TOO_MANY_REQUESTS,
            "this caller has too many reads open: wait for one to finish",
        );
        busy.headers_mut()
            .insert(RETRY_AFTER, HeaderValue::from_static(RETRY_AFTER_SECONDS));
        return busy;
    };
    let (head, inner) = next.run(request).await.into_parts();
    Response::from_parts(head, Body::new(Guarded { inner, _held: held }))
}

/// A body that holds a slot until it is dropped, and is otherwise the body it wraps:
/// its size is the inner body's, so a sized answer keeps its `Content-Length`.
struct Guarded {
    inner: Body,
    _held: Held,
}

impl http_body::Body for Guarded {
    type Data = Bytes;
    type Error = axum::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, axum::Error>>> {
        Pin::new(&mut self.inner).poll_frame(cx)
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}

#[cfg(test)]
mod tests;
