//! The S3 API front of the proxy: an HTTP/1.1 server that answers the calls of S3 clients from
//! a [`Backend`], which is a [`Proxy`](crate::Proxy) in the `tierstore` binary, and the pieces
//! of that binary ([`cli`]).
//!
//! Path-style addressing of one bucket, and these calls, with the S3 error documents and status
//! codes (`NoSuchKey` 404, `PreconditionFailed` 412, `SlowDown` 503 where the backend has no
//! room, and so on):
//!
//! - GetObject (whole, `bytes=a-b`, `bytes=a-` or `bytes=-n`; the object is asked for its size
//!   first, so a ranged read costs a head) and HeadObject, with `If-Match` (another content tag
//!   is `PreconditionFailed`) and `If-None-Match` (the content tag is a 304); the conditions on
//!   the time are `NotImplemented`;
//! - PutObject, which only ever creates: `If-None-Match: *` is what a client sends, and a put
//!   without it is create-only too, because the proxy keeps objects immutable. A key that
//!   exists is `PreconditionFailed`. `If-Match` is answered `PreconditionFailed` for another
//!   content tag and `NotImplemented` for the right one (an immutable object cannot be
//!   replaced);
//! - DeleteObject, which succeeds for a key that is not there;
//! - ListObjectsV2 with `prefix`, `max-keys`, `continuation-token` and `start-after`, without
//!   `delimiter`;
//! - CreateMultipartUpload, UploadPart, CompleteMultipartUpload and AbortMultipartUpload. The
//!   parts are held in memory by the server that took them (up to
//!   [`ServerConfig::max_staged_bytes`], for [`ServerConfig::upload_ttl`]) and the completed
//!   object is written to the backend as one create-only put, so the parts of an upload go to
//!   one server, an upload does not outlive a restart, and an object larger than
//!   [`ServerConfig::max_staged_bytes`] cannot be uploaded in parts (`EntityTooLarge`);
//! - CreateBucket and HeadBucket of the served bucket (always successful), for setup tools.
//!
//! Every other call, a copy (`x-amz-copy-source`), and a query parameter this front does not
//! know (`delimiter`, `versionId`), is answered `NotImplemented`. A request with a key under
//! `_journal/` or `_tiering/` is refused with `AccessDenied`.
//!
//! A put, a delete or the completion of an upload is carried to its end on a task of its own
//! when the client goes away before the answer, so that no object is left without the journal
//! entry that has it copied.
//!
//! Requests are signed with SigV4 in the `Authorization` header (see `auth`): the one access key
//! of [`ServerConfig`], over `host`, `x-amz-content-sha256` and `x-amz-date` at least, and a
//! request without a good signature, a presigned URL included, is answered 403 before its body
//! is read. The body is checked against the hash the signature covers. Other checksums a client
//! sends (`x-amz-checksum-*`) are not checked.
//!
//! `GET /healthz` is 200 while the process runs. `GET /readyz` is 200 until the drain begins and
//! 503 after. Neither is signed, so a bucket cannot have either name.
//!
//! # Drain
//!
//! [`Server::begin_drain`], which the binary calls on SIGTERM or SIGINT, starts a sequence that
//! [`Server::serve`] runs: `/readyz` answers 503 at once; for [`ServerConfig::drain_lb_wait`]
//! the server goes on serving everything, so that a load balancer or a client that has not seen
//! the 503 yet is not refused; then it stops accepting connections and answers every new
//! request, but for the `GET` and `HEAD` probes `/healthz` and `/readyz`, 503
//! `ServiceUnavailable` with `Connection: close`; then it waits until the requests in flight
//! are finished; then it closes the connections that are open, after the answers they are still
//! sending, and waits until they are closed. Both waits together take
//! [`ServerConfig::drain_deadline`] at most. `serve` returns, with the number of requests that
//! the deadline cut: those still running, or still being sent.

mod auth;
pub mod cli;
mod ops;
mod reply;
mod request;
mod time;
mod uploads;
mod xml;

#[cfg(test)]
mod auth_tests;
#[cfg(test)]
mod ops_tests;
#[cfg(test)]
mod reply_tests;
#[cfg(test)]
mod request_tests;
#[cfg(test)]
mod server_tests;
#[cfg(test)]
mod testing;
#[cfg(test)]
mod time_tests;
#[cfg(test)]
mod uploads_tests;
#[cfg(test)]
mod xml_tests;

use std::convert::Infallible;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

use bytes::Bytes;
use http_body_util::Full;
use hyper::header::{HeaderName, HeaderValue};
use hyper::http::request::Parts;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::{TokioIo, TokioTimer};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Notify, watch};

use crate::{Backend, Clock};

use reply::{Answer, S3Error, answer};
use request::{BoxError, Query, check_payload, read_body, resolve};
use uploads::Uploads;

/// The settings of a [`Server`].
#[derive(Clone)]
pub struct ServerConfig {
    /// The one bucket the server serves.
    pub bucket: String,
    /// The region in the credential scope of a signature.
    pub region: String,
    /// The access key id requests are signed with.
    pub access_key_id: String,
    /// The secret access key requests are signed with.
    pub secret_access_key: String,
    /// The most bytes of a request body: the largest object that one put takes, and the
    /// largest part of an upload.
    pub max_request_bytes: usize,
    /// The most bytes that the parts of the unfinished multipart uploads hold.
    pub max_staged_bytes: u64,
    /// How long an upload that gets no part is kept.
    pub upload_ttl: Duration,
    /// How long a client has to send the head of a request. It also bounds how long a
    /// connection may wait, idle, for its next request, so keep it above the time a client
    /// keeps an idle connection in its pool.
    pub header_timeout: Duration,
    /// How long the server goes on serving after the drain began and `/readyz` said 503.
    pub drain_lb_wait: Duration,
    /// How long the server waits for requests in flight, and for the answers it is sending, after
    /// it began refusing new requests.
    pub drain_deadline: Duration,
    /// The time, for the age of a signature.
    pub clock: Arc<dyn Clock>,
}

impl ServerConfig {
    /// The settings for `bucket` and an access key, in region `us-east-1`, with bodies of at
    /// most 256 MiB, 1 GiB of unfinished uploads kept for an hour, a minute for a request head
    /// (and for an idle connection to send its next request), a drain that waits 10 seconds
    /// with `/readyz` at 503 and then at most 15 minutes for the requests in flight.
    #[must_use]
    pub fn new(
        bucket: &str,
        access_key_id: &str,
        secret_access_key: &str,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            bucket: bucket.to_owned(),
            region: "us-east-1".to_owned(),
            access_key_id: access_key_id.to_owned(),
            secret_access_key: secret_access_key.to_owned(),
            max_request_bytes: 256 << 20,
            max_staged_bytes: 1 << 30,
            upload_ttl: Duration::from_secs(3600),
            header_timeout: Duration::from_secs(60),
            drain_lb_wait: Duration::from_secs(10),
            drain_deadline: Duration::from_secs(900),
            clock,
        }
    }
}

/// How a drain ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Drained {
    /// The requests that were still running when the drain deadline passed: zero for a drain
    /// that waited for every one.
    pub abandoned: usize,
}

/// The server is serving.
const SERVING: u8 = 0;
/// The drain began: `/readyz` says 503.
const NOT_READY: u8 = 1;
/// The server refuses new requests.
const REFUSING: u8 = 2;

/// How long [`Server::serve`] waits after a failed accept before the next.
const ACCEPT_PAUSE: Duration = Duration::from_millis(100);

/// The S3 front of a [`Backend`].
pub struct Server {
    backend: Arc<dyn Backend>,
    config: ServerConfig,
    uploads: Uploads,
    phase: AtomicU8,
    /// Notified once, when the drain begins.
    drain_requested: Notify,
    /// The requests that have begun and not ended.
    inflight: AtomicUsize,
    /// The connections that are open.
    open: AtomicUsize,
    /// Notified whenever a request ends or a connection closes.
    idle: Notify,
    /// Set once, when the drain has waited for the requests in flight: the connections finish
    /// the answers they are sending and close.
    closing: watch::Sender<bool>,
    next_request: AtomicU64,
}

/// A request in flight, counted until it is dropped.
struct Flight<'a>(&'a Server);

impl<'a> Flight<'a> {
    fn enter(server: &'a Server) -> Self {
        server.inflight.fetch_add(1, Ordering::SeqCst);
        Self(server)
    }
}

impl Drop for Flight<'_> {
    fn drop(&mut self) {
        self.0.inflight.fetch_sub(1, Ordering::SeqCst);
        self.0.idle.notify_waiters();
    }
}

/// A connection that is open, counted until it is dropped.
struct Connected(Arc<Server>);

impl Connected {
    fn enter(server: &Arc<Server>) -> Self {
        server.open.fetch_add(1, Ordering::SeqCst);
        Self(Arc::clone(server))
    }
}

impl Drop for Connected {
    fn drop(&mut self) {
        self.0.open.fetch_sub(1, Ordering::SeqCst);
        self.0.idle.notify_waiters();
    }
}

impl Server {
    /// A server over `backend`.
    #[must_use]
    pub fn new(backend: Arc<dyn Backend>, config: ServerConfig) -> Arc<Self> {
        let epoch = config.clock.now();
        Arc::new(Self {
            backend,
            config,
            uploads: Uploads::new(epoch),
            phase: AtomicU8::new(SERVING),
            drain_requested: Notify::new(),
            inflight: AtomicUsize::new(0),
            open: AtomicUsize::new(0),
            idle: Notify::new(),
            closing: watch::channel(false).0,
            next_request: AtomicU64::new(1),
        })
    }

    fn phase(&self) -> u8 {
        self.phase.load(Ordering::SeqCst)
    }

    /// Whether the server is serving and `/readyz` says 200: the drain has not begun.
    #[must_use]
    pub fn is_ready(&self) -> bool {
        self.phase() == SERVING
    }

    /// The requests being answered now.
    #[must_use]
    pub fn in_flight(&self) -> usize {
        self.inflight.load(Ordering::SeqCst)
    }

    /// Begins the drain: `/readyz` says 503 from now on, and [`Server::serve`] runs the rest of
    /// the sequence. Calling it again does nothing more.
    pub fn begin_drain(&self) {
        let _ = self
            .phase
            .compare_exchange(SERVING, NOT_READY, Ordering::SeqCst, Ordering::SeqCst);
        self.drain_requested.notify_one();
    }

    /// Serves the connections of `listener` until the drain has run its course (see the module
    /// docs): the drain begins when [`Server::begin_drain`] is called.
    pub async fn serve(self: &Arc<Self>, listener: TcpListener) -> Drained {
        loop {
            tokio::select! {
                accepted = listener.accept() => match accepted {
                    Ok((stream, _)) => self.connection(stream),
                    Err(_) => tokio::time::sleep(ACCEPT_PAUSE).await,
                },
                () = self.drain_requested.notified() => break,
            }
        }
        let wait = tokio::time::sleep(self.config.drain_lb_wait);
        tokio::pin!(wait);
        loop {
            tokio::select! {
                accepted = listener.accept() => match accepted {
                    Ok((stream, _)) => self.connection(stream),
                    Err(_) => tokio::time::sleep(ACCEPT_PAUSE).await,
                },
                () = &mut wait => break,
            }
        }
        self.phase.store(REFUSING, Ordering::SeqCst);
        drop(listener);
        let finished = tokio::time::timeout(self.config.drain_deadline, self.finish());
        match finished.await {
            Ok(()) => Drained { abandoned: 0 },
            Err(_) => Drained {
                abandoned: self.in_flight().max(self.unsent()),
            },
        }
    }

    /// Waits until no request is in flight, then has the connections finish the answers they
    /// are sending and close, and waits until they have. A process that ends before that cuts
    /// the answers that were not sent in full.
    async fn finish(&self) {
        self.wait_idle().await;
        let _ = self.closing.send_replace(true);
        self.wait_for_zero(&self.open).await;
    }

    /// The connections that were told to close and have not: each is still sending an answer.
    fn unsent(&self) -> usize {
        let closing = usize::from(*self.closing.borrow());
        closing * self.open.load(Ordering::SeqCst)
    }

    /// Resolves when no request is in flight.
    async fn wait_idle(&self) {
        self.wait_for_zero(&self.inflight).await;
    }

    /// Resolves when `count`, one of the counts of this server, is zero.
    async fn wait_for_zero(&self, count: &AtomicUsize) {
        loop {
            let ended = self.idle.notified();
            tokio::pin!(ended);
            let _ = ended.as_mut().enable();
            if count.load(Ordering::SeqCst) == 0 {
                return;
            }
            ended.await;
        }
    }

    /// Serves one connection on a task of its own, which ends with the connection, or after the
    /// drain has closed it.
    fn connection(self: &Arc<Self>, stream: TcpStream) {
        let server = Arc::clone(self);
        let service = service_fn(move |request| {
            let server = Arc::clone(&server);
            async move { Ok::<_, Infallible>(server.handle(request).await) }
        });
        let serving = http1::Builder::new()
            .timer(TokioTimer::new())
            .header_read_timeout(self.config.header_timeout)
            .serve_connection(TokioIo::new(stream), service);
        let connected = Connected::enter(self);
        let mut closing = self.closing.subscribe();
        drop(tokio::spawn(async move {
            let _connected = connected;
            tokio::pin!(serving);
            tokio::select! {
                // A connection that ends badly has nothing to tell the other side.
                _ = serving.as_mut() => return,
                _ = closing.changed() => {}
            }
            // The drain waits for this connection: the answer that is being sent is sent in
            // full, and then the connection closes.
            serving.as_mut().graceful_shutdown();
            let _ = serving.await;
        }));
    }

    /// Answers one request.
    pub async fn handle<B>(&self, request: Request<B>) -> Response<Full<Bytes>>
    where
        B: hyper::body::Body,
        B::Error: Into<BoxError>,
    {
        let _flight = Flight::enter(self);
        let number = self.next_request.fetch_add(1, Ordering::SeqCst);
        let id = format!("{number:016X}");
        let (parts, body) = request.into_parts();
        let outcome = self.respond(&parts, body).await;
        let path = parts.uri.path();
        let mut response = outcome.unwrap_or_else(|error| error.into_answer(path, &id));
        let name = HeaderName::from_static("x-amz-request-id");
        response
            .headers_mut()
            .insert(name, HeaderValue::from_str(&id).expect("hex digits"));
        response
    }

    async fn respond<B>(&self, parts: &Parts, body: B) -> Result<Answer, S3Error>
    where
        B: hyper::body::Body,
        B::Error: Into<BoxError>,
    {
        if let Some(answer) = self.probe(parts) {
            return Ok(answer);
        }
        if self.phase() == REFUSING {
            return Err(S3Error::draining());
        }
        let claim = auth::verify(&self.config, parts)?;
        let body = read_body(body, self.config.max_request_bytes).await?;
        check_payload(&claim, &body)?;
        let target = resolve(&self.config.bucket, parts.uri.path())?;
        let query = Query::parse(parts.uri.query())?;
        self.dispatch(parts, target, &query, body).await
    }

    /// The answer to a health or readiness probe, which are not signed.
    fn probe(&self, parts: &Parts) -> Option<Answer> {
        let read = matches!(parts.method, Method::GET | Method::HEAD);
        let (status, text) = match (read, parts.uri.path(), self.is_ready()) {
            (true, "/healthz", _) => (StatusCode::OK, "ok"),
            (true, "/readyz", true) => (StatusCode::OK, "ready"),
            (true, "/readyz", false) => (StatusCode::SERVICE_UNAVAILABLE, "draining"),
            _ => return None,
        };
        let headers = [("content-type", "text/plain".to_owned())];
        Some(answer(status, &headers, Bytes::from(text)))
    }
}
