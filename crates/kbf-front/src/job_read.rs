//! The job-facing read: how a task on a worker fetches the inputs of its one Action.
//!
//! A task holds a read grant ([`crate::grants`]), not a client certificate, so it cannot use
//! the REAPI listener. The job-facing listener of `kbf frontend` (`--job-listen`) answers it
//! over HTTPS, and this module is what answers: `GET /<hex>/<size>` with
//! `Authorization: Bearer <grant>`, the form `kbf-task` requests, and nothing else. Every
//! other path is 404 and every other method 405.
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
//! before anything is read), and at most [`Limits::reads_in_flight`] reads are open at once,
//! a streamed one until its response ends or is dropped; the others wait for one to end.
//!
//! An answer carries a reason code in its body and no other detail: `missing_token`,
//! `invalid_token`, `expired`, `revoked`, `no_scope`, `out_of_scope`, `too_large`,
//! `not_found`, `method_not_allowed` or `store_unavailable`. Requests by outcome, the
//! bytes served and the streams that ended early are counted ([`JobRead::render_metrics`]).

use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::sync::{Arc, Mutex, PoisonError};

use bytes::Bytes;
use futures::future::BoxFuture;
use futures::stream::{self, BoxStream, StreamExt as _, TryStreamExt as _};
use kbf_proto::reapi;
use kbf_storage::{ReadStream, StorageError};
use kbf_types::{Digest, DigestFunction};
use prost::Message as _;
use sha2::{Digest as _, Sha256};
use tokio::sync::{AcquireError, OnceCell, OwnedSemaphorePermit, Semaphore};
use tonic::codegen::http::{Method, StatusCode};

use crate::backend::Backend;
use crate::cache::is_empty_blob;
use crate::grants::{Claims, GrantError, Grants, Scope};
use crate::metrics::{Counter, Families};
use crate::proxy::MAX_UNTRUSTED_BLOB_BYTES;
use crate::wire;

/// The size above which a blob is streamed and below which (itself included) it is read
/// whole: the per-blob cap of the packs a task's results are written into.
pub const STREAM_OVER_BYTES: u64 = MAX_UNTRUSTED_BLOB_BYTES;

/// The default of [`Limits::max_blob_bytes`]: 512 MiB, over the largest input a compile reads
/// (a 165 MiB toolchain binary).
pub const DEFAULT_MAX_BLOB_BYTES: u64 = 512 << 20;

/// The Directory blobs of one level of an input tree read at once.
const DIRECTORIES_IN_FLIGHT: usize = 16;

/// Where the blobs a job reads come from.
///
/// An implementation returns the bytes that hash to `digest`, or [`StorageError::NotFound`]
/// when it holds none.
pub trait BlobSource: Send + Sync {
    /// The blob `digest`, read whole and checked against its digest.
    fn read_blob<'a>(&'a self, digest: &'a Digest) -> BoxFuture<'a, Result<Bytes, StorageError>>;

    /// The blob `digest` as a stream of chunks, for one too large to hold: the answer is
    /// [`StorageError::NotFound`] when the source holds none, and an error item ends a stream
    /// that fails part way. The bytes are not trusted to be the blob's: the listener hashes
    /// them as they pass.
    fn read_stream<'a>(
        &'a self,
        digest: &'a Digest,
    ) -> BoxFuture<'a, Result<ReadStream, StorageError>>;
}

impl BlobSource for Backend {
    fn read_blob<'a>(&'a self, digest: &'a Digest) -> BoxFuture<'a, Result<Bytes, StorageError>> {
        Box::pin(Backend::read_blob(self, digest))
    }

    fn read_stream<'a>(
        &'a self,
        digest: &'a Digest,
    ) -> BoxFuture<'a, Result<ReadStream, StorageError>> {
        Box::pin(Backend::read_stream(self, digest))
    }
}

/// The bounds of a [`JobRead`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// The reads of blobs open at once (at least 1), a streamed one until its response ends;
    /// a request over it waits for one to end.
    pub reads_in_flight: usize,
    /// The most bytes of one blob a response carries: a blob of more is 413, before anything
    /// is read.
    pub max_blob_bytes: u64,
    /// The size over which a blob is streamed, not read whole. Not a flag: it is the bound of
    /// the whole-blob read ([`STREAM_OVER_BYTES`]), a test lowers it to stream small blobs.
    pub stream_over: u64,
    /// The most digests a scope may hold: an input tree past it gives its Action no scope.
    pub scope_digests: usize,
    /// The Actions whose scope is remembered; at the bound the memory is emptied and begins
    /// again.
    pub scopes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            reads_in_flight: 32,
            max_blob_bytes: DEFAULT_MAX_BLOB_BYTES,
            stream_over: STREAM_OVER_BYTES,
            scope_digests: 1 << 20,
            scopes: 256,
        }
    }
}

/// What the listener answers a request with.
#[derive(Debug)]
pub struct Reply {
    /// The status.
    pub status: StatusCode,
    /// The blob on 200 when it is read whole, else the reason code and a newline; empty for a
    /// streamed blob.
    pub body: Bytes,
    /// The blob of a 200 that is streamed ([`Limits::stream_over`]), whose `body` is empty.
    pub stream: Option<BlobStream>,
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
}

/// The label of each [`Outcome`], in the order of the variants.
const OUTCOMES: [&str; 7] = [
    "ok",
    "unauthorized",
    "forbidden",
    "not_found",
    "too_large",
    "method_not_allowed",
    "unavailable",
];

impl Outcome {
    fn status(self) -> StatusCode {
        match self {
            Self::Ok => StatusCode::OK,
            Self::Unauthorized => StatusCode::UNAUTHORIZED,
            Self::Forbidden => StatusCode::FORBIDDEN,
            Self::NotFound => StatusCode::NOT_FOUND,
            Self::TooLarge => StatusCode::PAYLOAD_TOO_LARGE,
            Self::MethodNotAllowed => StatusCode::METHOD_NOT_ALLOWED,
            Self::Unavailable => StatusCode::SERVICE_UNAVAILABLE,
        }
    }
}

/// Why an Action has no scope.
enum ScopeError {
    /// Its blobs are not in the store, are not what they must be, or name more than
    /// [`Limits::scope_digests`].
    Unusable,
    /// The store could not answer.
    Store,
}

impl From<StorageError> for ScopeError {
    fn from(error: StorageError) -> Self {
        match error {
            StorageError::NotFound(_) => Self::Unusable,
            _ => Self::Store,
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
    reads: Arc<Semaphore>,
    requests: [Counter; OUTCOMES.len()],
    bytes: Arc<Counter>,
    ended_early: Arc<EndedEarly>,
}

impl JobRead {
    /// The read of `source` for the grants `grants` issued, within `limits`.
    #[must_use]
    pub fn new(source: Arc<dyn BlobSource>, grants: Arc<Grants>, limits: Limits) -> Self {
        Self {
            source,
            grants,
            reads: Arc::new(Semaphore::new(limits.reads_in_flight.max(1))),
            limits,
            scopes: Mutex::new(HashMap::new()),
            requests: Default::default(),
            bytes: Arc::default(),
            ended_early: Arc::default(),
        }
    }

    /// The answer to `method` `path` with the `Authorization` header `authorization`.
    /// `path` is the request's path alone, without its query.
    pub async fn respond(&self, method: &Method, path: &str, authorization: Option<&str>) -> Reply {
        if *method != Method::GET {
            return self.refuse(Outcome::MethodNotAllowed, "method_not_allowed");
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
            return match self.source.read_stream(&digest).await {
                Ok(read) => self.serve_stream(digest, read, place),
                Err(error) => self.unreadable(&error),
            };
        }
        match self.source.read_blob(&digest).await {
            Ok(bytes) => self.serve(bytes),
            Err(error) => self.unreadable(&error),
        }
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
                cells.clear();
            }
            Arc::clone(cells.entry(action).or_default())
        };
        let scope = cell
            .get_or_try_init(|| async {
                walk(&*self.source, action, self.limits.scope_digests)
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

    /// A 200 whose body is `read`, passed on as it is hashed; `place` is held until the
    /// stream ends or is dropped.
    fn serve_stream(
        &self,
        digest: Digest,
        read: ReadStream,
        place: Result<OwnedSemaphorePermit, AcquireError>,
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
            _place: place,
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
            chunks,
        });
        reply
    }

    fn refuse(&self, outcome: Outcome, reason: &'static str) -> Reply {
        self.answer(outcome, Bytes::from(format!("{reason}\n")))
    }

    fn answer(&self, outcome: Outcome, body: Bytes) -> Reply {
        self.requests[outcome as usize].add(1);
        Reply {
            status: outcome.status(),
            body,
            stream: None,
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
        for (outcome, counter) in OUTCOMES.iter().zip(&self.requests) {
            let _ = writeln!(
                families.0,
                "{name}{{outcome=\"{outcome}\"}} {}",
                counter.get()
            );
        }
        families.counter(
            "kbf_job_read_bytes_total",
            "Blob bytes served to jobs",
            self.bytes.get(),
        );
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
        *out = families.0;
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
    /// The read's place among [`Limits::reads_in_flight`], given back when the stream is dropped.
    _place: Result<OwnedSemaphorePermit, AcquireError>,
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
                        return Step::Chunk(ready);
                    }
                }
                Some(Err(StorageError::DigestMismatch { .. } | StorageError::Corrupt { .. })) => {
                    return self.corrupt();
                }
                Some(Err(error)) => {
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
        Step::Chunk(last)
    }

    fn corrupt(&self) -> Step {
        self.ended_early.digest_mismatch.add(1);
        Step::Fail(StreamError::Corrupt)
    }
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

/// The scope of `action`: the Action, its Command, and the Directory blobs and files of the
/// tree under its input root, read from `source` a level at a time.
async fn walk(source: &dyn BlobSource, action: Digest, most: usize) -> Result<Scope, ScopeError> {
    let decoded = reapi::Action::decode(source.read_blob(&action).await?)
        .map_err(|_| ScopeError::Unusable)?;
    let command = proto(decoded.command_digest.as_ref())?;
    let root = proto(decoded.input_root_digest.as_ref())?;
    let mut seen = HashSet::from([root]);
    let mut level = vec![root];
    while !level.is_empty() {
        let directories: Vec<reapi::Directory> = stream::iter(level)
            .map(|digest| directory(source, digest))
            .buffer_unordered(DIRECTORIES_IN_FLIGHT)
            .try_collect()
            .await?;
        level = Vec::new();
        for directory in &directories {
            seen.extend(files_of(directory)?);
            for digest in directories_of(directory)? {
                if seen.insert(digest) {
                    level.push(digest);
                }
            }
        }
        if seen.len() > most {
            return Err(ScopeError::Unusable);
        }
    }
    Ok(Scope::new(action, command, seen))
}

/// The Directory blob `digest`, decoded; the empty blob is the empty Directory.
async fn directory(
    source: &dyn BlobSource,
    digest: Digest,
) -> Result<reapi::Directory, ScopeError> {
    if is_empty_blob(&digest) {
        return Ok(reapi::Directory::default());
    }
    reapi::Directory::decode(source.read_blob(&digest).await?).map_err(|_| ScopeError::Unusable)
}

fn files_of(directory: &reapi::Directory) -> Result<Vec<Digest>, ScopeError> {
    directory
        .files
        .iter()
        .map(|file| proto(file.digest.as_ref()))
        .collect()
}

fn directories_of(directory: &reapi::Directory) -> Result<Vec<Digest>, ScopeError> {
    directory
        .directories
        .iter()
        .map(|child| proto(child.digest.as_ref()))
        .collect()
}

fn proto(digest: Option<&reapi::Digest>) -> Result<Digest, ScopeError> {
    wire::digest(digest).map_err(|_| ScopeError::Unusable)
}
