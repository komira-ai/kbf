//! The job-facing read: how a task on a worker fetches the inputs of its one Action.
//!
//! A task holds a read grant ([`crate::grants`]), not a client certificate, so it cannot use
//! the REAPI listener. The job-facing listener of `kbf frontend` (`--job-listen`) answers it
//! over HTTPS, and this module is what answers: `GET /<hex>/<size>` with
//! `Authorization: Bearer <grant>`, the form `kbf-task` requests, and `POST /batch` (below),
//! and nothing else. Every other path is 404 and every other method 405.
//!
//! A request is served only when the grant verifies (a forged, garbled, expired or revoked
//! one is 401), and the digest asked for is in the grant's scope (403): the Action the grant
//! is for, its Command, and the Directory blobs and files of the Action's input tree. The
//! scope is worked out from the blobs themselves, by reading the Action and walking the tree
//! from its root, once per Action, and remembered ([`Limits::scopes`] Actions at most). A
//! grant never reads a blob outside it, so it cannot read another tenant's bytes.
//!
//! The bytes come from a [`BlobSource`], which is the frontend's [`Backend`] in a running
//! server. A blob of at most [`Limits::stream_over`] bytes (64 MiB) is read whole, from the
//! pack index first and then the storage nodes, each read checked against its digest, as a
//! ByteStream read is. A larger blob is streamed: the source's chunks are passed on as they
//! arrive and hashed on the way, so no more than a chunk or two of it is held, whatever its
//! size. The last chunk is held back until the whole blob has hashed to its digest, so a
//! source whose bytes are not the blob's ends the response early and without its last chunk
//! (the response is shorter than its length, and the client's own hash check refuses it); the
//! end is counted. A response is never larger than [`Limits::max_blob_bytes`] (413 above it,
//! before anything is read), and at most [`Limits::reads_in_flight`] places are taken at once:
//! a GET takes one, a streamed one until its response ends or is dropped, and a batch takes one
//! reservation of at most half of them for its whole read (as many as it has blobs to read, not
//! one per blob); the others wait for room.
//!
//! `POST /batch` answers many small blobs of the Action in one request, so that a task is not
//! held up by the round trips of thousands of GETs. The body is JSON, `{"digests":
//! ["<64 hex>/<size>", ...]}`, with 1 to [`BATCH_MAX_DIGESTS`] entries (a repeated one is
//! answered each time). The whole request is refused, before any blob is read, with the reason
//! codes of a GET (401, 403) or `malformed` (400: bad JSON or digest, no entries or too many)
//! or `too_large` (413: the sizes asked for add up to more than [`BATCH_MAX_BYTES`]; a blob
//! that alone is over it is not added, it is answered `too_large` below, so that the status is
//! reachable). Otherwise it is 200 `application/octet-stream` with one frame per digest, in
//! request order: one status byte (0 ok, 1 not_found, 2 out_of_scope, 3 too_large, 4
//! store_unavailable), the length as eight bytes big-endian (the blob's size when ok, else 0)
//! and the bytes. Scope is checked for each digest, the batch's read takes one reservation of
//! [`Limits::reads_in_flight`] places (at most half of them, as above), and an ok blob is hashed against its digest here: bytes that
//! are not the blob's are `store_unavailable`, never an ok frame.
//!
//! `GET /locate` answers where the blobs of the grant's Action are, as JSON lines, for the task
//! to fetch from the stores itself: see the `locate` module.
//!
//! An answer carries a reason code in its body and no other detail: `missing_token`,
//! `invalid_token`, `expired`, `revoked`, `no_scope`, `out_of_scope`, `too_large`,
//! `not_found`, `method_not_allowed` or `store_unavailable`. Requests by outcome, the
//! bytes served and the streams that ended early are counted ([`JobRead::render_metrics`]).

use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use bytes::Bytes;
use futures::stream::{self, BoxStream, StreamExt as _};
use kbf_storage::{ReadStream, StorageError};
use kbf_types::{Digest, DigestFunction};
use sha2::{Digest as _, Sha256};
use tokio::sync::{AcquireError, OnceCell, OwnedSemaphorePermit, Semaphore};
use tonic::codegen::http::{Method, StatusCode};

use crate::cache::is_empty_blob;
use crate::grants::{Claims, GrantError, Grants, Scope};
use crate::large_backfill::{LargeBackfill, Tee};
use crate::metrics::{Counter, Families};
use crate::proxy::MAX_UNTRUSTED_BLOB_BYTES;
use crate::store_metrics::Stopwatch;

mod idle;
mod latency;
mod locate;
mod source;
mod store_grants;
mod walk;

pub use self::locate::{HEADS_IN_FLIGHT, LOCATE_PATH, MAX_LOCATE_LINE_BYTES, Span, large_key};
pub use self::source::BlobSource;
pub use self::store_grants::{
    BODY_BYTES_PER_GRANT, GRANT_LIFETIME, KEYS_PER_GRANT, StoreGrants, check_store_bucket,
    check_store_url,
};
pub use tierstore::server::{AccessKey, GrantError as StoreGrantError};

pub use self::walk::MAX_SCOPE_BLOB_BYTES;
use self::walk::{Budget, ScopeError, walk};

/// The size above which a blob is streamed and below which (itself included) it is read
/// whole: the per-blob cap of the packs a task's results are written into.
pub const STREAM_OVER_BYTES: u64 = MAX_UNTRUSTED_BLOB_BYTES;

/// The path of the batch read, which takes a POST.
pub const BATCH_PATH: &str = "/batch";

/// The most digests a batch names.
pub const BATCH_MAX_DIGESTS: usize = 256;

/// The most bytes a batch asks for, and the largest blob it answers: 4 MiB.
pub const BATCH_MAX_BYTES: u64 = 4 << 20;

/// The most bytes of a batch request's body: far more than the largest valid one (256 digests
/// of at most 90 bytes each), so that no body of any size is held.
pub const MAX_BATCH_REQUEST_BYTES: usize = 64 << 10;

/// The size of the pieces a held body is handed to a connection in: a connection takes the next
/// when it has room, so what the budget no longer charges is no more than one piece.
pub const HELD_CHUNK_BYTES: usize = 256 << 10;

/// What a batch is charged for each gap between its blobs: the pack layer reads blobs that lie
/// within this of each other in one range, and the slices of that range pin the gap.
pub const BATCH_GAP_BYTES: u64 = 64 << 10;

/// The default of [`Limits::held_idle`]: a minute.
pub const DEFAULT_HELD_IDLE: Duration = Duration::from_secs(60);

/// The default of [`Limits::read_budget_bytes`]: 1 GiB.
pub const DEFAULT_READ_BUDGET_BYTES: u64 = 1 << 30;

/// The default of [`Limits::stream_window`]: what a streamed blob holds at a time (the read
/// window of the store, 8 MiB, and the chunk being sent) with some to spare.
pub const STREAM_WINDOW_BYTES: u64 = 16 << 20;

/// The default of [`Limits::max_blob_bytes`]: 512 MiB, over the largest input a compile reads
/// (a 165 MiB toolchain binary).
pub const DEFAULT_MAX_BLOB_BYTES: u64 = 512 << 20;

/// The bounds of a [`JobRead`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// The places for reads of blobs (at least 1): a GET takes one, a streamed one until its
    /// response ends, and a batch one reservation of at most half of them for its whole read; a
    /// request over it waits for room.
    pub reads_in_flight: usize,
    /// The bytes of reads open at once, in all (at least 1 KiB): a read takes its blob's size
    /// first (a streamed blob [`Limits::stream_window`] of it, the scope walk's reads their
    /// own sizes, a batch the sizes of its blobs and the gaps between them), and keeps it until
    /// its reply is sent or dropped (the bytes are held until then); one over the budget waits
    /// for room and never fails, and a read larger than the whole budget takes the whole of it.
    pub read_budget_bytes: u64,
    /// What a streamed blob takes from the budget, at most its size. Not a flag; a test
    /// lowers it.
    pub stream_window: u64,
    /// How long a body that holds budget may go with no chunk taken by its connection before
    /// it is dropped, its budget given back and its response ended: a client that does not
    /// read does not hold the budget for ever. It counts from the last chunk taken, so a client
    /// that reads makes progress however long its response takes in all.
    pub held_idle: Duration,
    /// The most bytes of one blob a response carries: a blob of more is 413, before anything
    /// is read.
    pub max_blob_bytes: u64,
    /// The size over which a blob is streamed, not read whole. Not a flag: it is the bound of
    /// the whole-blob read ([`STREAM_OVER_BYTES`]), a test lowers it to stream small blobs. It must equal the pack store's
    /// `max_blob_bytes`: `GET /locate` asks the index for the blobs up to it.
    pub stream_over: u64,
    /// The most digests a scope may hold: an input tree past it gives its Action no scope.
    pub scope_digests: usize,
    /// The Actions whose scope is remembered; at the bound the memory is emptied of the walks
    /// that are done (those in progress stay, so that a second ask joins them) and begins
    /// again.
    pub scopes: usize,
    /// How long `GET /locate` remembers that tierstore holds a large object (seconds, not
    /// forever: an object can be removed).
    pub locate_present_for: std::time::Duration,
    /// How long `GET /locate` remembers that tierstore lacks a large object: short, because an
    /// ingest or a backfill may write it.
    pub locate_absent_for: std::time::Duration,
    /// The digests whose presence in tierstore `GET /locate` remembers, at most: past it the
    /// oldest answer goes.
    pub locate_remembered: usize,
    /// The most large digests one `GET /locate` checks in tierstore; the others are `absent`.
    pub locate_heads_per_request: usize,
    /// The most digests one `GET /locate` asks the pack index about; the others are `absent`.
    pub locate_packed_per_request: usize,
    /// How long the store phase of one `GET /locate` may take: the index ask and the presence
    /// checks together, the wait for a place among all requests' checks included. An index not
    /// answering by then is a 503; a blob whose check is not answered is `absent` (and the
    /// answer not remembered).
    pub locate_head_timeout: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            reads_in_flight: 32,
            read_budget_bytes: DEFAULT_READ_BUDGET_BYTES,
            stream_window: STREAM_WINDOW_BYTES,
            held_idle: DEFAULT_HELD_IDLE,
            max_blob_bytes: DEFAULT_MAX_BLOB_BYTES,
            stream_over: STREAM_OVER_BYTES,
            scope_digests: 1 << 20,
            scopes: 256,
            locate_present_for: std::time::Duration::from_secs(10),
            locate_absent_for: std::time::Duration::from_secs(2),
            locate_remembered: 4096,
            locate_heads_per_request: 256,
            locate_packed_per_request: 1 << 16,
            locate_head_timeout: Duration::from_secs(10),
        }
    }
}

/// What the listener answers a request with.
pub struct Reply {
    /// The status.
    pub status: StatusCode,
    /// The blob on 200 when it is read whole, else the reason code and a newline; empty for a
    /// streamed blob.
    pub body: Bytes,
    /// The blob of a 200 that is streamed ([`Limits::stream_over`]), whose `body` is empty.
    pub stream: Option<BlobStream>,
    /// What the bytes of `body` are charged to until the reply is sent or dropped.
    hold: Option<Hold>,
}

impl std::fmt::Debug for Reply {
    /// Shows the status, the size of the body and the stream (its size, never its bytes), and not
    /// the body: a `/locate` body carries bearer tokens.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Reply")
            .field("status", &self.status)
            .field("body_len", &self.body.len())
            .field("stream", &self.stream)
            .finish_non_exhaustive()
    }
}

/// The read places and budget a reply keeps until it is dropped, so that bytes held for a
/// client that is slow to read them stay charged to the budget.
#[derive(Debug)]
struct Hold {
    _permits: Vec<Result<OwnedSemaphorePermit, AcquireError>>,
    /// How long a body that keeps them may go with no chunk taken ([`Limits::held_idle`]).
    idle: Duration,
}

impl Reply {
    /// The body as a stream of pieces of at most [`HELD_CHUNK_BYTES`], when the reply holds
    /// budget for it, which then stays held until the stream ends or is dropped; `body` is
    /// then empty. None (and `body` as it was) for a reply that holds nothing. The caller that
    /// sends the reply calls this, so that the budget is not given back while the connection
    /// still has the bytes.
    pub fn held_stream(&mut self) -> Option<BlobStream> {
        let hold = self.hold.take()?;
        let idle_limit = hold.idle;
        let body = std::mem::take(&mut self.body);
        let size = body.len();
        let pieces: Vec<Bytes> = (0..size)
            .step_by(HELD_CHUNK_BYTES)
            .map(|at| body.slice(at..(at + HELD_CHUNK_BYTES).min(size)))
            .collect();
        let chunks = stream::unfold(
            (pieces.into_iter(), hold),
            |(mut pieces, hold)| async move {
                let piece = pieces.next()?;
                Some((Ok(piece), (pieces, hold)))
            },
        )
        .boxed();
        Some(BlobStream {
            size: size as u64,
            chunks: idle::bounded(chunks, idle_limit),
        })
    }
}

/// A blob being streamed to a response: its length, and its chunks as they are checked.
pub struct BlobStream {
    /// The blob's length, which a response states before its first byte.
    pub size: u64,
    /// The chunks, in order. A blob whose bytes are not its digest's ends in an error and
    /// without its last chunk; a stream that ends cleanly delivered the blob. Dropping it
    /// gives the read's place back.
    pub chunks: BoxStream<'static, Result<Bytes, StreamError>>,
}

impl std::fmt::Debug for BlobStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BlobStream")
            .field("size", &self.size)
            .finish_non_exhaustive()
    }
}

/// Why a streamed response ended before the blob did.
#[derive(Debug, thiserror::Error)]
pub enum StreamError {
    /// The bytes the source gave are not the blob's: they do not hash to its digest, or are
    /// more or fewer than its size.
    #[error("the blob's bytes are not its digest's")]
    Corrupt,
    /// The source failed part way.
    #[error("the store failed part way: {0}")]
    Store(#[from] StorageError),
    /// The client took no chunk for [`Limits::held_idle`]: the body was dropped.
    #[error("the client did not read for the idle limit")]
    Idle,
}

/// The streams that ended early, by cause.
#[derive(Default)]
struct EndedEarly {
    digest_mismatch: Counter,
    store_error: Counter,
}

/// How a request ended, which is how it is counted.
#[derive(Clone, Copy)]
enum Outcome {
    Ok,
    Unauthorized,
    Forbidden,
    NotFound,
    TooLarge,
    MethodNotAllowed,
    Unavailable,
    Malformed,
}

/// The outcomes of a GET, in the order they are counted.
const GET_OUTCOMES: [Outcome; 7] = [
    Outcome::Ok,
    Outcome::Unauthorized,
    Outcome::Forbidden,
    Outcome::NotFound,
    Outcome::TooLarge,
    Outcome::MethodNotAllowed,
    Outcome::Unavailable,
];

/// The outcomes of a locate.
const LOCATE_OUTCOMES: [Outcome; 4] = [
    Outcome::Ok,
    Outcome::Unauthorized,
    Outcome::Forbidden,
    Outcome::Unavailable,
];

/// The outcomes of a batch.
const BATCH_OUTCOMES: [Outcome; 6] = [
    Outcome::Ok,
    Outcome::Unauthorized,
    Outcome::Forbidden,
    Outcome::Malformed,
    Outcome::TooLarge,
    Outcome::Unavailable,
];

/// The counters of the variants of [`Outcome`].
type Tally = [Counter; 8];

/// How one blob of a batch was answered. The discriminants are the status bytes of a frame.
#[derive(Clone, Copy)]
enum BlobStatus {
    Ok = 0,
    NotFound = 1,
    OutOfScope = 2,
    TooLarge = 3,
    StoreUnavailable = 4,
}

/// The label of each [`BlobStatus`], in the order of the variants.
const BLOB_STATUSES: [&str; 5] = [
    "ok",
    "not_found",
    "out_of_scope",
    "too_large",
    "store_unavailable",
];

impl Outcome {
    fn label(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Unauthorized => "unauthorized",
            Self::Forbidden => "forbidden",
            Self::NotFound => "not_found",
            Self::TooLarge => "too_large",
            Self::MethodNotAllowed => "method_not_allowed",
            Self::Unavailable => "unavailable",
            Self::Malformed => "malformed",
        }
    }

    fn status(self) -> StatusCode {
        match self {
            Self::Ok => StatusCode::OK,
            Self::Unauthorized => StatusCode::UNAUTHORIZED,
            Self::Forbidden => StatusCode::FORBIDDEN,
            Self::NotFound => StatusCode::NOT_FOUND,
            Self::TooLarge => StatusCode::PAYLOAD_TOO_LARGE,
            Self::MethodNotAllowed => StatusCode::METHOD_NOT_ALLOWED,
            Self::Unavailable => StatusCode::SERVICE_UNAVAILABLE,
            Self::Malformed => StatusCode::BAD_REQUEST,
        }
    }
}

/// The scope of each Action worked out, each in a cell that is filled by the first ask.
type Cells = HashMap<Digest, Arc<OnceCell<Arc<Scope>>>>;

/// The job-facing read of one frontend. Shared by every connection.
pub struct JobRead {
    source: Arc<dyn BlobSource>,
    grants: Arc<Grants>,
    limits: Limits,
    scopes: Mutex<Cells>,
    locate: locate::Locating,
    reads: Arc<Semaphore>,
    budget: Budget,
    requests: Tally,
    batches: Tally,
    blobs: [Counter; BLOB_STATUSES.len()],
    bytes: Arc<Counter>,
    ended_early: Arc<EndedEarly>,
    backfill: Option<LargeBackfill>,
    /// What `GET /locate` signs store read grants with; none, and it says nothing of grants.
    store_grants: Option<StoreGrants>,
    latency: latency::Latency,
}

impl JobRead {
    /// The read of `source` for the grants `grants` issued, within `limits`.
    #[must_use]
    pub fn new(source: Arc<dyn BlobSource>, grants: Arc<Grants>, limits: Limits) -> Self {
        Self {
            source,
            grants,
            reads: Arc::new(Semaphore::new(limits.reads_in_flight.max(1))),
            budget: Budget::new(limits.read_budget_bytes),
            limits,
            scopes: Mutex::new(HashMap::new()),
            locate: locate::Locating::new(&limits),
            requests: Default::default(),
            batches: Default::default(),
            blobs: Default::default(),
            bytes: Arc::default(),
            ended_early: Arc::default(),
            backfill: None,
            store_grants: None,
            latency: latency::Latency::new(),
        }
    }

    /// This read, timing its answers by `clock` (the process's unless told).
    #[must_use]
    pub fn with_stopwatch(mut self, clock: Stopwatch) -> Self {
        self.latency = self.latency.with_clock(clock);
        self
    }

    /// This read, handing out store read grants with the table of `GET /locate` (see the
    /// `locate` module); `None` hands out none, and the table is the listener-only one.
    #[must_use]
    pub fn with_store_grants(mut self, grants: Option<StoreGrants>) -> Self {
        self.store_grants = grants;
        self
    }

    /// How many Actions the memory of scopes holds now, walks in progress included: at most
    /// [`Limits::scopes`] once the walks that are done, failed or cancelled are dropped (which
    /// happens when an Action arrives at the bound).
    #[must_use]
    pub fn scopes_held(&self) -> usize {
        self.scopes
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }

    /// This read, copying each large blob it streams to tierstore through `backfill` once it
    /// has hashed to its digest ([`crate::large_backfill`]); `None` copies nothing.
    #[must_use]
    pub fn with_backfill(mut self, backfill: Option<LargeBackfill>) -> Self {
        self.backfill = backfill;
        self
    }

    /// The answer to `method` `path` with the `Authorization` header `authorization`.
    /// `path` is the request's path alone, without its query.
    pub async fn respond(&self, method: &Method, path: &str, authorization: Option<&str>) -> Reply {
        self.respond_with_body(method, path, authorization, &[])
            .await
    }

    /// [`JobRead::respond`] for a request that has a body: `body` is what a `POST /batch`
    /// carries, and is read by the caller, at most [`MAX_BATCH_REQUEST_BYTES`] of it (a body
    /// that is longer or does not arrive is passed as empty, which is a malformed request).
    pub async fn respond_with_body(
        &self,
        method: &Method,
        path: &str,
        authorization: Option<&str>,
        body: &[u8],
    ) -> Reply {
        let answer = self.route(method, path, authorization, body);
        self.latency.timed((method, path), answer).await
    }

    async fn route(
        &self,
        method: &Method,
        path: &str,
        authorization: Option<&str>,
        body: &[u8],
    ) -> Reply {
        if path == BATCH_PATH {
            if *method != Method::POST {
                return self.refuse(Outcome::MethodNotAllowed, "method_not_allowed");
            }
            return self.batch(authorization, body).await;
        }
        if *method != Method::GET {
            return self.refuse(Outcome::MethodNotAllowed, "method_not_allowed");
        }
        if path == LOCATE_PATH {
            return self.locate(authorization).await;
        }
        let Some(digest) = digest_of(path) else {
            return self.refuse(Outcome::NotFound, "not_found");
        };
        let claims = match self.claims(authorization) {
            Ok(claims) => claims,
            Err(reason) => return self.refuse(Outcome::Unauthorized, reason),
        };
        let scope = match self.scope(claims.action).await {
            Ok(scope) => scope,
            Err(ScopeError::Unusable) => return self.refuse(Outcome::Forbidden, "no_scope"),
            Err(ScopeError::Store) => {
                return self.refuse(Outcome::Unavailable, "store_unavailable");
            }
        };
        if scope.check(&claims, &[digest]).is_err() {
            return self.refuse(Outcome::Forbidden, "out_of_scope");
        }
        if digest.size_bytes > self.limits.max_blob_bytes {
            return self.refuse(Outcome::TooLarge, "too_large");
        }
        if is_empty_blob(&digest) {
            return self.serve(Bytes::new());
        }
        let place = Arc::clone(&self.reads).acquire_owned().await;
        if digest.size_bytes > self.limits.stream_over {
            let room = self
                .budget
                .take(digest.size_bytes.min(self.limits.stream_window))
                .await;
            return match self.source.read_stream(&digest).await {
                Ok(read) => self.serve_stream(digest, read, (place, room)),
                Err(error) => self.unreadable(&error),
            };
        }
        let room = self.budget.take(digest.size_bytes).await;
        match self.source.read_blob(&digest).await {
            Ok(bytes) => {
                // The place is given back with the read; the bytes stay charged to the budget
                // until the reply is sent.
                let mut reply = self.serve(bytes);
                reply.hold = Some(Hold {
                    _permits: vec![room],
                    idle: self.limits.held_idle,
                });
                reply
            }
            Err(error) => self.unreadable(&error),
        }
    }

    /// The answer to a `POST /batch` with `body`, from [`JobRead::respond_with_body`].
    async fn batch(&self, authorization: Option<&str>, body: &[u8]) -> Reply {
        let claims = match self.claims(authorization) {
            Ok(claims) => claims,
            Err(reason) => return self.refuse_batch(Outcome::Unauthorized, reason),
        };
        let Some(digests) = batch_digests(body) else {
            return self.refuse_batch(Outcome::Malformed, "malformed");
        };
        let asked: u64 = digests
            .iter()
            .map(|digest| digest.size_bytes)
            .filter(|size| *size <= BATCH_MAX_BYTES)
            .sum();
        if asked > BATCH_MAX_BYTES {
            return self.refuse_batch(Outcome::TooLarge, "too_large");
        }
        let scope = match self.scope(claims.action).await {
            Ok(scope) => scope,
            Err(ScopeError::Unusable) => return self.refuse_batch(Outcome::Forbidden, "no_scope"),
            Err(ScopeError::Store) => {
                return self.refuse_batch(Outcome::Unavailable, "store_unavailable");
            }
        };
        let (answers, held) = self.batch_blobs(&claims, &scope, &digests).await;
        let mut frames = Vec::new();
        for (status, bytes) in answers {
            self.blobs[status as usize].add(1);
            self.bytes.add(bytes.len() as u64);
            frames.push(status as u8);
            frames.extend_from_slice(&(bytes.len() as u64).to_be_bytes());
            frames.extend_from_slice(&bytes);
        }
        let mut reply = self.tally(&self.batches, Outcome::Ok, Bytes::from(frames));
        reply.hold = Some(Hold {
            _permits: held,
            idle: self.limits.held_idle,
        });
        reply
    }

    /// How each of `digests` is answered, in order, and the bytes of the ok ones, with the
    /// budget they are charged to, which the reply keeps until it is sent. The blobs that need
    /// a read are read together, each once, with [`BlobSource::read_many`], under a take of at
    /// most half the reads in flight (as many places as blobs) while they are read, and of the
    /// budget: their sizes together and [`BATCH_GAP_BYTES`] for each gap between them.
    /// A batch takes one reservation of places, not one place per blob. The places bound the
    /// batches that read at once, not the reads of one: a source reads the
    /// blobs it does not read together concurrently, up to the batch's size (at most
    /// [`BATCH_MAX_DIGESTS`]), and what bounds their bytes is the budget.
    async fn batch_blobs(
        &self,
        claims: &Claims,
        scope: &Scope,
        digests: &[Digest],
    ) -> (
        Vec<(BlobStatus, Bytes)>,
        Vec<Result<OwnedSemaphorePermit, AcquireError>>,
    ) {
        let allowed: Vec<bool> = digests
            .iter()
            .map(|digest| scope.check(claims, std::slice::from_ref(digest)).is_ok())
            .collect();
        let mut wanted: Vec<Digest> = Vec::new();
        for (digest, allowed) in digests.iter().zip(&allowed) {
            let readable = *allowed && digest.size_bytes <= BATCH_MAX_BYTES;
            if readable && !is_empty_blob(digest) && !wanted.contains(digest) {
                wanted.push(*digest);
            }
        }
        let mut read: HashMap<Digest, (BlobStatus, Bytes)> = HashMap::new();
        let mut held = Vec::new();
        if !wanted.is_empty() {
            let share = (self.limits.reads_in_flight / 2).max(1);
            let places = wanted.len().min(share);
            let _places = Arc::clone(&self.reads)
                .acquire_many_owned(u32::try_from(places).unwrap_or(u32::MAX))
                .await;
            let sizes: u64 = wanted.iter().map(|digest| digest.size_bytes).sum();
            let gaps = BATCH_GAP_BYTES * (wanted.len() as u64).saturating_sub(1);
            held.push(self.budget.take(sizes + gaps).await);
            let answers = self.source.read_many(&wanted).await;
            for (digest, answer) in wanted.iter().zip(answers) {
                let status = match answer {
                    Ok(bytes) if kbf_segments::sha256(&bytes) == *digest => (BlobStatus::Ok, bytes),
                    Ok(_) => (BlobStatus::StoreUnavailable, Bytes::new()),
                    Err(StorageError::NotFound(_)) => (BlobStatus::NotFound, Bytes::new()),
                    Err(_) => (BlobStatus::StoreUnavailable, Bytes::new()),
                };
                read.insert(*digest, status);
            }
        }
        let answers = digests
            .iter()
            .zip(&allowed)
            .map(|(digest, allowed)| {
                if !allowed {
                    (BlobStatus::OutOfScope, Bytes::new())
                } else if digest.size_bytes > BATCH_MAX_BYTES {
                    (BlobStatus::TooLarge, Bytes::new())
                } else if is_empty_blob(digest) {
                    (BlobStatus::Ok, Bytes::new())
                } else {
                    let unanswered = (BlobStatus::StoreUnavailable, Bytes::new());
                    read.get(digest).cloned().unwrap_or(unanswered)
                }
            })
            .collect();
        (answers, held)
    }

    fn refuse_batch(&self, outcome: Outcome, reason: &'static str) -> Reply {
        self.tally(&self.batches, outcome, Bytes::from(format!("{reason}\n")))
    }

    /// The refusal for a blob the source could not give.
    fn unreadable(&self, error: &StorageError) -> Reply {
        match error {
            StorageError::NotFound(_) => self.refuse(Outcome::NotFound, "not_found"),
            _ => self.refuse(Outcome::Unavailable, "store_unavailable"),
        }
    }

    /// The claims of the grant in `authorization`, or the reason code it is refused with.
    fn claims(&self, authorization: Option<&str>) -> Result<Claims, &'static str> {
        let token = bearer(authorization).ok_or("missing_token")?;
        self.grants.verify(token).map_err(|error| match error {
            GrantError::Malformed | GrantError::BadMac => "invalid_token",
            GrantError::Expired => "expired",
            GrantError::Revoked => "revoked",
        })
    }

    /// The scope of `action`, worked out on the first ask and remembered. A failure is not
    /// remembered: the blobs may be there at the next ask.
    async fn scope(&self, action: Digest) -> Result<Arc<Scope>, ScopeError> {
        let cell = {
            let mut cells = self.scopes.lock().unwrap_or_else(PoisonError::into_inner);
            if cells.len() >= self.limits.scopes {
                // A walk that is done, and one that no one is waiting on (it failed, or was
                // cancelled), go; one in progress stays, so that a second ask joins it.
                cells.retain(|_, cell| !cell.initialized() && Arc::strong_count(cell) > 1);
            }
            Arc::clone(cells.entry(action).or_default())
        };
        let scope = cell
            .get_or_try_init(|| async {
                walk(
                    &*self.source,
                    &self.budget,
                    action,
                    self.limits.scope_digests,
                )
                .await
                .map(Arc::new)
            })
            .await?;
        Ok(Arc::clone(scope))
    }

    fn serve(&self, body: Bytes) -> Reply {
        self.bytes.add(body.len() as u64);
        self.answer(Outcome::Ok, body)
    }

    /// A 200 whose body is `read`, passed on as it is hashed; `held` is kept until the
    /// stream ends or is dropped.
    fn serve_stream(
        &self,
        digest: Digest,
        read: ReadStream,
        held: (
            Result<OwnedSemaphorePermit, AcquireError>,
            Result<OwnedSemaphorePermit, AcquireError>,
        ),
    ) -> Reply {
        let state = Streaming {
            read,
            hasher: Sha256::new(),
            digest,
            seen: 0,
            held: None,
            finished: false,
            bytes: Arc::clone(&self.bytes),
            ended_early: Arc::clone(&self.ended_early),
            tee: self
                .backfill
                .as_ref()
                .and_then(|backfill| backfill.tee(&digest)),
            _held: held,
        };
        let chunks = stream::unfold(Some(state), |state| async move {
            let mut state = state?;
            match state.step().await {
                Step::Chunk(chunk) => Some((Ok(chunk), Some(state))),
                Step::Fail(error) => Some((Err(error), None)),
                Step::Done => None,
            }
        })
        .boxed();
        let mut reply = self.answer(Outcome::Ok, Bytes::new());
        reply.stream = Some(BlobStream {
            size: digest.size_bytes,
            chunks: idle::bounded(chunks, self.limits.held_idle),
        });
        reply
    }

    fn refuse(&self, outcome: Outcome, reason: &'static str) -> Reply {
        self.answer(outcome, Bytes::from(format!("{reason}\n")))
    }

    fn answer(&self, outcome: Outcome, body: Bytes) -> Reply {
        self.tally(&self.requests, outcome, body)
    }

    /// `body` as the answer of `outcome`, counted in `counters`.
    fn tally(&self, counters: &Tally, outcome: Outcome, body: Bytes) -> Reply {
        counters[outcome as usize].add(1);
        Reply {
            status: outcome.status(),
            body,
            stream: None,
            hold: None,
        }
    }

    /// Appends the counters to `out`: `kbf_job_read_requests_total` by `outcome` and
    /// `kbf_job_read_bytes_total`.
    pub fn render_metrics(&self, out: &mut String) {
        let mut families = Families(std::mem::take(out));
        let name = "kbf_job_read_requests_total";
        families.head(
            name,
            "counter",
            "Job-facing read requests answered, by outcome",
        );
        for outcome in GET_OUTCOMES {
            let count = self.requests[outcome as usize].get();
            let label = outcome.label();
            let _ = writeln!(families.0, "{name}{{outcome=\"{label}\"}} {count}");
        }
        families.counter(
            "kbf_job_read_bytes_total",
            "Blob bytes served to jobs",
            self.bytes.get(),
        );
        let name = "kbf_job_batch_requests_total";
        families.head(
            name,
            "counter",
            "Job-facing batch read requests answered, by outcome",
        );
        for outcome in BATCH_OUTCOMES {
            let count = self.batches[outcome as usize].get();
            let label = outcome.label();
            let _ = writeln!(families.0, "{name}{{outcome=\"{label}\"}} {count}");
        }
        let name = "kbf_job_batch_blobs_total";
        families.head(
            name,
            "counter",
            "Blobs of job-facing batch reads, by the status they were answered with",
        );
        for (status, counter) in BLOB_STATUSES.iter().zip(&self.blobs) {
            let _ = writeln!(
                families.0,
                "{name}{{status=\"{status}\"}} {}",
                counter.get()
            );
        }
        let name = "kbf_job_read_stream_ended_early_total";
        families.head(
            name,
            "counter",
            "Streamed blob responses that ended before the blob did, by cause",
        );
        let causes = [
            ("digest_mismatch", &self.ended_early.digest_mismatch),
            ("store_error", &self.ended_early.store_error),
        ];
        for (reason, counter) in causes {
            let _ = writeln!(
                families.0,
                "{name}{{reason=\"{reason}\"}} {}",
                counter.get()
            );
        }
        self.latency.render(&mut families);
        self.locate.render_metrics(&mut families);
        *out = families.0;
        if let Some(backfill) = &self.backfill {
            backfill.render_metrics(out);
        }
    }

    /// [`JobRead::render_metrics`] when there is a job-facing read, and nothing when there
    /// is none, so that a scrape has one branch for it.
    pub fn render_metrics_of(read: Option<&Self>, out: &mut String) {
        if let Some(read) = read {
            read.render_metrics(out);
        }
    }
}

/// A blob being streamed: what [`Streaming::step`] passes on and what it has seen.
struct Streaming {
    read: ReadStream,
    hasher: Sha256,
    digest: Digest,
    /// The bytes the source has given.
    seen: u64,
    /// The last chunk given, held until the next arrives or the blob has hashed to its digest.
    held: Option<Bytes>,
    finished: bool,
    bytes: Arc<Counter>,
    ended_early: Arc<EndedEarly>,
    /// The copy of the blob to tierstore, fed what is passed on; dropped on any end but a
    /// verified one, which abandons the copy.
    tee: Option<Tee>,
    /// The read's place among [`Limits::reads_in_flight`] and its window of the budget, given
    /// back when the stream is dropped.
    _held: (
        Result<OwnedSemaphorePermit, AcquireError>,
        Result<OwnedSemaphorePermit, AcquireError>,
    ),
}

/// What one step of a [`Streaming`] came to.
enum Step {
    Chunk(Bytes),
    Fail(StreamError),
    Done,
}

impl Streaming {
    /// The next chunk to pass on. A chunk is passed on when the one after it has arrived, so
    /// the last is passed on only once the whole has hashed to the digest.
    async fn step(&mut self) -> Step {
        if self.finished {
            return Step::Done;
        }
        loop {
            match self.read.next().await {
                Some(Ok(chunk)) if chunk.is_empty() => {}
                Some(Ok(chunk)) => {
                    self.seen += chunk.len() as u64;
                    if self.seen > self.digest.size_bytes {
                        return self.corrupt();
                    }
                    self.hasher.update(&chunk);
                    if let Some(ready) = self.held.replace(chunk) {
                        self.bytes.add(ready.len() as u64);
                        self.copy(&ready);
                        return Step::Chunk(ready);
                    }
                }
                Some(Err(StorageError::DigestMismatch { .. } | StorageError::Corrupt { .. })) => {
                    return self.corrupt();
                }
                Some(Err(error)) => {
                    self.tee = None;
                    self.ended_early.store_error.add(1);
                    return Step::Fail(StreamError::Store(error));
                }
                None => return self.end(),
            }
        }
    }

    /// The source has no more: the last chunk is passed on if the whole is the blob.
    fn end(&mut self) -> Step {
        let hash: [u8; 32] = self.hasher.clone().finalize().into();
        if self.seen != self.digest.size_bytes || hash != self.digest.hash {
            return self.corrupt();
        }
        self.finished = true;
        // Held, as the blob is not empty ([`is_empty_blob`] is served before a read).
        let last = self.held.take().unwrap_or_default();
        self.bytes.add(last.len() as u64);
        self.copy(&last);
        if let Some(tee) = self.tee.take() {
            tee.finish();
        }
        Step::Chunk(last)
    }

    /// Hands `chunk`, which the response is given, to the copy; a copy that cannot take it
    /// is dropped, and the response goes on.
    fn copy(&mut self, chunk: &Bytes) {
        if let Some(tee) = &mut self.tee
            && !tee.send(chunk.clone())
        {
            self.tee = None;
        }
    }

    fn corrupt(&mut self) -> Step {
        self.tee = None;
        self.ended_early.digest_mismatch.add(1);
        Step::Fail(StreamError::Corrupt)
    }
}

/// The digests of a batch request's body, `{"digests": ["<hex>/<size>", ...]}`: none if it is
/// not that, has no entries or more than [`BATCH_MAX_DIGESTS`], or has an entry that is not a
/// digest in the form of a GET path.
fn batch_digests(body: &[u8]) -> Option<Vec<Digest>> {
    let value: serde_json::Value = serde_json::from_slice(body).ok()?;
    let list = value.get("digests")?.as_array()?;
    if list.is_empty() || list.len() > BATCH_MAX_DIGESTS {
        return None;
    }
    list.iter()
        .map(|entry| Digest::parse(DigestFunction::Sha256, entry.as_str()?).ok())
        .collect()
}

/// The token of an `Authorization: Bearer <token>` header (the scheme in any case).
fn bearer(authorization: Option<&str>) -> Option<&str> {
    let (scheme, token) = authorization?.split_once(' ')?;
    scheme
        .eq_ignore_ascii_case("bearer")
        .then_some(token.trim())
}

/// The digest a path `/<hex>/<size>` names.
fn digest_of(path: &str) -> Option<Digest> {
    Digest::parse(DigestFunction::Sha256, path.strip_prefix('/')?).ok()
}
