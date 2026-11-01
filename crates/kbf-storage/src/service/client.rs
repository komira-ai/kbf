//! [`RemoteStorage`]: a [`Storage`] over a gRPC connection to a storage node, so a
//! caller keeps holding `Arc<dyn Storage>` whether the cache is in its process or not.
//! This is the storage client the frontend (and, over a map, the daemon) uses.

use std::collections::VecDeque;
use std::iter;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use bytes::Bytes;
use futures::future::BoxFuture;
use futures::{FutureExt as _, Stream, StreamExt as _, TryStreamExt as _, stream};
use kbf_meta::{ActionRecord, ActionWriteError, Closure, Role};
use kbf_proto::google::bytestream::byte_stream_client::ByteStreamClient;
use kbf_proto::google::bytestream::{ReadRequest, WriteRequest};
use kbf_proto::reapi::content_addressable_storage_client::ContentAddressableStorageClient;
use kbf_proto::reapi::{ActionResult, FindMissingBlobsRequest};
use kbf_proto::storage::action_store_client::ActionStoreClient;
use kbf_proto::storage::{
    CommitRequest, GetRequest, PrepareRequest, Record, Refusal, Writer, prepare_response, refusal,
};
use kbf_proto::worker::segments_client::SegmentsClient;
use kbf_proto::worker::segments_server::Segments;
use kbf_proto::worker::{
    BeginRequest, BeginResponse, CommitRequest as SegmentsCommitRequest,
    CommitResponse as SegmentsCommitResponse, LocateRequest, LocateResponse, PutRangeRequest,
    PutRangeResponse, ReadRangeRequest, ReadRangeResponse,
};
use kbf_types::Digest;
use tonic::metadata::MetadataValue;
use tonic::transport::Channel;
use tonic::{Code, Request, Response, Status};

use super::{
    DURABLE_HEADER, DURABLE_ONLY, DURABLE_WAIT, MAP_GENERATION_HEADER, MAX_MESSAGE_BYTES,
    UNOWNED_HEADER, wire,
};
use crate::{
    ActionEntry, Answer, PieceStream, PutStream, ReadStream, Storage, StorageError, in_flight,
};

/// The largest message of a `Write`'s data.
const WRITE_CHUNK_BYTES: usize = 1 << 20;

/// A storage node, called over `channel`. Cheap to clone.
#[derive(Clone, Debug)]
pub struct RemoteStorage {
    channel: Channel,
    /// Shared by every clone (and so by the `Segments` handle), read once per call.
    generation: Arc<AtomicU64>,
    uploads: Arc<AtomicU64>,
}

/// What a storage node said when it failed a call, as an error.
#[derive(Debug, thiserror::Error)]
#[error("storage node: {0}")]
struct Remote(String);

impl RemoteStorage {
    /// The node `channel` reaches, every call tagged with map `generation` (0: no map
    /// seen; a one-member farm has none).
    #[must_use]
    pub fn new(channel: Channel, generation: u64) -> Self {
        Self {
            channel,
            generation: Arc::new(AtomicU64::new(generation)),
            uploads: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Tags every call made from now on with map `generation`, on this handle and on
    /// every clone of it (a map update reaches the one the caller holds). A call already
    /// begun keeps the generation it read.
    pub fn set_map_generation(&self, generation: u64) {
        self.generation.store(generation, Ordering::Relaxed);
    }

    /// [`Storage::put`], or [`Storage::put_unowned`] if `unowned`: hashed here, sent in
    /// messages of at most 1 MiB, and answered once the node holds it.
    fn put_as(&self, bytes: Bytes, unowned: bool) -> Answer<'_, Digest> {
        Box::pin(async move {
            let digest = kbf_segments::sha256(&bytes);
            let upload = self.uploads.fetch_add(1, Ordering::Relaxed);
            let name = format!(
                "uploads/storage-client-{}-{upload}/blobs/{}/{}",
                std::process::id(),
                digest.hash_hex(),
                digest.size_bytes
            );
            let requests = write_messages(name, bytes);
            let mut request = self.tag(stream::iter(requests));
            if unowned {
                request
                    .metadata_mut()
                    .insert(UNOWNED_HEADER, MetadataValue::from_static("1"));
            }
            let response = self.bytestream().write(request).await.map_err(other)?;
            let committed = response.into_inner().committed_size;
            if committed as u64 != digest.size_bytes {
                return Err(StorageError::Internal(format!(
                    "{digest} was acknowledged as {committed} bytes"
                )));
            }
            Ok(digest)
        })
    }

    /// The generation a call made now carries.
    fn generation(&self) -> u64 {
        self.generation.load(Ordering::Relaxed)
    }

    /// `message` as a request carrying the map generation.
    fn tag<T>(&self, message: T) -> Request<T> {
        self.tag_at(message, self.generation())
    }

    /// `message` as a request carrying `generation`: for a call whose message carries it
    /// too, so the field and the header are one read of the generation.
    fn tag_at<T>(&self, message: T, generation: u64) -> Request<T> {
        let mut request = Request::new(message);
        request
            .metadata_mut()
            .insert(MAP_GENERATION_HEADER, MetadataValue::from(generation));
        request
    }

    fn bytestream(&self) -> ByteStreamClient<Channel> {
        ByteStreamClient::new(self.channel.clone())
            .max_decoding_message_size(MAX_MESSAGE_BYTES)
            .max_encoding_message_size(MAX_MESSAGE_BYTES)
    }

    fn segments_client(&self) -> SegmentsClient<Channel> {
        SegmentsClient::new(self.channel.clone())
            .max_decoding_message_size(MAX_MESSAGE_BYTES)
            .max_encoding_message_size(MAX_MESSAGE_BYTES)
    }

    fn actions(&self) -> ActionStoreClient<Channel> {
        ActionStoreClient::new(self.channel.clone())
            .max_decoding_message_size(MAX_MESSAGE_BYTES)
            .max_encoding_message_size(MAX_MESSAGE_BYTES)
    }
}

/// The error a failed call is. The codes a [`Storage`] answers are kept (`at` is the
/// blob and offset of a read, which NOT_FOUND and OUT_OF_RANGE are about): a caller sees
/// what an in-process store would have said. Only UNAVAILABLE, DEADLINE_EXCEEDED and
/// UNKNOWN, which say nothing of the request (a transport failure, a deadline), are
/// `Unavailable`; a store that is held but unreadable is UNAVAILABLE too, so it is
/// `Unavailable` here where it was `Unreachable` there. Every other code is an answer
/// from a node that was reached and declined (ABORTED, DATA_LOSS, RESOURCE_EXHAUSTED,
/// CANCELLED, ALREADY_EXISTS, FAILED_PRECONDITION without a map generation): `Internal`
/// with the code in the message, never an outage, so a primary does not answer a write
/// with one copy over a replica's refusal ([`crate::replicate`]).
pub(crate) fn lift(status: &Status, at: Option<(Digest, u64)>) -> StorageError {
    if let Some(current) = stale_generation(status) {
        return StorageError::StaleMap { current };
    }
    let message = status.message().to_owned();
    match (status.code(), at) {
        (Code::NotFound, Some((digest, _))) => StorageError::NotFound(digest),
        (Code::OutOfRange, Some((digest, offset))) => StorageError::OutOfRange { digest, offset },
        (Code::InvalidArgument | Code::PermissionDenied | Code::Unauthenticated, _) => {
            StorageError::Invalid(message)
        }
        (Code::Internal | Code::Unimplemented | Code::NotFound | Code::OutOfRange, _) => {
            StorageError::Internal(message)
        }
        (Code::Unavailable | Code::DeadlineExceeded | Code::Unknown, _) => {
            StorageError::Unavailable(Arc::new(Remote(message)))
        }
        (code, _) => StorageError::Internal(format!("{code:?}: {message}")),
    }
}

/// The generation a node named when it answered `STALE_MAP`: FAILED_PRECONDITION with the
/// generation in the [`MAP_GENERATION_HEADER`] entry. FAILED_PRECONDITION alone is also a
/// refused action-cache write, so the entry is what says it.
pub fn stale_generation(status: &Status) -> Option<u64> {
    if status.code() != Code::FailedPrecondition {
        return None;
    }
    let entry = status.metadata().get(MAP_GENERATION_HEADER);
    entry
        .and_then(|value| value.to_str().ok())
        .and_then(|text| text.parse().ok())
}

/// [`lift`] for a call that is not about one blob.
fn other(status: Status) -> StorageError {
    lift(&status, None)
}

/// The refusal a node answered, as the store's error.
pub(super) fn refused(refusal: &Refusal) -> StorageError {
    let blob = wire::digest(refusal.blob.as_ref());
    let more: Result<Vec<Digest>, _> = refusal.more.iter().map(|d| wire::digest(Some(d))).collect();
    let lacking = blob.and_then(|b| Ok(iter::once(b).chain(more?).collect::<Vec<_>>()));
    let error = match (refusal::Kind::try_from(refusal.kind), lacking) {
        (Ok(refusal::Kind::NotDaemon), _) => ActionWriteError::NotDaemon,
        (Ok(refusal::Kind::Absent), Ok(lacking)) => ActionWriteError::Absent(lacking),
        (Ok(refusal::Kind::Unreachable), Ok(lacking)) => ActionWriteError::Unreachable(lacking),
        _ => {
            return StorageError::Internal(format!("an unreadable refusal {refusal:?}"));
        }
    };
    error.into()
}

/// The messages of a `Write` of `bytes` to the resource `name`: one per chunk of at most
/// 1 MiB, and an empty blob is one message with no data. Made as they are taken, so a
/// sender holds one chunk's copy at a time, never a second whole blob.
pub(super) fn write_messages(
    name: String,
    bytes: Bytes,
) -> impl Iterator<Item = WriteRequest> + Send + 'static {
    let count = bytes.len().div_ceil(WRITE_CHUNK_BYTES).max(1);
    let mut name = Some(name);
    (0..count).map(move |i| {
        let start = i * WRITE_CHUNK_BYTES;
        WriteRequest {
            resource_name: name.take().unwrap_or_default(),
            write_offset: start as i64,
            finish_write: i + 1 == count,
            data: bytes[start..bytes.len().min(start + WRITE_CHUNK_BYTES)].to_vec(),
        }
    })
}

/// The messages of a `Write` of `size` bytes to the resource `name`, taken from `body` as they are
/// sent: each piece is cut into messages of at most 1 MiB, and the message that completes `size`
/// bytes has `finish_write`. A body that fails ends the messages without `finish_write`, so the
/// node stores nothing, and leaves its error in `failed`.
fn streamed_messages(
    name: String,
    size: u64,
    body: PieceStream,
    failed: Arc<Mutex<Option<std::io::Error>>>,
) -> impl Stream<Item = WriteRequest> + Send + 'static {
    struct Sending {
        body: PieceStream,
        queue: VecDeque<Bytes>,
        name: Option<String>,
        offset: u64,
        failed: Arc<Mutex<Option<std::io::Error>>>,
    }
    let sending = Sending {
        body,
        queue: VecDeque::new(),
        name: Some(name),
        offset: 0,
        failed,
    };
    stream::unfold(sending, move |mut sending| async move {
        loop {
            if let Some(chunk) = sending.queue.pop_front() {
                let at = sending.offset;
                sending.offset += chunk.len() as u64;
                let request = WriteRequest {
                    resource_name: sending.name.take().unwrap_or_default(),
                    write_offset: at as i64,
                    finish_write: sending.offset == size,
                    data: chunk.to_vec(),
                };
                return Some((request, sending));
            }
            match sending.body.next().await {
                Some(Ok(piece)) => {
                    let chunks = piece.len().div_ceil(WRITE_CHUNK_BYTES);
                    for i in 0..chunks {
                        let start = i * WRITE_CHUNK_BYTES;
                        let end = piece.len().min(start + WRITE_CHUNK_BYTES);
                        sending.queue.push_back(piece.slice(start..end));
                    }
                }
                Some(Err(error)) => {
                    let mut failed = sending
                        .failed
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner);
                    *failed = Some(error);
                    return None;
                }
                None => return None,
            }
        }
    })
}

fn record_to_proto(record: &ActionRecord) -> Record {
    Record {
        result: Some(wire::to_proto(&record.result)),
        closure: record.closure.iter().map(wire::to_proto).collect(),
    }
}

/// The most [`RemoteStorage::read_blob`] allocates before the first byte arrives.
const READ_BLOB_PREALLOCATE: u64 = 64 << 20;

impl RemoteStorage {
    /// `FindMissingBlobs` for `digests`, marked [`DURABLE_HEADER`] with `durable` when
    /// there is one.
    async fn ask_missing(
        &self,
        digests: &[Digest],
        durable: Option<&'static str>,
    ) -> Result<Vec<Digest>, StorageError> {
        let request = FindMissingBlobsRequest {
            instance_name: String::new(),
            blob_digests: digests.iter().map(wire::to_proto).collect(),
            digest_function: 0,
        };
        let mut request = self.tag(request);
        if let Some(value) = durable {
            request
                .metadata_mut()
                .insert(DURABLE_HEADER, MetadataValue::from_static(value));
        }
        let mut client = ContentAddressableStorageClient::new(self.channel.clone())
            .max_decoding_message_size(MAX_MESSAGE_BYTES);
        let response = client.find_missing_blobs(request).await.map_err(other)?;
        response
            .into_inner()
            .missing_blob_digests
            .iter()
            .map(|d| wire::digest(Some(d)))
            .collect::<Result<Vec<_>, _>>()
            .map_err(other)
    }
}

impl Storage for RemoteStorage {
    fn find_missing<'a>(&'a self, digests: &'a [Digest]) -> Answer<'a, Vec<Digest>> {
        Box::pin(self.ask_missing(digests, None))
    }

    /// Asks with [`DURABLE_HEADER`]: the node answers from its index alone.
    fn find_durable<'a>(&'a self, digests: &'a [Digest]) -> Answer<'a, Vec<Digest>> {
        Box::pin(self.ask_missing(digests, Some(DURABLE_ONLY)))
    }

    /// Asks with [`DURABLE_HEADER`] `wait`: the node waits, for at most its own bound
    /// (`--durable-wait`), for its held blobs among `digests` to be written, then answers
    /// from its index alone.
    fn find_durable_waiting<'a>(&'a self, digests: &'a [Digest]) -> Answer<'a, Vec<Digest>> {
        Box::pin(self.ask_missing(digests, Some(DURABLE_WAIT)))
    }

    /// The whole blob, collected from the stream of [`Storage::read_range`]; its length
    /// is checked against the digest's size.
    ///
    /// Each chunk is appended to one buffer as it arrives and dropped, so the blob is
    /// held once and a chunk beside it, not twice. Counted (`kbf_bytes_in_flight`) at
    /// the buffer's capacity as it grows, and until the last `Bytes` over it is dropped.
    fn read_blob<'a>(&'a self, digest: &'a Digest) -> Answer<'a, Bytes> {
        Box::pin(async move {
            let mut chunks = Storage::read_range(self, digest, 0, 0).await?;
            // The digest's size, up to a bound: the buffer is one allocation for any blob
            // that fits it, and counted from here whether or not its pages are touched.
            let room = usize::try_from(digest.size_bytes.min(READ_BLOB_PREALLOCATE)).unwrap_or(0);
            let mut blob: Vec<u8> = Vec::with_capacity(room);
            let mut held = in_flight::hold(blob.capacity() as u64);
            while let Some(chunk) = chunks.try_next().await? {
                let chunk = in_flight::counted(chunk);
                blob.extend_from_slice(&chunk);
                // The capacity only grows: what it grew by (nothing, for a blob up to the
                // preallocation) is counted too.
                held.add(blob.capacity() as u64 - held.bytes());
            }
            if blob.len() as u64 != digest.size_bytes {
                return Err(StorageError::Internal(format!(
                    "{digest} came back as {} bytes",
                    blob.len()
                )));
            }
            Ok(in_flight::counted_with(blob, held))
        })
    }

    /// The node verifies the blob as a whole before its first byte (the trait's rule);
    /// this is that stream, a chunk per message.
    fn read_range<'a>(
        &'a self,
        digest: &'a Digest,
        offset: u64,
        limit: u64,
    ) -> Answer<'a, ReadStream> {
        Box::pin(async move {
            let at = Some((*digest, offset));
            let request = ReadRequest {
                resource_name: format!("blobs/{}/{}", digest.hash_hex(), digest.size_bytes),
                read_offset: i64::try_from(offset).unwrap_or(i64::MAX),
                read_limit: i64::try_from(limit).unwrap_or(i64::MAX),
            };
            let response = self
                .bytestream()
                .read(self.tag(request))
                .await
                .map_err(|s| lift(&s, at))?;
            Ok(response
                .into_inner()
                .map(move |message| match message {
                    Ok(read) => Ok(Bytes::from(read.data)),
                    Err(status) => Err(lift(&status, at)),
                })
                .boxed())
        })
    }

    /// Hashed here, sent in messages of at most 1 MiB, and answered once the node holds
    /// it.
    fn put(&self, bytes: Bytes) -> Answer<'_, Digest> {
        self.put_as(bytes, false)
    }

    /// A write marked [`UNOWNED_HEADER`]: the node stores one copy although the map places
    /// the blob elsewhere.
    fn put_unowned(&self, bytes: Bytes) -> Answer<'_, Digest> {
        self.put_as(bytes, true)
    }

    /// Always `None`: a streamed write over the wire is [`Storage::put`]'s messages.
    fn stream(&self, _size: u64) -> Option<Box<dyn PutStream>> {
        None
    }

    /// A blob of `size` bytes sent as `Write` messages of at most 1 MiB taken from `bytes` as they
    /// are sent, so this process never holds it whole. ByteStream names the digest in the
    /// resource name, before the first byte, so `claim` must be ready when this is called (a claim
    /// that is only known once the bytes ended is [`StorageError::Invalid`]); a claim of `None`
    /// is a withdrawn blob, and nothing is sent. The node checks that the bytes hash to the
    /// digest and stores nothing if they do not (that refusal is [`StorageError::Invalid`]).
    ///
    /// # Errors
    /// [`StorageError::Invalid`] for a claim that is not ready, a size that is not the claimed
    /// digest's or 0 (an empty blob is [`Storage::put`]'s), or the node's refusal;
    /// [`StorageError::Unavailable`] for a body that failed, and for a node that could not be
    /// reached.
    fn put_streamed(
        &self,
        size: u64,
        bytes: PieceStream,
        claim: BoxFuture<'static, Option<Digest>>,
    ) -> Answer<'_, Option<Digest>> {
        Box::pin(async move {
            let Some(claimed) = claim.now_or_never() else {
                let message = "a remote streamed put needs the digest before the first byte";
                return Err(StorageError::Invalid(message.to_owned()));
            };
            let Some(digest) = claimed else {
                return Ok(None);
            };
            if size == 0 {
                let message = "an empty blob is not sent as a stream";
                return Err(StorageError::Invalid(message.to_owned()));
            }
            if digest.size_bytes != size {
                let message = format!("{digest} is not {size} bytes");
                return Err(StorageError::Invalid(message));
            }
            let upload = self.uploads.fetch_add(1, Ordering::Relaxed);
            let name = format!(
                "uploads/storage-client-{}-{upload}/blobs/{}/{}",
                std::process::id(),
                digest.hash_hex(),
                digest.size_bytes
            );
            let failed = Arc::new(Mutex::new(None));
            let requests = streamed_messages(name, size, bytes, Arc::clone(&failed));
            let sent = self.bytestream().write(self.tag(requests)).await;
            let body = failed.lock().unwrap_or_else(PoisonError::into_inner).take();
            if let Some(error) = body {
                return Err(StorageError::Unavailable(Arc::new(error)));
            }
            let committed = sent.map_err(other)?.into_inner().committed_size;
            if committed as u64 != size {
                return Err(StorageError::Internal(format!(
                    "{digest} was acknowledged as {committed} bytes"
                )));
            }
            Ok(Some(digest))
        })
    }

    fn segments(&self) -> Arc<dyn Segments> {
        Arc::new(self.clone())
    }

    fn action_result<'a>(
        &'a self,
        action: &'a Digest,
    ) -> Answer<'a, Option<(Digest, ActionResult)>> {
        Box::pin(async move {
            Ok(self
                .action_entry(action)
                .await?
                .map(|entry| (entry.result_digest, entry.result)))
        })
    }

    fn action_entry<'a>(&'a self, action: &'a Digest) -> Answer<'a, Option<ActionEntry>> {
        Box::pin(async move {
            let generation = self.generation();
            let request = GetRequest {
                map_generation: generation,
                action: Some(wire::to_proto(action)),
            };
            let response = self
                .actions()
                .get(self.tag_at(request, generation))
                .await
                .map_err(other)?;
            let Some(entry) = response.into_inner().entry else {
                return Ok(None);
            };
            let result_digest = wire::digest(entry.result_digest.as_ref()).map_err(other)?;
            let result = entry.result.unwrap_or_default();
            // Unset: an older node that does not report the closure, so unchecked.
            let closure = entry
                .closure
                .map(|c| {
                    c.digests
                        .iter()
                        .map(|d| wire::digest(Some(d)))
                        .collect::<Result<Vec<_>, _>>()
                })
                .transpose()
                .map_err(other)?;
            Ok(Some(ActionEntry {
                result_digest,
                result,
                closure,
            }))
        })
    }

    fn prepare_action_result<'a>(&'a self, result: &'a ActionResult) -> Answer<'a, ActionRecord> {
        Box::pin(async move {
            let generation = self.generation();
            let request = PrepareRequest {
                map_generation: generation,
                result: Some(result.clone()),
            };
            let response = self
                .actions()
                .prepare(self.tag_at(request, generation))
                .await
                .map_err(other)?;
            match response.into_inner().outcome {
                Some(prepare_response::Outcome::Record(record)) => Ok(ActionRecord {
                    result: wire::digest(record.result.as_ref()).map_err(other)?,
                    closure: record
                        .closure
                        .iter()
                        .map(|d| wire::digest(Some(d)))
                        .collect::<Result<Closure, _>>()
                        .map_err(other)?,
                }),
                Some(prepare_response::Outcome::Refused(refusal)) => Err(refused(&refusal)),
                None => Err(StorageError::Internal("an empty Prepare answer".to_owned())),
            }
        })
    }

    fn commit_action_record(
        &self,
        role: Role,
        action: Digest,
        record: ActionRecord,
    ) -> Answer<'_, ()> {
        Box::pin(async move {
            let writer = match role {
                Role::Daemon => Writer::Daemon,
                Role::Client => Writer::Client,
            };
            let generation = self.generation();
            let request = CommitRequest {
                map_generation: generation,
                action: Some(wire::to_proto(&action)),
                record: Some(record_to_proto(&record)),
                writer: writer as i32,
            };
            let response = self
                .actions()
                .commit(self.tag_at(request, generation))
                .await
                .map_err(other)?;
            match response.into_inner().refused {
                None => Ok(()),
                Some(refusal) => Err(refused(&refusal)),
            }
        })
    }
}

/// The `Segments` calls of a node, forwarded: what [`Storage::segments`] is for a
/// remote store. The uploads are the node's, each named by the id it answered.
#[tonic::async_trait]
impl Segments for RemoteStorage {
    async fn begin(
        &self,
        request: Request<BeginRequest>,
    ) -> Result<Response<BeginResponse>, Status> {
        let mut client = self.segments_client();
        client.begin(self.tag(request.into_inner())).await
    }

    async fn put_range(
        &self,
        request: Request<PutRangeRequest>,
    ) -> Result<Response<PutRangeResponse>, Status> {
        let mut client = self.segments_client();
        client.put_range(self.tag(request.into_inner())).await
    }

    async fn commit(
        &self,
        request: Request<SegmentsCommitRequest>,
    ) -> Result<Response<SegmentsCommitResponse>, Status> {
        let mut client = self.segments_client();
        client.commit(self.tag(request.into_inner())).await
    }

    async fn locate(
        &self,
        request: Request<LocateRequest>,
    ) -> Result<Response<LocateResponse>, Status> {
        let mut client = self.segments_client();
        client.locate(self.tag(request.into_inner())).await
    }

    async fn read_range(
        &self,
        request: Request<ReadRangeRequest>,
    ) -> Result<Response<ReadRangeResponse>, Status> {
        let mut client = self.segments_client();
        client.read_range(self.tag(request.into_inner())).await
    }
}
