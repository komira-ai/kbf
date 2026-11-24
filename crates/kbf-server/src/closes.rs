//! How each REAPI call on the frontend's REAPI listener ended, counted by method and cause
//! (`kbf_reapi_calls_closed_total`, on `GET /metrics`), with one log line for every call that
//! did not end with an answer the client asked for: so a build's cut streams are counted by
//! the frontend itself, for every build, and not read back from the clients' logs.
//!
//! A call ends one of two ways. Its response carries a gRPC status (in the trailers, or in
//! the headers of a response that is nothing else), which is read as it passes; or it is
//! dropped before that, its stream reset or its connection gone, and then the connection's
//! [`Liveness`] and the drain say why. The record is made when the call's response, or the
//! call before there is one, is dropped, which the transport does once it is done with it.
//!
//! What is counted is what the server did: a call whose status the transport has taken
//! ends with that status, even when the connection is lost or closed before the bytes reach
//! the client (a download that fits in the client's flow-control window, an Execute stream's
//! last operation). The connection's own close is logged by [`crate::conn_watch`].

use std::fmt::Write as _;
use std::future::Future;
use std::net::IpAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context, Poll};

use kbf_front::CallMetadata;
use kbf_tls::{ConnInfo, Liveness};
use tokio::time::Instant;
use tonic::Code;
use tonic::codegen::{Service, http};
use tower_layer::Layer;

use crate::drain::Drain;
use crate::proxy_auth::ClientAddr;

/// The metric family.
const FAMILY: &str = "kbf_reapi_calls_closed_total";

/// The REAPI methods counted by name, each by its gRPC path; any other path is `other`.
const METHODS: [(&str, &str); 14] = [
    (
        "/build.bazel.remote.execution.v2.Execution/Execute",
        "Execute",
    ),
    (
        "/build.bazel.remote.execution.v2.Execution/WaitExecution",
        "WaitExecution",
    ),
    (
        "/build.bazel.remote.execution.v2.ActionCache/GetActionResult",
        "GetActionResult",
    ),
    (
        "/build.bazel.remote.execution.v2.ActionCache/UpdateActionResult",
        "UpdateActionResult",
    ),
    (
        "/build.bazel.remote.execution.v2.ContentAddressableStorage/FindMissingBlobs",
        "FindMissingBlobs",
    ),
    (
        "/build.bazel.remote.execution.v2.ContentAddressableStorage/BatchUpdateBlobs",
        "BatchUpdateBlobs",
    ),
    (
        "/build.bazel.remote.execution.v2.ContentAddressableStorage/BatchReadBlobs",
        "BatchReadBlobs",
    ),
    (
        "/build.bazel.remote.execution.v2.ContentAddressableStorage/GetTree",
        "GetTree",
    ),
    (
        "/build.bazel.remote.execution.v2.Capabilities/GetCapabilities",
        "GetCapabilities",
    ),
    ("/google.bytestream.ByteStream/Read", "Read"),
    ("/google.bytestream.ByteStream/Write", "Write"),
    (
        "/google.bytestream.ByteStream/QueryWriteStatus",
        "QueryWriteStatus",
    ),
    (
        "/google.longrunning.Operations/GetOperation",
        "GetOperation",
    ),
    (
        "/google.longrunning.Operations/CancelOperation",
        "CancelOperation",
    ),
];

/// The label of a path no method of [`METHODS`] has.
const OTHER: &str = "other";

/// Rows of the counts: one per method of [`METHODS`], and [`OTHER`].
const ROWS: usize = METHODS.len() + 1;

/// The row of the method at `path`.
fn row_of(path: &str) -> usize {
    METHODS
        .iter()
        .position(|(known, _)| *known == path)
        .unwrap_or(METHODS.len())
}

/// The method label of `row`.
fn method_label(row: usize) -> &'static str {
    METHODS.get(row).map_or(OTHER, |(_, label)| label)
}

/// How a call ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Cause {
    /// Its status was OK.
    Ok,
    /// Its status was an error the request called for (a cache miss's NOT_FOUND, an
    /// INVALID_ARGUMENT, a refused credential): the call was answered.
    ClientError,
    /// Its status was an error of the server (INTERNAL, UNAVAILABLE outside a drain, ...),
    /// or it ended with no status at all, or its `grpc-timeout` passed before its answer
    /// (the transport answers it DEADLINE_EXCEEDED, outside this layer).
    ServerError,
    /// The client reset its stream, and its connection stayed.
    ClientCancel,
    /// Its connection was lost (closed or failed by the client's side, or by the path), or
    /// it had none to ask, before a status.
    ConnectionLost,
    /// The server closed its connection because the connection moved nothing either way for
    /// the keepalive's limit ([`crate::conn_watch`]).
    Keepalive,
    /// It ended UNAVAILABLE while the drain refused new calls (a stop's drain, or a retire's
    /// refusal), or the drain closed the open streams (its deadline, a second stop) before
    /// it ended.
    Drain,
}

impl Cause {
    /// Every cause, in the order they are exposed.
    pub(crate) const ALL: [Self; 7] = [
        Self::Ok,
        Self::ClientError,
        Self::ServerError,
        Self::ClientCancel,
        Self::ConnectionLost,
        Self::Keepalive,
        Self::Drain,
    ];

    /// The `cause` label.
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::ClientError => "client_error",
            Self::ServerError => "server_error",
            Self::ClientCancel => "client_cancel",
            Self::ConnectionLost => "connection_lost",
            Self::Keepalive => "keepalive",
            Self::Drain => "drain",
        }
    }

    /// The cause of a call that ended with status `code`, while the drain was (`refusing`) or
    /// was not refusing new calls.
    fn of_status(code: Code, refusing: bool) -> Self {
        match code {
            Code::Ok => Self::Ok,
            Code::Unavailable if refusing => Self::Drain,
            Code::InvalidArgument
            | Code::NotFound
            | Code::AlreadyExists
            | Code::PermissionDenied
            | Code::FailedPrecondition
            | Code::OutOfRange
            | Code::Unimplemented
            | Code::Unauthenticated => Self::ClientError,
            Code::Cancelled
            | Code::Unknown
            | Code::DeadlineExceeded
            | Code::ResourceExhausted
            | Code::Aborted
            | Code::Internal
            | Code::Unavailable
            | Code::DataLoss => Self::ServerError,
        }
    }
}

/// The counts of `kbf_reapi_calls_closed_total`, by method and cause.
#[derive(Debug)]
pub(crate) struct CloseMetrics {
    counts: [[AtomicU64; Cause::ALL.len()]; ROWS],
}

impl Default for CloseMetrics {
    fn default() -> Self {
        Self {
            counts: std::array::from_fn(|_| std::array::from_fn(|_| AtomicU64::new(0))),
        }
    }
}

impl CloseMetrics {
    fn add(&self, row: usize, cause: Cause) {
        self.counts[row][cause as usize].fetch_add(1, Ordering::Relaxed);
    }

    /// The calls of `method` (a label: `Execute`, `other`, ...) that ended by `cause`.
    #[cfg(test)]
    pub(crate) fn of(&self, method: &str, cause: Cause) -> u64 {
        let row = (0..ROWS)
            .find(|row| method_label(*row) == method)
            .expect("a method label");
        self.counts[row][cause as usize].load(Ordering::Relaxed)
    }

    /// Appends the family to `out`, every series of it, so a series that is absent is never
    /// mistaken for one that is 0.
    pub(crate) fn render(&self, out: &mut String) {
        let _ = writeln!(
            out,
            "# HELP {FAMILY} REAPI calls the REAPI listener ended, by method and by how each ended."
        );
        let _ = writeln!(out, "# TYPE {FAMILY} counter");
        for (row, counts) in self.counts.iter().enumerate() {
            for cause in Cause::ALL {
                let _ = writeln!(
                    out,
                    "{FAMILY}{{method=\"{}\",cause=\"{}\"}} {}",
                    method_label(row),
                    cause.label(),
                    counts[cause as usize].load(Ordering::Relaxed)
                );
            }
        }
    }
}

/// Records how every call of the service it wraps ended, in `metrics`, and logs the calls
/// that did not end with an answer (see [`Cause`]): OK, or an error the request called
/// for. It must wrap the REAPI routes inside the `RequestMetadata` layer, whose ids it
/// logs, and inside the proxy check, whose client address it logs.
#[derive(Clone, Debug)]
pub(crate) struct ClosesLayer {
    shared: Arc<Shared>,
}

#[derive(Debug)]
struct Shared {
    metrics: Arc<CloseMetrics>,
    drain: Arc<Drain>,
}

impl ClosesLayer {
    /// Counts into `metrics`; `drain` says which ends are the drain's.
    pub(crate) fn new(metrics: Arc<CloseMetrics>, drain: Arc<Drain>) -> Self {
        Self {
            shared: Arc::new(Shared { metrics, drain }),
        }
    }
}

impl<S> Layer<S> for ClosesLayer {
    type Service = Closes<S>;

    fn layer(&self, inner: S) -> Self::Service {
        Closes {
            inner,
            shared: Arc::clone(&self.shared),
        }
    }
}

/// The service [`ClosesLayer`] wraps around the REAPI routes.
#[derive(Clone, Debug)]
pub(crate) struct Closes<S> {
    inner: S,
    shared: Arc<Shared>,
}

impl<S, B, R> Service<http::Request<B>> for Closes<S>
where
    S: Service<http::Request<B>, Response = http::Response<R>>,
    S::Future: Send + 'static,
{
    type Response = http::Response<ClosedBody<R>>;
    type Error = S::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, request: http::Request<B>) -> Self::Future {
        let mut call = Call::open(&self.shared, &request);
        let answer = self.inner.call(request);
        Box::pin(async move {
            // An error of the service is an end with no status: the transport answers it.
            let response = answer.await.inspect_err(|_| call.ended = true)?;
            call.status_in(response.headers());
            Ok(response.map(|inner| ClosedBody { inner, call }))
        })
    }
}

/// A response body that records its call's end when it is dropped.
#[derive(Debug)]
pub(crate) struct ClosedBody<B> {
    inner: B,
    call: Call,
}

impl<B> http_body::Body for ClosedBody<B>
where
    B: http_body::Body + Unpin,
{
    type Data = B::Data;
    type Error = B::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<http_body::Frame<Self::Data>, Self::Error>>> {
        let this = &mut *self;
        let polled = Pin::new(&mut this.inner).poll_frame(cx);
        match &polled {
            Poll::Ready(Some(Ok(frame))) => {
                if let Some(trailers) = frame.trailers_ref() {
                    this.call.status_in(trailers);
                }
            }
            Poll::Ready(Some(Err(_)) | None) => this.call.ended = true,
            Poll::Pending => {}
        }
        polled
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> http_body::SizeHint {
        self.inner.size_hint()
    }
}

/// One call, recorded when it is dropped.
#[derive(Debug)]
struct Call {
    shared: Arc<Shared>,
    row: usize,
    began: Instant,
    /// The connection's flag, if the call came over a listener's connection.
    live: Option<Liveness>,
    /// The client's address: the one the proxy names, or the connection's peer.
    peer: Option<String>,
    metadata: Option<Arc<CallMetadata>>,
    /// The `x-request-id` a proxy in front (Envoy) sent, to find the call in its log.
    request_id: Option<String>,
    /// When the call's `grpc-timeout` runs out, if it sent one.
    deadline: Option<Instant>,
    /// The status the response carried, once it was seen.
    status: Option<Code>,
    /// The response ended, or failed, without a status being seen.
    ended: bool,
}

/// The longest `x-request-id` kept.
const MAX_REQUEST_ID: usize = 128;

impl Call {
    fn open<B>(shared: &Arc<Shared>, request: &http::Request<B>) -> Self {
        let extensions = request.extensions();
        let info = extensions.get::<ConnInfo>();
        let client = extensions.get::<ClientAddr>().map(|ClientAddr(ip)| *ip);
        let peer = peer_of(client, info.and_then(|info| info.tcp.remote_addr));
        let request_id = request
            .headers()
            .get("x-request-id")
            .and_then(|value| value.to_str().ok())
            .filter(|id| id.len() <= MAX_REQUEST_ID && id.bytes().all(|b| b.is_ascii_graphic()))
            .map(str::to_owned);
        let began = Instant::now();
        let timeout = request.headers().get("grpc-timeout");
        let deadline = timeout.and_then(|t| grpc_timeout(t.as_bytes()));
        Self {
            shared: Arc::clone(shared),
            row: row_of(request.uri().path()),
            began,
            deadline: deadline.and_then(|after| began.checked_add(after)),
            live: info.map(|info| info.live.clone()),
            peer,
            metadata: extensions.get::<Arc<CallMetadata>>().cloned(),
            request_id,
            status: None,
            ended: false,
        }
    }

    /// Takes the status in `headers` (a response's, or its trailers), if there is one.
    fn status_in(&mut self, headers: &http::HeaderMap) {
        if let Some(status) = headers.get("grpc-status") {
            self.status = Some(Code::from_bytes(status.as_bytes()));
        }
    }

    /// How the call ended, now that it is dropped.
    fn cause(&self) -> Cause {
        let drain = &self.shared.drain;
        if let Some(code) = self.status {
            let refusing = code == Code::Unavailable && drain.is_refusing();
            return Cause::of_status(code, refusing);
        }
        if self.ended {
            return Cause::ServerError;
        }
        match &self.live {
            Some(live) if live.closed_quiet() => Cause::Keepalive,
            _ if drain.is_closed() => Cause::Drain,
            Some(live) if !live.is_gone() => Cause::ClientCancel,
            _ => Cause::ConnectionLost,
        }
    }
}

/// The timeout of a `grpc-timeout` header's `value`: one to eight digits and a unit (`H`,
/// `M`, `S`, `m`, `u`, `n`); `None` for anything else, which the transport ignores too.
fn grpc_timeout(value: &[u8]) -> Option<std::time::Duration> {
    use std::time::Duration;
    let (unit, digits) = value.split_last()?;
    let ok = (1..=8).contains(&digits.len()) && digits.iter().all(u8::is_ascii_digit);
    let n: u64 = std::str::from_utf8(digits)
        .ok()
        .filter(|_| ok)?
        .parse()
        .ok()?;
    match unit {
        b'H' => Some(Duration::from_secs(n * 3600)),
        b'M' => Some(Duration::from_secs(n * 60)),
        b'S' => Some(Duration::from_secs(n)),
        b'm' => Some(Duration::from_millis(n)),
        b'u' => Some(Duration::from_micros(n)),
        b'n' => Some(Duration::from_nanos(n)),
        _ => None,
    }
}

/// The client's address as logged: the proxy's `client` if it named one, else the
/// connection's peer.
fn peer_of(client: Option<IpAddr>, connection: Option<std::net::SocketAddr>) -> Option<String> {
    match client {
        Some(ip) => Some(ip.to_string()),
        None => connection.map(|addr| addr.to_string()),
    }
}

impl Drop for Call {
    fn drop(&mut self) {
        // Dropped with no status past its deadline, on a connection that is there: the
        // transport's timeout dropped it, and answered DEADLINE_EXCEEDED itself.
        let live = self.live.as_ref().is_some_and(|live| !live.is_gone());
        let timed_out = self.deadline.is_some_and(|at| Instant::now() >= at);
        if self.status.is_none() && !self.ended && live && timed_out {
            self.status = Some(Code::DeadlineExceeded);
        }
        let cause = self.cause();
        self.shared.metrics.add(self.row, cause);
        if matches!(cause, Cause::Ok | Cause::ClientError) {
            return;
        }
        let method = method_label(self.row);
        let status = self
            .status
            .map_or_else(|| "-".to_owned(), |c| format!("{c:?}"));
        let peer = self.peer.as_deref().unwrap_or("-");
        let metadata = self.metadata.as_deref();
        let invocation = metadata.map_or("-", CallMetadata::invocation_display);
        let action = metadata.map_or("", |m| m.action_id.value.as_str());
        let request_id = self.request_id.as_deref().unwrap_or("-");
        let duration_ms = u64::try_from(self.began.elapsed().as_millis()).unwrap_or(u64::MAX);
        let label = cause.label();
        // INFO for a client's cancel, which a client sends whenever it no longer needs an
        // answer (an interrupted build); WARN for every other end without an answer.
        if cause == Cause::ClientCancel {
            tracing::info!(method, cause = label, status, peer, invocation, action = ?action, request_id, duration_ms, "{MESSAGE}");
        } else {
            tracing::warn!(method, cause = label, status, peer, invocation, action = ?action, request_id, duration_ms, "{MESSAGE}");
        }
    }
}

/// The message of a close's log line.
const MESSAGE: &str = "a REAPI call ended without the answer it asked for";

#[cfg(test)]
#[path = "closes_tests.rs"]
mod tests;
