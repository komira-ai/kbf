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
//! - CreateMultipartUpload, UploadPart, CompleteMultipartUpload and AbortMultipartUpload, which
//!   the backend does (the [`Proxy`](crate::Proxy) passes them to its cold tier). The server
//!   holds no part: the body of an UploadPart is handed to the backend as a stream, piece by
//!   piece as it arrives, so a part of any size is stored without being held in memory, and
//!   the completion joins the parts into the object in the backend, create-only. A part
//!   whose body is refused (a checksum that does not match, a body over the limit, a stalled
//!   one) is not stored. An upload is the backend's: it lives as long as the backend keeps it,
//!   and `NoSuchUpload` is the answer for an id the backend does not hold. The ETag of a part
//!   is the content tag the backend gives it, and that of the completion is the content tag of
//!   the object;
//! - ListMultipartUploads (`GET /<bucket>?uploads`) with `prefix`, `max-uploads`, `key-marker`
//!   and `upload-id-marker`, without `delimiter`: the open uploads of the backend (for the
//!   [`Proxy`](crate::Proxy), those of its cold tier), by key and then by id, paged by the two
//!   markers, so that a janitor can find the uploads a job left open and abort them. Uploads of
//!   a key under `_journal/` or `_tiering/` are not listed. ListParts (`GET /<key>?uploadId=`) is
//!   `NotImplemented`;
//! - CreateBucket and HeadBucket of the served bucket (always successful), for setup tools.
//!
//! Every other call, a copy (`x-amz-copy-source`), and a query parameter this front does not
//! know (`delimiter`, `versionId`), is answered `NotImplemented`. A request with a key under
//! `_journal/` or `_tiering/` is refused with `AccessDenied`.
//!
//! A put, a delete or the completion of an upload is carried to its end on a task of its own
//! when the client goes away before the answer, so that no object is left without the journal
//! entry that has it copied. (A put whose body the client had not finished sending is not
//! carried on: its body is cut, and nothing is stored.)
//!
//! # Streamed puts
//!
//! The body of a put that only creates is not held. It is read a piece at a time, checked
//! against its hashes as the pieces pass, and handed through a queue of a few pieces to the
//! backend's streamed put ([`Backend::put_stream`](crate::Backend::put_stream)), so that the
//! memory of a put does not grow with the length of the object, and a slow backend slows the
//! client down instead of filling memory. The object becomes visible, and its journal entry is
//! written, only after the last piece was read and every hash came out right: a hash that does
//! not match at the last byte, a body over the limit, a stalled body and a connection cut half
//! way each leave no object and no journal entry. A put with `If-Match`, or an `If-None-Match`
//! other than `*`, is refused without being stored. [`ServerConfig::max_request_bytes`] is
//! the most bytes a body may have, not a bound on memory.
//!
//! Before the first byte of a streamed body (a put, or a part of an upload) is read, the request
//! is refused if it gives no `Content-Length` and the backend needs the length to stream
//! ([`Backend::needs_length`](crate::Backend::needs_length): an S3 tier does, and would hold the
//! body whole otherwise) with 411 `MissingContentLength`; if its `Content-Length` is over
//! [`ServerConfig::max_request_bytes`], with 413 `EntityTooLarge`, so that nothing is stored and
//! no upload is begun; and if [`ServerConfig::max_concurrent_puts`] bodies are already being
//! taken, with 503 `SlowDown` and `Retry-After`. A body that then comes in at an average rate
//! under [`ServerConfig::put_min_bytes_per_sec`], once [`ServerConfig::put_rate_grace`] has
//! passed, is cut with 400 `RequestTimeout`, which ends the stream with an error, so nothing is
//! stored and an upload at an S3 tier is aborted.
//!
//! Create-only on an S3 tier needs a store that honours conditional writes
//! (`If-None-Match: *` on a put and on the completion of a multipart upload, answering 412 for a
//! key that exists): on one that ignores them a put would replace.
//!
//! Requests are signed with SigV4 in the `Authorization` header (see `auth`) by one of the access
//! keys of [`ServerConfig`], over `host`, `x-amz-content-sha256` and `x-amz-date` at least, or are
//! presigned PUT URLs of a presigning key (see [`presign`]). A request without a good signature is
//! answered 403 before its body is read. A request names its key by the key id of its credential, and an id
//! that is no key's is `InvalidAccessKeyId`.
//!
//! # Access keys
//!
//! The server has one full key, which may do everything above, and any number of read-only
//! keys ([`ServerConfig::read_only_keys`], `--read-only-key-file` in the binary), which may do
//! GetObject, HeadObject, ListObjectsV2, ListMultipartUploads and HeadBucket and nothing else.
//! Any other call signed by a read-only key (a put, a delete, CreateBucket, and every call of a
//! multipart upload but the listing, which reads) is
//! answered 403 `AccessDenied` with the S3 error document before its body is read, and changes
//! nothing. The roles of those keys are of the key and not of a prefix. A put by the full key
//! is create-only but, when the hot tier is full, can race another put of the same key into the
//! cold tier (see [`Proxy`](crate::Proxy)); a presigned PUT never goes to the cold tier of a
//! proxy, and on a cache with `write_through` it goes to the upstream like every other put.
//! A credential that may write one object under a prefix, and expires, is a presigned PUT URL: see [`presign`], and
//! [`ServerConfig::presign_keys`] for the keys that sign them (`--presign-key-file` in the binary).
//! A key that lists, heads and deletes under one prefix, and does nothing else (no get, no put), is a
//! GC key: see [`gc`], and [`ServerConfig::gc_keys`] (`--gc-key-file` in the binary). Its deletes are
//! not create-only: they are the only way an object leaves a prefix that presigned PUTs write.
//! A key that reads, creates and deletes under the prefixes of three lists in its key file, and replaces and
//! copies nothing, is a scoped key: see [`scoped`], and [`ServerConfig::scoped_keys`] (`--scoped-key-file` in the
//! binary).
//!
//! # Checksums
//!
//! The body of a request is checked while it is read, against every hash the request carries,
//! and a body that does not match is refused with 400 and nothing is stored (a put stores
//! nothing; a part is not kept):
//!
//! - the hash the signature covers (`x-amz-content-sha256`, as hex): `XAmzContentSHA256Mismatch`
//!   for a body that is not that;
//! - `Content-MD5`, `x-amz-checksum-crc32`, `x-amz-checksum-crc32c`, `x-amz-checksum-sha1` and
//!   `x-amz-checksum-sha256`, each the base64 of the hash (the CRCs as four big-endian bytes):
//!   `BadDigest` for a body that does not match, `InvalidDigest` for a value that is not the
//!   base64 of a hash of the right length;
//! - `x-amz-checksum-crc64nvme` is answered `NotImplemented` (the crate depends on no CRC-64
//!   implementation), and so is a trailing checksum or a signed stream (`STREAMING-` payloads,
//!   `aws-chunked`), which the clients of kbf do not use.
//!
//! The `x-amz-checksum-*` headers are read on the body of a put of an object or of a part, which
//! is the data they are the checksums of. On any other request they are not: those of a
//! CompleteMultipartUpload are the checksums of the whole object it makes, not of its XML body,
//! so they are ignored there. `Content-MD5` and the signed hash are held to every body.
//!
//! A body with no checksum header and the signed hash `UNSIGNED-PAYLOAD` is accepted unchecked.
//!
//! # Idle bodies
//!
//! Every wait for a piece of a request body, and every write of an answer, is bounded by
//! [`ServerConfig::body_idle_timeout`]; the head of a request has
//! [`ServerConfig::header_timeout`]. The bound is on the time with no progress, not the time of
//! the whole: a body that is slow and steady is not cut.
//! If no byte of a request body arrives for that long, the request is answered 400
//! `RequestTimeout` with `Connection: close` and a line is logged, and nothing is stored. If the
//! client takes no byte of an answer for that long (its receive buffer stays full), the
//! connection is closed and a line is logged. A drain therefore waits for a stalled client no
//! longer than that timeout.
//!
//! `GET /healthz` is 200 while the process runs. `GET /readyz` is 200 until the drain begins and
//! 503 after. Neither is signed, so a bucket cannot have either name.
//!
//! `GET /usagez`, for alerting and not for a load balancer, is 200 with the age of the usage
//! object that the reconciler publishes (`_tiering/usage`), and 503 with the reason once that
//! object is older than [`ServerConfig::usage_stale_after`]. The puts ignore a stale usage, as if
//! the hot tier were not full, so a stopped reconciler would otherwise show nowhere. A usage
//! object that is missing is judged by the time the server has run, so that a server that starts
//! before the first pass of the reconciler is 200 for that long. `/readyz` does not look at the
//! usage (one stopped reconciler would make every node unready), and the puts are not refused.
//! Without [`ServerConfig::proxy`] it is 200 and says the usage is not watched. A bucket cannot
//! be called `usagez` either.
//!
//! `GET /metricz` is 200 with the counters of every source the process has, one block after the
//! other, in the Prometheus text format: the proxy's reads ([`Proxy::metrics`]: hits, misses,
//! errors and bytes of each tier, the copies cold reads made into the hot tier), the OCI read
//! front ([`oci`]) and the local cache ([`local_cache`]). A line says so when the process has
//! none of them. It is not signed either, and a bucket cannot be called `metricz`. A cache node
//! and a storage node are different processes; the README of the crate lists every name and
//! label. A server that is the front of a local cache is a read cache: it answers GET and HEAD
//! and refuses every other method, copies and multipart calls included, with 403
//! `AccessDenied`, before the role of the key is looked at. With [`ServerConfig::cache_writes`]
//! it takes the writes its keys may do (see [`local_cache`]): the roles apply as on any front.
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
mod cache_cli;
pub mod cli;
mod config;
mod config_cache;
mod config_print;
mod front;
pub mod gc;
mod handover;
mod idle;
mod keys;
mod launch;
pub mod local_cache;
pub mod oci;
mod ops;
pub mod presign;
mod reply;
mod request;
pub mod scoped;
mod time;
mod xml;

#[cfg(test)]
mod auth_tests;
#[cfg(test)]
mod body_stall_tests;
#[cfg(test)]
mod cache_cli_tests;
#[cfg(test)]
mod completion_tests;
#[cfg(test)]
mod config_cache_tests;
#[cfg(test)]
mod config_testing;
#[cfg(test)]
mod docs_tests;
#[cfg(test)]
mod drain_tests;
#[cfg(test)]
mod gc_server_tests;
#[cfg(test)]
mod gc_tests;
#[cfg(test)]
mod idle_tests;
#[cfg(test)]
mod metrics_race_tests;
#[cfg(test)]
mod metrics_tests;
#[cfg(test)]
mod ops_tests;
#[cfg(test)]
mod pipes;
#[cfg(test)]
mod presign_server_tests;
#[cfg(test)]
mod ready_tests;
#[cfg(test)]
mod reply_tests;
#[cfg(test)]
mod request_tests;
#[cfg(test)]
mod role_tests;
#[cfg(test)]
mod scoped_server_tests;
#[cfg(test)]
mod scoped_tests;
#[cfg(test)]
mod server_tests;
#[cfg(test)]
mod slow_reader_tests;
#[cfg(test)]
mod stream_put_tests;
#[cfg(test)]
mod testing;
#[cfg(test)]
mod time_tests;
#[cfg(test)]
mod uploads_tests;
#[cfg(test)]
mod xml_tests;

use std::convert::Infallible;
use std::future::poll_fn;
use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicU64, AtomicUsize, Ordering};
use std::task::{Context, Poll};
use std::time::Duration;

use bytes::Bytes;
use http_body_util::Full;
use hyper::header::{HeaderName, HeaderValue};
use hyper::http::request::Parts;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::{TokioIo, TokioTimer};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpListener;
use tokio::sync::{Notify, Semaphore, watch};

use crate::{Backend, Clock, MonotonicClock, Proxy};

pub use gc::GcKey;
pub use keys::AccessKey;
use keys::Role;
pub use local_cache::LocalCache;
pub use presign::{Grant, PresignKey, PresignRequest, presign_put};
pub use scoped::{Prefixes, ScopedKey};

use idle::Watched;
use reply::{Answer, S3Error, answer};
use request::{BoxError, Check, Query, Reader, Target, resolve};

/// The settings of a [`Server`].
#[derive(Clone)]
pub struct ServerConfig {
    /// The one bucket the server serves.
    pub bucket: String,
    /// The region in the credential scope of a signature.
    pub region: String,
    /// The id of the full key, which may do everything the server serves.
    pub access_key_id: String,
    /// The secret of the full key.
    pub secret_access_key: String,
    /// The read-only keys, which may read objects and list the bucket and do nothing else (see
    /// the module docs). None by default. Their ids differ from each other's and from the full
    /// key's: if one is the full key's, the full key is the one that is meant.
    pub read_only_keys: Vec<AccessKey>,
    /// The keys that sign presigned PUT URLs (see [`presign`]); a URL is accepted only if one of
    /// them signed it. None by default. Their ids differ from each other's and from the other keys'.
    pub presign_keys: Vec<PresignKey>,
    /// The GC keys (see [`gc`]): header-signed keys that list, head and delete under one prefix.
    /// None by default. Their ids differ from each other's and from the other keys'.
    pub gc_keys: Vec<GcKey>,
    /// The scoped keys (see [`scoped`]): header-signed keys that read, create and delete under
    /// the prefixes of three lists. None by default. Their ids differ from each other's and from
    /// the other keys'.
    pub scoped_keys: Vec<ScopedKey>,
    /// The longest `X-Amz-Expires` a presigned URL may have, whatever the ceiling of its key says.
    pub presign_max_expires: Duration,
    /// The most bytes of a request body: the longest object that one put takes, and the
    /// longest part of an upload. It is an upper bound on a length, not on memory: the body of
    /// a put or of a part is streamed to the backend and is never held whole. (The body of a
    /// request that is not streamed, the XML of a completion, is held, up to this many bytes.)
    pub max_request_bytes: usize,
    /// How long a request body may go without a byte arriving, and how long the client may
    /// go without taking a byte of an answer, before the request ends: a body that arrives
    /// slowly but steadily is not cut, and neither is a slow reader that keeps taking bytes.
    /// See the module docs.
    pub body_idle_timeout: Duration,
    /// The most streamed bodies (puts and parts of uploads) taken at once. A request over the
    /// limit is answered 503 `SlowDown` with a `Retry-After` and its body is not read. Each
    /// streamed body holds a few pieces (`PIECES_QUEUED`, of the size the connection delivers),
    /// and an S3 tier up to 16 MiB more, so the memory of the puts is bounded by this number
    /// times that.
    pub max_concurrent_puts: usize,
    /// The least average rate, in bytes a second, at which a request body is taken, counted
    /// over the time the server spends waiting for the client to send the next piece once
    /// that time has passed `put_rate_grace`; a body that goes slower is refused with 400
    /// `RequestTimeout` and nothing of it is stored (an upload in progress at the store is
    /// aborted). Time the server spends not waiting for the client does not count, so a tier
    /// that is slow to take the body, or to answer before it takes it, is not blamed on the
    /// client. Zero turns the check off. It catches a sender that trickles a byte inside every
    /// idle timeout.
    pub put_min_bytes_per_sec: u64,
    /// How much time spent waiting for the client a body is let to go before its rate is held
    /// to `put_min_bytes_per_sec`.
    pub put_rate_grace: Duration,
    /// How long a client has to send the head of a request. It also bounds how long a
    /// connection may wait, idle, for its next request, so keep it above the time a client
    /// keeps an idle connection in its pool.
    pub header_timeout: Duration,
    /// How long the server goes on serving after the drain began and `/readyz` said 503.
    pub drain_lb_wait: Duration,
    /// How long the server waits for requests in flight, and for the answers it is sending, after
    /// it began refusing new requests.
    pub drain_deadline: Duration,
    /// The proxy whose usage object `/usagez` judges: none by default, and then `/usagez` says
    /// the usage is not watched. The binary sets the proxy it serves.
    pub proxy: Option<Arc<Proxy>>,
    /// The local cache the server is the front of, if it is one: none by default. With one the
    /// server is a read cache: every call but GET and HEAD is refused 403 `AccessDenied` before
    /// its body is read, and `/metricz` shows the cache's counters. The binary sets the cache it
    /// serves, which is also the server's backend.
    pub cache: Option<LocalCache>,
    /// Whether a server that is the front of a local cache takes writes: false by default, and
    /// then the refusal of `cache` stands, before the role of the key is looked at. With it the
    /// roles of the keys decide, as on a storage node. It does nothing without `cache`. The
    /// binary sets it from `write_through` in `[local_cache]`, the setting that also lets the
    /// cache send the writes on.
    pub cache_writes: bool,
    /// The age of the usage object past which `/usagez` says 503. A server that has run this
    /// long without finding any usage object is judged the same way. Longer than the lease and
    /// the pass interval of the reconciler, so that a takeover of the lease does not show.
    pub usage_stale_after: Duration,
    /// The counters of the OCI read front, which `GET /metricz` shows; none by default, and then
    /// `/metricz` has no block of it. The binary sets them when it runs the front.
    pub oci_metrics: Option<Arc<oci::OciMetrics>>,
    /// The time, for the age of a signature and of the usage object.
    pub clock: Arc<dyn Clock>,
    /// The time, for the rate of a request body (`put_min_bytes_per_sec`). It is a clock that
    /// only goes forward ([`MonotonicClock`](crate::MonotonicClock) from [`ServerConfig::new`]),
    /// not `clock`, so that a step of the clock of the machine (the time service setting it)
    /// while the server waits for a piece does not count for or against the client.
    pub rate_clock: Arc<dyn Clock>,
}

impl ServerConfig {
    /// The settings for `bucket` and a full access key, in region `us-east-1`, with bodies of at
    /// most 256 MiB, 30 seconds of idle time for a body or an answer, a minute for a request
    /// head (and for an idle connection to send its next request), a drain that waits 10
    /// seconds with `/readyz` at 503 and then at most 15 minutes for the requests in flight.
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
            read_only_keys: Vec::new(),
            presign_keys: Vec::new(),
            gc_keys: Vec::new(),
            scoped_keys: Vec::new(),
            presign_max_expires: Duration::from_secs(900),
            max_request_bytes: 256 << 20,
            body_idle_timeout: Duration::from_secs(30),
            max_concurrent_puts: 32,
            put_min_bytes_per_sec: 1024,
            put_rate_grace: Duration::from_secs(30),
            header_timeout: Duration::from_secs(60),
            drain_lb_wait: Duration::from_secs(10),
            drain_deadline: Duration::from_secs(900),
            proxy: None,
            cache: None,
            cache_writes: false,
            usage_stale_after: Duration::from_secs(120),
            oci_metrics: None,
            clock,
            rate_clock: Arc::new(MonotonicClock::new()),
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

/// Where [`Server::serve`] takes its connections from: the listener of a socket, and in the
/// tests a channel of pipes, which a test runs on a paused clock.
trait Incoming {
    /// The byte stream of a connection.
    type Stream: AsyncRead + AsyncWrite + Unpin + Send + 'static;

    /// Takes the next connection, once there is one, with the address of its peer.
    fn poll_incoming(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<io::Result<(Self::Stream, SocketAddr)>>;
}

impl Incoming for TcpListener {
    type Stream = tokio::net::TcpStream;

    fn poll_incoming(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<io::Result<(tokio::net::TcpStream, SocketAddr)>> {
        self.poll_accept(cx)
    }
}

/// Waits a moment when an accept failed, so that a listener that keeps failing is not asked
/// again at once.
async fn pause_after(failed: bool) {
    if failed {
        tokio::time::sleep(ACCEPT_PAUSE).await;
    }
}

/// The S3 front of a [`Backend`].
pub struct Server {
    backend: Arc<dyn Backend>,
    config: ServerConfig,
    /// When the server was made, by the clock of the config.
    started: u64,
    phase: AtomicU8,
    /// Notified once, when the drain begins.
    drain_requested: Notify,
    /// The requests that have begun and not ended.
    inflight: AtomicUsize,
    /// The connections that are open.
    open: AtomicUsize,
    /// Notified whenever a request begins or ends or a connection opens or closes.
    idle: Notify,
    /// Set once, when the drain has waited for the requests in flight: the connections finish
    /// the answers they are sending and close.
    closing: watch::Sender<bool>,
    next_request: AtomicU64,
    /// One permit for each streamed body that may be taken at once.
    puts: Arc<Semaphore>,
}

/// A request in flight, counted until it is dropped.
struct Flight<'a>(&'a Server);

impl<'a> Flight<'a> {
    fn enter(server: &'a Server) -> Self {
        server.inflight.fetch_add(1, Ordering::SeqCst);
        server.idle.notify_waiters();
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
        server.idle.notify_waiters();
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
        let puts = Arc::new(Semaphore::new(config.max_concurrent_puts));
        Arc::new(Self {
            puts,
            backend,
            started: config.clock.now(),
            config,
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

    /// The streamed bodies that may still be taken at once.
    #[cfg(test)]
    pub(super) fn put_permits_free(&self) -> usize {
        self.puts.available_permits()
    }

    /// The connections that are open now, those that wait for a request included.
    #[must_use]
    pub fn open_connections(&self) -> usize {
        self.open.load(Ordering::SeqCst)
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
        self.serve_from(listener).await
    }

    /// [`Server::serve`] over any source of connections: the tests serve pipes.
    async fn serve_from<I>(self: &Arc<Self>, mut incoming: I) -> Drained
    where
        I: Incoming + Send,
    {
        loop {
            tokio::select! {
                () = self.accept_one(&mut incoming) => {}
                () = self.drain_requested.notified() => break,
            }
        }
        let wait = tokio::time::sleep(self.config.drain_lb_wait);
        tokio::pin!(wait);
        loop {
            tokio::select! {
                () = self.accept_one(&mut incoming) => {}
                () = &mut wait => break,
            }
        }
        self.phase.store(REFUSING, Ordering::SeqCst);
        drop(incoming);
        self.finish_by_deadline().await
    }

    /// Takes the next connection `incoming` gives and serves it; after an accept that failed,
    /// waits a moment.
    async fn accept_one<I: Incoming>(self: &Arc<Self>, incoming: &mut I) {
        let accepted = poll_fn(|cx| incoming.poll_incoming(cx)).await;
        let failed = accepted
            .map(|(stream, peer)| self.connection(stream, peer))
            .is_err();
        pause_after(failed).await;
    }

    /// Waits as [`Server::finish`] does, for [`ServerConfig::drain_deadline`] at most, and says
    /// how many requests that deadline cut.
    async fn finish_by_deadline(&self) -> Drained {
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
    fn connection<S>(self: &Arc<Self>, stream: S, peer: SocketAddr)
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let server = Arc::clone(self);
        let service = service_fn(move |request| {
            let server = Arc::clone(&server);
            async move { Ok::<_, Infallible>(server.handle(request).await) }
        });
        let watched = Watched::new(stream, self.config.body_idle_timeout, peer);
        let serving = http1::Builder::new()
            .timer(TokioTimer::new())
            .header_read_timeout(self.config.header_timeout)
            .serve_connection(TokioIo::new(watched), service);
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
        B: hyper::body::Body + Send,
        B::Data: Send,
        B::Error: Into<BoxError>,
    {
        let _flight = Flight::enter(self);
        let number = self.next_request.fetch_add(1, Ordering::SeqCst);
        let id = format!("{number:016X}");
        let (parts, body) = request.into_parts();
        let outcome = self.respond(&parts, body).await;
        let path = parts.uri.path();
        let mut response = outcome.unwrap_or_else(|error| {
            if error.is_timeout() {
                self.log_stalled(&parts, &id, error.message());
            }
            error.into_answer(path, &id)
        });
        let name = HeaderName::from_static("x-amz-request-id");
        response
            .headers_mut()
            .insert(name, HeaderValue::from_str(&id).expect("hex digits"));
        response
    }

    /// Logs a request that was ended because its body stalled.
    fn log_stalled(&self, parts: &Parts, id: &str, why: &str) {
        let (method, path) = (&parts.method, parts.uri.path());
        eprintln!("tierstore: request {id} ({method} {path}): {why} Closed.");
    }

    async fn respond<B>(&self, parts: &Parts, body: B) -> Result<Answer, S3Error>
    where
        B: hyper::body::Body + Send,
        B::Data: Send,
        B::Error: Into<BoxError>,
    {
        if let Some(answer) = self.probe(parts).await {
            return Ok(answer);
        }
        if self.phase() == REFUSING {
            return Err(S3Error::draining());
        }
        let signed = auth::verify(&self.config, parts)?;
        let reads = matches!(parts.method, Method::GET | Method::HEAD);
        if self.config.cache.is_some() && !self.config.cache_writes && !reads {
            return Err(S3Error::read_cache());
        }
        signed.role.permit(&parts.method)?;
        let target = resolve(&self.config.bucket, parts.uri.path())?;
        let query = Query::parse(parts.uri.query())?;
        if let Some(grant) = &signed.presign {
            grant.permit(parts, &target, &query)?;
        }
        // After the presigning grant, which refuses a listing: a presigned request may only PUT.
        if parts.method == Method::GET && query.has("uploads") && target == Target::Bucket {
            signed
                .role
                .permit_list_uploads(query.get("prefix").unwrap_or_default())?;
        }
        let data = matches!((&parts.method, &target), (&Method::PUT, Target::Object(_)));
        let check = Check::new(&signed.payload, &parts.headers, data)?;
        if let Role::Gc(prefix) = &signed.role {
            gc::permit(prefix, parts, &target, &query)?;
        }
        if let Role::Scoped(scopes) = &signed.role {
            scoped::permit(scopes, parts, &target, &query)?;
        }
        let limit = self.config.max_request_bytes;
        let limit = signed.presign.as_ref().map_or(limit, |grant| {
            limit.min(usize::try_from(grant.max_object_bytes).unwrap_or(usize::MAX))
        });
        let config = &self.config;
        let floor = (config.put_min_bytes_per_sec, config.put_rate_grace);
        let reader = Reader::new(body, limit, config.body_idle_timeout).with_floor(
            floor.0,
            floor.1,
            Arc::clone(&config.rate_clock),
        );
        let grant = signed.presign.as_ref();
        self.dispatch(parts, target, &query, reader, check, grant)
            .await
    }

    /// The answer to a health or readiness probe, which are not signed.
    async fn probe(&self, parts: &Parts) -> Option<Answer> {
        let read = matches!(parts.method, Method::GET | Method::HEAD);
        let (status, text) = match (read, parts.uri.path(), self.is_ready()) {
            (true, "/healthz", _) => (StatusCode::OK, "ok".to_owned()),
            (true, "/readyz", true) => (StatusCode::OK, "ready".to_owned()),
            (true, "/readyz", false) => (StatusCode::SERVICE_UNAVAILABLE, "draining".to_owned()),
            (true, "/usagez", _) => self.usage_probe().await,
            (true, "/metricz", _) => (StatusCode::OK, self.metrics_probe()),
            _ => return None,
        };
        let headers = [("content-type", "text/plain".to_owned())];
        Some(answer(status, &headers, Bytes::from(text)))
    }

    /// The text of `/metricz`: the block of every source of counters the process has (the
    /// proxy's, the OCI front's, the local cache's), one after the other, each ending in a
    /// newline, or one line that names them all when there is none. Every block that exists
    /// renders, never one of them; a storage node has the proxy's, a cache node the cache's, and
    /// either may have the OCI front's.
    fn metrics_probe(&self) -> String {
        let blocks = [
            self.config.proxy.as_ref().map(|proxy| proxy.metrics()),
            self.config.oci_metrics.as_ref().map(|oci| oci.render()),
            self.config.cache.as_ref().map(|cache| cache.metrics()),
        ];
        let mut text = String::new();
        for block in blocks.into_iter().flatten() {
            text.push_str(&block);
            if !block.ends_with('\n') {
                text.push('\n');
            }
        }
        if text.is_empty() {
            text = "# this server has no proxy, no OCI read front and no local cache\n".to_owned();
        }
        text
    }

    /// The answer of `/usagez`: 200 and the age of the usage object of the reconciler while it is
    /// within the limit, 503 and the reason once it is older, or missing for longer than the
    /// limit since the server started. It does not depend on the drain.
    async fn usage_probe(&self) -> (StatusCode, String) {
        let Some(proxy) = &self.config.proxy else {
            return (StatusCode::OK, "usage not watched".to_owned());
        };
        let limit = self.config.usage_stale_after;
        let published = proxy.usage_age().await;
        let since_start = self.config.clock.now().saturating_sub(self.started);
        let age = published.unwrap_or(Duration::from_millis(since_start));
        let (secs, max) = (age.as_secs(), limit.as_secs());
        let down = StatusCode::SERVICE_UNAVAILABLE;
        match (age <= limit, published.is_some()) {
            (true, true) => (StatusCode::OK, format!("usage published {secs}s ago")),
            (true, false) => (StatusCode::OK, format!("no usage yet, {secs}s since start")),
            (false, true) => (
                down,
                format!("usage of the hot tier is stale: published {secs}s ago, limit {max}s"),
            ),
            (false, false) => (
                down,
                format!(
                    "usage of the hot tier is missing: none published in {secs}s, limit {max}s"
                ),
            ),
        }
    }
}
