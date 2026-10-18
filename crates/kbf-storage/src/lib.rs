//! The storage part of the farm, as calls: [`Storage`], the surface every caller of the
//! cache goes through (`docs/design/architecture-v2.md`, sections 3 and 13 P2).
//!
//! CAS reads and writes, FindMissing, the action cache and the `Segments` service, over
//! one store. The one implementation is the cache itself, in this crate ([`Cache`], the
//! write path, the write tier's flusher, the evictor, the rebuild, the input-tree walk,
//! the `Segments` service and the cache's `/metrics` families), behind [`CacheStorage`].
//! `kbf-front` serves the REAPI services over a [`Cache`] and re-exports these names, and
//! `kbf-server`'s in-process store (`kbf_server::LocalStore`, what `kbf-all-in-one`'s
//! daemon reads and writes through) holds an `Arc<dyn Storage>`. A storage process of its
//! own serves one over gRPC, and [`service::RemoteStorage`] is the implementation that
//! calls it ([`service`]).
//!
//! Object safe and asynchronous: each call returns a boxed future ([`Answer`]), so a
//! caller holds `Arc<dyn Storage>` and never names the metadata log or the object store
//! behind it.
//!
//! What every implementation keeps (the cache's promises, `kbf-front`'s crate docs, which serve them):
//! - **FindMissing never omits a digest**, and answers in the order asked.
//! - **Present means durable.** A blob is reported present, and a write answered, only
//!   once its bytes are stored and indexed.
//! - **Every byte is verified,** on write and on read.
//! - **Only a daemon writes the action cache**, and only once every blob the entry
//!   needs is held.
//!
//! This crate depends on no frontend crate: not the scheduler (`kbf-sched`) and not
//! authorization (`kbf-auth`). `crates/kbf-it/tests/layering.rs` checks it.

mod cache;
mod cache_storage;
pub mod closure;
mod cpu;
pub mod flush;
mod input_tree;
pub mod local_cache;
mod meta_log;
pub mod metrics;
pub mod repair;
pub mod replicate;
mod segments;
pub mod service;
pub mod wire;

use std::sync::Arc;

use bytes::Bytes;
use futures::future::BoxFuture;
use futures::stream::BoxStream;
use kbf_meta::{ActionRecord, ActionWriteError, Role};
use kbf_proto::reapi::ActionResult;
use kbf_proto::worker::segments_server::Segments;
use kbf_types::Digest;
use tonic::Status;
use tonic::metadata::MetadataValue;

pub use crate::cache::{
    ADOPT_RECORDS, Admitted, BATCH_BLOBS, BATCH_BYTES, BATCH_READS_IN_FLIGHT, BUFFERED_BLOB_BYTES,
    BlobRead, Cache, CacheError, DEFAULT_REAPI_STREAMS, DEFAULT_WORKER_STREAMS, EVICT_BATCH,
    EVICT_EVERY, EVICT_GRACE, EvictReady, EvictRound, FLUSH_BACKOFF_FIRST, FLUSH_BACKOFF_MAX,
    FLUSH_READ_BYTES, FLUSHES_PER_ROUND, FlushRound, HeldObject, Losses, MAX_ADMISSION_BYTES,
    MAX_DECODED_BLOB_BYTES, NEEDS_V3, ObjectKeys, PASS_OPENS_PER_SEC, PUT_BACKOFF,
    PUT_BACKOFF_FACTOR, PassReport, PoolConfig, READ_WINDOW_BYTES, RebuildConfig, RebuildReport,
    RecordStream, STREAM_WINDOW_BYTES, StreamedRecord, TierStartError, TierStartup, TouchConfig,
    VerifiedBlob, WriteConfig,
};
pub use crate::cache::{HIT_CHECKS_IN_FLIGHT, is_empty_blob};
pub use crate::cache_storage::CacheStorage;
pub use crate::input_tree::{Decoded, ENTRY_OVERHEAD, InputTreeConfig, Walked, decode};
pub use crate::meta_log::{Leadership, MemoryMetaLog, MetaLog, MetaLogError};
pub use crate::segments::{
    Connection, Connections, MAX_RANGE_BYTES, MAX_UPLOAD_RECORDS, MIN_SEGMENT_UPLOAD_BYTES,
    SegmentsConfig, SegmentsService, Tracked,
};

/// The most blob data one `BatchUpdateBlobs` or `BatchReadBlobs` call may carry,
/// advertised in the capabilities. Larger blobs go through ByteStream. A request is
/// decoded whole before its handler runs, so one stream may hold up to
/// [`MAX_MESSAGE_BYTES`] (17 MiB) of decode buffer: the decode memory is that times the
/// streams a connection may open (`--reapi-max-streams`, `--worker-max-streams`), about
/// 3.4 GiB for 200, which `--write-bytes-in-flight` does not cover
/// (`docs/design/write-path.md`).
pub const MAX_BATCH_TOTAL_BYTES: usize = 16 << 20;

/// The largest gRPC message the cache services accept or send: a full batch plus room
/// for the digests and statuses around it.
pub const MAX_MESSAGE_BYTES: usize = MAX_BATCH_TOTAL_BYTES + (1 << 20);

/// No blob is too large for the cache, on any path, and none is held whole in memory
/// past [`MAX_UNSTAGED_BLOB_BYTES`]: a ByteStream `Write` of a larger blob is handed to
/// the store as it arrives (a multipart upload on S3), a segment upload's large records
/// and the in-process plane's large outputs are written to the write tier as they
/// arrive, the tier's flusher reads a block in pieces, and every read of a blob is
/// served in windows of [`READ_WINDOW_BYTES`]. A digest's size is an `i64`, so no blob
/// is over `i64::MAX` bytes; sizes are added as `u64`, saturating.
///
/// The largest blob a daemon's data plane holds in memory on its way into the cache:
/// 16 MiB. A larger one is never held whole: with a write tier that has room for it, it
/// is written onto the tier as it arrives (a segment upload's range, the in-process
/// plane's stream); without one, or when the tier is full, it is streamed to the object
/// store as it arrives (a ByteStream `Write`, the in-process plane's
/// `Cache::store_streamed_claimed`). A segment upload that would have to hold one in
/// memory until its commit is answered FAILED_PRECONDITION at `Begin`
/// ([`unstaged_refusal`]), and the daemon sends its records by ByteStream instead. No
/// blob is refused for its size. 16 MiB is where the write group already writes a blob
/// as a segment of its own (`--write-own-segment-bytes`), four PutRange messages, and
/// well under the 32 MiB upload a daemon batches small files into, so small outputs
/// keep the in-memory path unchanged.
pub const MAX_UNSTAGED_BLOB_BYTES: u64 = 16 << 20;

/// Why a segment upload holding the blob `blob` (its digest), over
/// [`MAX_UNSTAGED_BLOB_BYTES`], is not taken: no write tier has room for it, and it is
/// not held in memory instead. The blob itself is stored by a ByteStream `Write`.
#[must_use]
pub fn unstaged_refusal(blob: &str) -> String {
    format!(
        "{blob} is over {MAX_UNSTAGED_BLOB_BYTES} bytes and no write tier has room for it: \
         send it by a ByteStream Write, which streams it to the object store"
    )
}

/// The answer of a [`Storage`] call.
pub type Answer<'a, T> = BoxFuture<'a, Result<T, StorageError>>;

/// An action-cache hit as [`Storage::action_entry`] answers it.
#[derive(Clone, Debug)]
pub struct ActionEntry {
    /// The digest of the blob the result was read from.
    pub result_digest: Digest,
    /// The result.
    pub result: ActionResult,
    /// The blobs the entry needs besides the result blob. `None`: the storage did not
    /// report them (a node from before the field existed), so they are unchecked; `Some`
    /// of an empty list is an entry that needs nothing else.
    pub closure: Option<Vec<Digest>>,
}

/// The bytes of a ranged read ([`Storage::read_range`]), in order, in chunks of at most
/// [`READ_CHUNK_BYTES`]. An item is an error if the read failed after the answer began;
/// the stream ends after one.
pub type ReadStream = BoxStream<'static, Result<Bytes, StorageError>>;

/// The largest chunk of a [`ReadStream`]: what a ByteStream `Read` sends in one message.
pub const READ_CHUNK_BYTES: usize = 1 << 20;

/// The storage service: CAS, action cache, FindMissing and `Segments` over one store.
pub trait Storage: Send + Sync {
    /// The digests among `digests` the store does not hold present and reachable, in
    /// the order asked, as `FindMissingBlobs` answers; the held ones are touched. The
    /// empty blob is never missing.
    ///
    /// # Errors
    /// The store could not answer; never a partial answer.
    fn find_missing<'a>(&'a self, digests: &'a [Digest]) -> Answer<'a, Vec<Digest>>;

    /// A whole blob, checked against its digest, after touching it: for a blob the
    /// caller decodes. A blob of any size is read with [`Storage::read_range`], a window
    /// at a time.
    ///
    /// # Errors
    /// [`StorageError::NotFound`] only if the blob is not held;
    /// [`StorageError::Unreachable`] if it is held but cannot be read;
    /// [`StorageError::Invalid`] for a blob too large to read whole.
    fn read_blob<'a>(&'a self, digest: &'a Digest) -> Answer<'a, Bytes>;

    /// The bytes `[offset, offset + limit)` of a blob (to its end when `limit` is 0, the
    /// ByteStream `read_limit`), as a stream of chunks of at most [`READ_CHUNK_BYTES`],
    /// after touching it. `offset` equal to the blob's size is an empty stream. A caller
    /// that lost its connection reads on from the bytes it has: `offset` is where it
    /// stopped.
    ///
    /// **What is verified.** The blob is checked, as a whole, against its digest, whatever
    /// the range: a stream that ends cleanly delivered bytes of a blob that hashed to its
    /// digest, and one that fails (a blob that does not hash to it
    /// is [`StorageError::Unreachable`]) ends in an error item, never in a short or wrong
    /// end. A caller discards what it received from a stream that ended in an error. An
    /// implementation reads and hashes the blob in windows and holds one window at a
    /// time, whatever the blob's size, handing over the last bytes only once the whole
    /// blob has checked.
    ///
    /// The errors that are known by the first chunk (the first window is read before the
    /// answer, so a blob that is gone, or a one-window blob that is damaged, is among them)
    /// are in the answer, before it, where the code is still open to the caller. An
    /// `offset` past the end is decided from the digest's size alone, before the store is
    /// asked.
    ///
    /// # Errors
    /// [`StorageError::OutOfRange`] if `offset` is past the end of the blob;
    /// [`StorageError::NotFound`] only if the blob is not held;
    /// [`StorageError::Unreachable`] if it is held but cannot be read; the store failed.
    fn read_range<'a>(
        &'a self,
        digest: &'a Digest,
        offset: u64,
        limit: u64,
    ) -> Answer<'a, ReadStream>;

    /// Hashes `bytes` once, stores them under that digest within the store's write
    /// admission (waiting for room, never refusing for it), and answers the digest once
    /// they are durable.
    ///
    /// # Errors
    /// [`StorageError::Invalid`] for a blob over the largest the store accepts; the
    /// store failed.
    fn put(&self, bytes: Bytes) -> Answer<'_, Digest>;

    /// [`Storage::put`] of a copy the caller needs on this node although the map does not
    /// place the blob here: the blobs of an action-cache entry, which the node checks
    /// against what it holds itself. A node that serves a chain stores the blob as one
    /// copy (nothing is forwarded, nobody owes a replica) where [`Storage::put`] would be
    /// refused as not the primary; a node that is one of the blob's write owners treats it
    /// as [`Storage::put`]. The default is [`Storage::put`].
    ///
    /// # Errors
    /// As [`Storage::put`].
    fn put_unowned(&self, bytes: Bytes) -> Answer<'_, Digest> {
        self.put(bytes)
    }

    /// A stream for one blob of `size` bytes, written as its bytes arrive and never
    /// held whole in memory: [`PutStream::append`] each chunk, [`PutStream::finish`]
    /// with the digest and CRC-32C, then [`StreamedPut::commit`]. `None` only when the
    /// store cannot stream at all (`size` is 0, or it has nowhere to stream to): the
    /// caller uses [`Storage::put`]. A store with a write tier but no room on it still
    /// streams, to its object store. Blocking: call it off the async runtime.
    fn stream(&self, size: u64) -> Option<Box<dyn PutStream>>;

    /// Stores a blob of `size` bytes straight to the object store as `bytes` come (a
    /// multipart upload on S3), holding a piece of it at a time whatever its size: how a
    /// large blob is stored when [`Storage::stream`] has no tier room for it. `claim`
    /// names the blob's digest once its bytes ended (they must hash to it, or nothing is
    /// stored), or `None` to withdraw it: the upload is aborted and this answers `None`.
    /// Answers the digest once the blob is present.
    ///
    /// The default refuses: a storage that cannot take a streamed put (a remote one, which
    /// writes a blob through [`Storage::put`]) says so rather than gathering the blob.
    ///
    /// # Errors
    /// [`StorageError::DigestMismatch`] if the bytes do not hash to the claimed digest;
    /// [`StorageError::Invalid`] if the storage takes no streamed put; a piece of `bytes`
    /// failed; the store failed.
    fn put_streamed(
        &self,
        size: u64,
        bytes: BoxStream<'static, std::io::Result<Bytes>>,
        claim: BoxFuture<'static, Option<Digest>>,
    ) -> Answer<'_, Option<Digest>> {
        let _ = (size, bytes, claim);
        Box::pin(async {
            Err(StorageError::Invalid(
                "this storage takes no streamed put".to_owned(),
            ))
        })
    }

    /// A `Segments` service of its own over the store, with uploads no other caller
    /// can name: the handlers a remote daemon's `Begin`, `PutRange` and `Commit` reach.
    fn segments(&self) -> Arc<dyn Segments>;

    /// `GetActionResult`: the result and the digest of the blob it was read from, if
    /// the entry passes the closure check and its result blob reads back; `None` (a
    /// miss) otherwise.
    ///
    /// # Errors
    /// The store could not answer.
    fn action_result<'a>(
        &'a self,
        action: &'a Digest,
    ) -> Answer<'a, Option<(Digest, ActionResult)>>;

    /// [`Storage::action_result`] with the entry's closure: the blobs the entry needs
    /// besides its result blob, so a caller can check each is still held where it
    /// belongs. The default answers [`Storage::action_result`] with `closure: None`
    /// ("not reported"), which a caller serves as it always did.
    ///
    /// # Errors
    /// The store could not answer.
    fn action_entry<'a>(&'a self, action: &'a Digest) -> Answer<'a, Option<ActionEntry>> {
        Box::pin(async move {
            Ok(self
                .action_result(action)
                .await?
                .map(|(result_digest, result)| ActionEntry {
                    result_digest,
                    result,
                    closure: None,
                }))
        })
    }

    /// The first half of an action-cache write: the closure of `result`, every blob of
    /// it checked held, and `result` stored as a blob. The action cache is not touched.
    ///
    /// # Errors
    /// [`StorageError::ActionWrite`] naming every blob of the closure not held; a
    /// malformed result; the store failed.
    fn prepare_action_result<'a>(&'a self, result: &'a ActionResult) -> Answer<'a, ActionRecord>;

    /// The second half: commits the entry for `action`. Only [`Role::Daemon`] may
    /// write, and the store checks again that every blob the entry needs is held.
    ///
    /// # Errors
    /// [`StorageError::ActionWrite`] if refused; the store failed.
    fn commit_action_record(
        &self,
        role: Role,
        action: Digest,
        record: ActionRecord,
    ) -> Answer<'_, ()>;
}

/// One blob being written as its bytes arrive ([`Storage::stream`]). Dropped before it
/// is committed, nothing is stored.
pub trait PutStream: Send {
    /// Writes `chunk` after the bytes written so far. Blocking.
    ///
    /// # Errors
    /// More bytes than the stream was made for, or the store's write failed.
    fn append(&mut self, chunk: &[u8]) -> Result<(), StorageError>;

    /// Ends the stream as the blob `digest`, whose bytes have CRC-32C `crc32c`.
    ///
    /// # Errors
    /// The bytes written are not as many as `digest` names, their CRC-32C is not
    /// `crc32c`, or they do not hash to `digest` ([`StorageError::DigestMismatch`]):
    /// nothing is stored.
    fn finish(
        self: Box<Self>,
        digest: Digest,
        crc32c: u32,
    ) -> Result<Box<dyn StreamedPut>, StorageError>;
}

/// A streamed blob whose bytes are written and checked, not yet committed.
pub trait StreamedPut: Send {
    /// Makes the blob durable and present: present once this answers.
    ///
    /// # Errors
    /// The store's sync or its index commit failed.
    fn commit(self: Box<Self>) -> Answer<'static, ()>;
}

/// Why a [`Storage`] call failed. [`Status`] has a `From` for it, so a service answers
/// with the right gRPC code. Cloned, so one failure can answer several callers.
#[derive(Clone, Debug, thiserror::Error)]
pub enum StorageError {
    /// The blob is not held. The only error that becomes NOT_FOUND.
    #[error("blob {0} is not in the cache")]
    NotFound(Digest),
    /// The blob is held, but its object cannot be read. It is reported missing until a
    /// write heals it.
    #[error("blob {0} is held but its object cannot be read")]
    Unreachable(Digest),
    /// Bytes do not hash to the digest they were sent under.
    #[error("blob sent as {claimed} hashes to {actual}")]
    DigestMismatch {
        /// The digest the caller named.
        claimed: Digest,
        /// The digest of the bytes it sent.
        actual: Digest,
    },
    /// Bytes read for a blob do not hash to the digest that names it: a node served the
    /// wrong bytes (or too many, or too few). Never the caller's fault, and not a reason to
    /// believe the node: the router goes on to another owner.
    #[error("blob {digest} came back as bytes that hash to {actual}")]
    Corrupt {
        /// The blob asked for.
        digest: Digest,
        /// The digest of the bytes that came back: of all of them, or, for a node that
        /// sent more than `digest` names, of the first byte past its size and those before.
        actual: Digest,
    },
    /// A read starting past the end of the blob. ByteStream answers it OUT_OF_RANGE (a
    /// `read_offset` greater than the resource's size), which is not a malformed request
    /// to be fixed but one a caller may make on purpose.
    #[error("read_offset {offset} is past the end of {digest}")]
    OutOfRange {
        /// The blob.
        digest: Digest,
        /// The offset asked for.
        offset: u64,
    },
    /// A malformed request or stored message, or a blob over the largest the store
    /// accepts.
    #[error("{0}")]
    Invalid(String),
    /// An action-cache write was refused.
    #[error(transparent)]
    ActionWrite(#[from] ActionWriteError),
    /// The store could not answer: its metadata, its objects, its write tier. The same
    /// call may succeed later; never turned into "absent". What fails the same way each
    /// time (a stored object that does not parse, a key that cannot be built) is
    /// [`StorageError::Internal`] instead.
    #[error(transparent)]
    Unavailable(Arc<dyn std::error::Error + Send + Sync>),
    /// The caller's map is older than the node's (`docs/design/architecture-v2.md`,
    /// section 6, the staleness protocol): the caller refetches the map and tries again.
    /// `current` is the node's generation. On the wire it is FAILED_PRECONDITION with the
    /// generation in the [`service::MAP_GENERATION_HEADER`] metadata entry.
    #[error("STALE_MAP: the node is at map generation {current}")]
    StaleMap {
        /// The map generation the node holds.
        current: u64,
    },
    /// The store's own records disagree. A bug or a broken backend, never the caller's
    /// fault.
    #[error("internal: {0}")]
    Internal(String),
}

impl From<StorageError> for Status {
    fn from(e: StorageError) -> Self {
        let message = e.to_string();
        match e {
            StorageError::NotFound(_) => Self::not_found(message),
            StorageError::OutOfRange { .. } => Self::out_of_range(message),
            StorageError::DigestMismatch { .. } | StorageError::Invalid(_) => {
                Self::invalid_argument(message)
            }
            StorageError::ActionWrite(ActionWriteError::NotDaemon) => {
                Self::permission_denied(message)
            }
            StorageError::ActionWrite(ActionWriteError::Absent(_)) => {
                Self::failed_precondition(message)
            }
            StorageError::ActionWrite(ActionWriteError::Unreachable(_))
            | StorageError::Unreachable(_)
            | StorageError::Unavailable(_) => Self::unavailable(message),
            StorageError::StaleMap { current } => {
                let mut status = Self::failed_precondition(message);
                status
                    .metadata_mut()
                    .insert(service::MAP_GENERATION_HEADER, MetadataValue::from(current));
                status
            }
            StorageError::Corrupt { .. } => Self::data_loss(message),
            StorageError::Internal(_) => Self::internal(message),
        }
    }
}

#[cfg(test)]
mod tests;
