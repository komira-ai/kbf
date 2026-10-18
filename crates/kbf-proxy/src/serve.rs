//! The accept loop, the per-connection limits and the per-request dispatch.
//!
//! Plaintext HTTP/2 for now (U4b-1): TLS, identity and auth arrive in U4b-2 at the one place a
//! request is classified ([`Service::handle`]). Until then this must not face a network.

use std::collections::HashMap;
use std::future::Future;
use std::io;
use std::net::IpAddr;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use http::{HeaderValue, Request, Response, StatusCode, header};
use http_body_util::{BodyExt, Limited};
use hyper::body::Incoming;
use hyper::server::conn::http2;
use hyper::service::service_fn;
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::graceful::GracefulShutdown;
use tokio::net::TcpListener;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, watch};
use tokio::task::JoinHandle;

use crate::body::ProxyBody;
use crate::control::{MAX_UPDATE_BODY, UPDATE_DEADLINE, UpdateHandler, UpdateReply, UpdateRequest};
use crate::core::{CONNECTION_STREAMS, Core};
use crate::hold::STREAM_WINDOW_BYTES;
use crate::metrics::Metrics;
use crate::pool::BoxIo;
use crate::reply::{from_update, is_grpc, text, unknown};
use crate::route::{Route, route_of};
use crate::stream::proxy_child;

/// Where connections come from: the public socket, or an in-memory queue in the tests.
pub(crate) trait Listen: Send + 'static {
    /// The next connection and its peer's address.
    fn accept(&mut self) -> impl Future<Output = io::Result<(BoxIo, IpAddr)>> + Send;
}

impl Listen for TcpListener {
    async fn accept(&mut self) -> io::Result<(BoxIo, IpAddr)> {
        let (tcp, peer) = Self::accept(self).await?;
        tcp.set_nodelay(true)?;
        Ok((Box::new(tcp), peer.ip()))
    }
}

/// How long a shutdown waits for open streams to finish before the connections are dropped.
pub(crate) const SHUTDOWN_DRAIN: Duration = Duration::from_secs(5);

/// A stream that panicked: hyper resets that one stream (`INTERNAL_ERROR`), nothing else.
#[derive(Debug, thiserror::Error)]
#[error("the stream panicked")]
pub(crate) struct StreamPanicked;

/// Stops a task when its handle drops. A plain `JoinHandle` detaches instead, and a client
/// that reset its stream would leave a held ticket and a permit behind.
struct AbortOnDrop<T>(JoinHandle<T>);

impl<T> Drop for AbortOnDrop<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}

pub(crate) struct Service<U> {
    core: Arc<Core>,
    updates: Arc<U>,
}

impl<U: UpdateHandler + 'static> Service<U> {
    pub(crate) fn new(core: Arc<Core>, updates: U) -> Arc<Self> {
        Arc::new(Self {
            core,
            updates: Arc::new(updates),
        })
    }

    /// Runs one request in its own task, so a panic is that stream's `JoinError` and not a
    /// connection's, and the task stops with the request if the client goes away.
    pub(crate) async fn call(
        self: Arc<Self>,
        request: Request<Incoming>,
    ) -> Result<Response<ProxyBody>, StreamPanicked> {
        let core = Arc::clone(&self.core);
        let mut task = AbortOnDrop(tokio::spawn(async move { self.handle(request).await }));
        (&mut task.0).await.map_err(|_| {
            Metrics::inc(&core.metrics.panics);
            StreamPanicked
        })
    }

    async fn handle(self: Arc<Self>, request: Request<Incoming>) -> Response<ProxyBody> {
        let grpc = is_grpc(request.headers());
        // Counted from accept: a bypass stream is one for its whole life, a backend stream
        // until its permit exists.
        let pending = self.core.slots.begin_stream();
        match route_of(request.uri().path()) {
            Route::Child(target) => {
                proxy_child(Arc::clone(&self.core), pending, target, request).await
            }
            Route::Update => self.update(request).await,
            Route::Health => text(StatusCode::OK, "ok"),
            // Readiness has one writer, the proxy's own poll of the active child (U4b-3).
            Route::Ready => text(StatusCode::SERVICE_UNAVAILABLE, "not ready"),
            Route::Metrics => {
                let body = self.core.metrics.render(&self.core.slots, &self.core.queue);
                let mut response = text(StatusCode::OK, body);
                response.headers_mut().insert(
                    header::CONTENT_TYPE,
                    HeaderValue::from_static("text/plain; version=0.0.4"),
                );
                response
            }
            Route::Unknown => unknown(grpc),
        }
    }

    /// The update API: the proxy reads the body (at most [`MAX_UPDATE_BODY`]) and bounds the
    /// whole call by [`UPDATE_DEADLINE`]; the handler is the only thing it calls.
    async fn update(&self, request: Request<Incoming>) -> Response<ProxyBody> {
        let reply = tokio::time::timeout(UPDATE_DEADLINE, self.run_update(request))
            .await
            .unwrap_or_else(|_| UpdateReply::timed_out());
        from_update(reply)
    }

    async fn run_update(&self, request: Request<Incoming>) -> UpdateReply {
        let (parts, body) = request.into_parts();
        let Ok(body) = Limited::new(body, MAX_UPDATE_BODY).collect().await else {
            return UpdateReply::payload_too_large();
        };
        let request = UpdateRequest {
            method: parts.method.to_string(),
            path: parts.uri.path().to_owned(),
            body: body.to_bytes().to_vec(),
        };
        // Its own task: a 504 drops this future, and the handler (commands are idempotent)
        // still finishes. A panic goes on as the stream's panic.
        let updates = Arc::clone(&self.updates);
        let task = tokio::spawn(async move { updates.handle(request).await });
        match task.await {
            Ok(reply) => reply,
            Err(error) => std::panic::resume_unwind(error.into_panic()),
        }
    }
}

/// The per-connection limits: total, and per client address.
pub(crate) struct ConnLimits {
    total: Arc<Semaphore>,
    per_ip: Mutex<HashMap<IpAddr, usize>>,
    max_per_ip: usize,
}

/// One accepted connection's place under the limits.
pub(crate) struct ConnPermit {
    _total: OwnedSemaphorePermit,
    limits: Arc<ConnLimits>,
    ip: IpAddr,
}

impl ConnLimits {
    pub(crate) fn new(max_connections: usize, max_per_ip: usize) -> Arc<Self> {
        Arc::new(Self {
            total: Arc::new(Semaphore::new(max_connections)),
            per_ip: Mutex::new(HashMap::new()),
            max_per_ip,
        })
    }

    /// A place for one more connection from `ip`, or nothing: the connection is closed.
    pub(crate) fn admit(self: &Arc<Self>, ip: IpAddr) -> Option<ConnPermit> {
        let total = Arc::clone(&self.total).try_acquire_owned().ok()?;
        let mut per_ip = self.per_ip.lock().unwrap_or_else(PoisonError::into_inner);
        let held = per_ip.entry(ip).or_insert(0);
        if *held >= self.max_per_ip {
            return None;
        }
        *held += 1;
        Some(ConnPermit {
            _total: total,
            limits: Arc::clone(self),
            ip,
        })
    }
}

impl Drop for ConnPermit {
    fn drop(&mut self) {
        let mut per_ip = self
            .limits
            .per_ip
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(held) = per_ip.get_mut(&self.ip) {
            *held -= 1;
            if *held == 0 {
                per_ip.remove(&self.ip);
            }
        }
    }
}

/// The downstream h2 server's settings. One window for every stream, and a connection window
/// of [`CONNECTION_STREAMS`] of them, so held bodies cannot use up a connection's credit.
fn http_builder(core: &Core) -> http2::Builder<TokioExecutor> {
    let mut http = http2::Builder::new(TokioExecutor::new());
    http.initial_stream_window_size(STREAM_WINDOW_BYTES)
        .initial_connection_window_size(STREAM_WINDOW_BYTES.saturating_mul(CONNECTION_STREAMS))
        .adaptive_window(false)
        .max_concurrent_streams(CONNECTION_STREAMS)
        .max_header_list_size(core.limits.max_header_list_bytes);
    http
}

/// Accepts connections on `listener` until `shutdown` turns true (or its sender is gone),
/// then sends each connection a graceful GOAWAY, the only GOAWAY the proxy sends on purpose,
/// and gives open streams [`SHUTDOWN_DRAIN`] to finish.
pub(crate) async fn serve<U: UpdateHandler + 'static>(
    core: Arc<Core>,
    updates: U,
    mut listener: impl Listen,
    mut shutdown: watch::Receiver<bool>,
) {
    let http = http_builder(&core);
    let limits = ConnLimits::new(
        core.limits.max_connections,
        core.limits.max_connections_per_ip,
    );
    let service = Service::new(core, updates);
    let graceful = GracefulShutdown::new();
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                // An accept that fails (a connection reset before it was taken) is skipped, and
                // so is a connection over a limit: it is dropped, which closes it.
                if let Ok((io, peer)) = accepted
                    && let Some(permit) = limits.admit(peer)
                {
                    let service = Arc::clone(&service);
                    let connection = http.serve_connection(
                        TokioIo::new(io),
                        service_fn(move |request| Arc::clone(&service).call(request)),
                    );
                    let watched = graceful.watch(connection);
                    tokio::spawn(async move {
                        let _permit = permit;
                        let _ = watched.await;
                    });
                }
            }
            _ = shutdown.wait_for(|stop| *stop) => break,
        }
    }
    drop(listener);
    let _ = tokio::time::timeout(SHUTDOWN_DRAIN, graceful.shutdown()).await;
}
