//! `ByteStream` over [`Stored`], as a tonic service: nothing is passed on, so nothing is raw.
//!
//! **Read** serves `[read_offset, read_offset + read_limit)` of a blob the index holds, in
//! messages of at most [`READ_CHUNK_BYTES`]. A blob within [`Packs::max_blob_bytes`] is read
//! whole from its pack and hashed; a larger one is read from its object in windows
//! ([`Packs::read_large`]) and, when the read is of the whole blob, hashed on the way with the
//! last window sent only once the digest checked. A blob the index lacks is NOT_FOUND.
//!
//! **Write** receives a blob from offset 0 (nothing resumes), checks every message (the same
//! resource, the next offset, no more bytes than the digest names, `finish_write` to end) and
//! answers with the blob's size once it is stored and recorded. A blob the index already holds
//! is answered at once. A blob within the bound takes its size from the room that uploads share
//! before a byte past the first message is read, is received whole, hashed and packed; a larger
//! one is streamed to its object as its messages arrive ([`Packs::put_large`]). A write that
//! gets no byte for [`Packs::upload_idle_timeout`] is ended DEADLINE_EXCEEDED and stores nothing.
//!
//! **QueryWriteStatus** says a blob the index holds is complete at its size, and otherwise the
//! bytes that have arrived of an upload of that name in flight in this process, or 0.
//!
//! **Compressed** names (`compressed-blobs/`) are INVALID_ARGUMENT, on every call.

use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use futures::{Stream, StreamExt as _, stream};
use kbf_auth::{Authorizers, authorize};
use kbf_proto::google::bytestream::byte_stream_server::ByteStream;
use kbf_proto::google::bytestream::{
    QueryWriteStatusRequest, QueryWriteStatusResponse, ReadRequest, ReadResponse, WriteRequest,
    WriteResponse,
};
use kbf_storage::in_flight;
use kbf_types::Digest;
use tonic::{Request, Response, Status, Streaming};

use super::Stored;
use super::uploads::Upload;
use crate::READ_CHUNK_BYTES;
use crate::proxy::name;
use crate::proxy::{Method, Packs};

/// The `ByteStream` service of a frontend with no storage node. Read is authorized by
/// [`Authorizers::cas_get`], Write and QueryWriteStatus by [`Authorizers::cas_put`].
pub(super) struct StoredByteStream {
    pub stored: Arc<Stored>,
    pub authorizers: Arc<Authorizers>,
}

/// The windows of a read, or its failure.
type Windows = futures::stream::BoxStream<'static, Result<Bytes, Status>>;

/// What a read sends.
type Messages = Pin<Box<dyn Stream<Item = Result<ReadResponse, Status>> + Send>>;

/// The resource a call names, refused if it names compressed bytes.
fn plain(method: Method, resource: &str) -> Result<name::Resource, Status> {
    let named = name::parse(method, resource)?;
    if resource.split('/').any(|part| part == "compressed-blobs") {
        return Err(Status::invalid_argument(format!(
            "{resource:?}: compressed blobs are not served by this frontend yet; \
             send the blob as it is"
        )));
    }
    Ok(named)
}

fn committed(size: u64) -> i64 {
    i64::try_from(size).unwrap_or(i64::MAX)
}

#[tonic::async_trait]
impl ByteStream for StoredByteStream {
    type ReadStream = Messages;

    /// The range of the blob, in messages of at most [`READ_CHUNK_BYTES`].
    async fn read(
        &self,
        request: Request<ReadRequest>,
    ) -> Result<Response<Self::ReadStream>, Status> {
        let caller = kbf_auth::metadata(&request);
        let request = request.into_inner();
        let resource = plain(Method::Read, &request.resource_name)?;
        let authorizer = &*self.authorizers.cas_get;
        authorize(authorizer, &caller, Method::Read.path(), &resource.instance).await?;
        let digest = resource.digest;
        let offset = u64::try_from(request.read_offset).map_err(|_| {
            Status::out_of_range(format!("read_offset {} is negative", request.read_offset))
        })?;
        let limit = u64::try_from(request.read_limit).map_err(|_| {
            Status::invalid_argument(format!("read_limit {} is negative", request.read_limit))
        })?;
        if offset > digest.size_bytes {
            return Err(Status::out_of_range(format!(
                "read_offset {offset} is past the end of {digest}"
            )));
        }
        let end = match limit {
            0 => digest.size_bytes,
            n => digest.size_bytes.min(offset.saturating_add(n)),
        };
        let packs = self.stored.packs();
        let windows: Windows = if digest.size_bytes <= packs.max_blob_bytes() {
            match packs.read(&digest).await? {
                Some(blob) => {
                    let wanted = blob.slice(offset as usize..end as usize);
                    stream::once(futures::future::ready(Ok(wanted))).boxed()
                }
                None => return Err(not_held(&digest)),
            }
        } else {
            match packs.read_large(&digest, offset, end).await? {
                Some(windows) => windows,
                None => return Err(not_held(&digest)),
            }
        };
        Ok(Response::new(chunked(windows)))
    }

    /// Receives a blob from offset 0, stores and records it, and answers with its size.
    async fn write(
        &self,
        request: Request<Streaming<WriteRequest>>,
    ) -> Result<Response<WriteResponse>, Status> {
        let caller = kbf_auth::metadata(&request);
        let mut stream = request.into_inner();
        let first = stream
            .message()
            .await?
            .ok_or_else(|| Status::invalid_argument("write stream sent no message"))?;
        let name = first.resource_name.clone();
        let resource = plain(Method::Write, &name)?;
        let authorizer = &*self.authorizers.cas_put;
        authorize(
            authorizer,
            &caller,
            Method::Write.path(),
            &resource.instance,
        )
        .await?;
        let digest = resource.digest;
        let reply = || {
            Response::new(WriteResponse {
                committed_size: committed(digest.size_bytes),
            })
        };
        let packs = self.stored.packs();
        // The cheapest upload is the one we skip.
        if packs.missing(&[digest]).await?.is_empty() {
            return Ok(reply());
        }
        let upload = self.stored.uploads.begin(&name);
        let receiving = Receiving {
            first: Some(first),
            stream,
            name,
            digest,
            upload,
            idle: packs.upload_idle_timeout(),
            received: 0,
            finished: false,
        };
        if digest.size_bytes <= packs.max_blob_bytes() {
            receiving.whole(packs).await?;
        } else {
            packs.put_large(&digest, || receiving.pieces()).await?;
        }
        Ok(reply())
    }

    /// The blob's size when the index holds it, else the bytes of an upload of that name in
    /// flight here, else 0.
    async fn query_write_status(
        &self,
        request: Request<QueryWriteStatusRequest>,
    ) -> Result<Response<QueryWriteStatusResponse>, Status> {
        let caller = kbf_auth::metadata(&request);
        let request = request.into_inner();
        let resource = plain(Method::QueryWriteStatus, &request.resource_name)?;
        let authorizer = &*self.authorizers.cas_put;
        let call = Method::QueryWriteStatus.path();
        authorize(authorizer, &caller, call, &resource.instance).await?;
        let digest = resource.digest;
        let held = self.stored.packs().missing(&[digest]).await?.is_empty();
        let (committed_size, complete) = if held {
            (committed(digest.size_bytes), true)
        } else {
            let arrived = self.stored.uploads.received(&request.resource_name);
            (committed(arrived.unwrap_or(0)), false)
        };
        Ok(Response::new(QueryWriteStatusResponse {
            committed_size,
            complete,
        }))
    }
}

fn not_held(digest: &Digest) -> Status {
    Status::not_found(format!("blob {digest} is not in the cache"))
}

/// `windows` as messages of at most [`READ_CHUNK_BYTES`], each cut from its window as the stream
/// is polled, so a read holds one window and one message.
fn chunked(windows: Windows) -> Messages {
    let messages = stream::unfold(
        (windows, Bytes::new()),
        |(mut windows, mut window)| async move {
            while window.is_empty() {
                match windows.next().await? {
                    Ok(next) => window = next,
                    Err(status) => {
                        return Some((Err(status), (stream::empty().boxed(), Bytes::new())));
                    }
                }
            }
            let message = window.split_to(window.len().min(READ_CHUNK_BYTES));
            let message = ReadResponse {
                data: message.to_vec(),
            };
            Some((Ok(message), (windows, window)))
        },
    );
    Box::pin(messages)
}

/// An upload being received: the stream of its messages and what has come of it.
struct Receiving {
    first: Option<WriteRequest>,
    stream: Streaming<WriteRequest>,
    name: String,
    digest: Digest,
    upload: Upload,
    idle: Duration,
    received: u64,
    finished: bool,
}

impl Receiving {
    /// The next message that carries bytes of the blob, checked, or `None` once the message
    /// that finishes the write has been taken.
    ///
    /// # Errors
    /// INVALID_ARGUMENT for a message that breaks the rules of an upload or a stream that ends
    /// without `finish_write`; DEADLINE_EXCEEDED when no message came within the idle timeout;
    /// or the client's own error.
    async fn next(&mut self) -> Result<Option<Bytes>, Status> {
        if self.finished {
            return Ok(None);
        }
        let message = match self.first.take() {
            Some(first) => first,
            None => {
                let waiting = tokio::time::timeout(self.idle, self.stream.message());
                let arrived = waiting.await.map_err(|_| {
                    Status::deadline_exceeded(format!(
                        "no byte of {} arrived for {:?}",
                        self.digest, self.idle
                    ))
                })?;
                arrived?.ok_or_else(|| {
                    Status::invalid_argument("write stream ended without finish_write")
                })?
            }
        };
        let size = self.digest.size_bytes;
        if !message.resource_name.is_empty() && message.resource_name != self.name {
            return Err(Status::invalid_argument(format!(
                "write stream changed resource from {:?} to {:?}",
                self.name, message.resource_name
            )));
        }
        if u64::try_from(message.write_offset).ok() != Some(self.received) {
            return Err(Status::invalid_argument(format!(
                "write_offset {} but {} bytes were received; uploads start at 0 and do not resume",
                message.write_offset, self.received
            )));
        }
        let more = message.data.len() as u64;
        if self.received + more > size {
            return Err(Status::invalid_argument(format!(
                "more than the {size} bytes {} names",
                self.digest
            )));
        }
        self.received += more;
        self.upload.add(more);
        self.finished = message.finish_write;
        Ok(Some(Bytes::from(message.data)))
    }

    /// The blob received whole, taking its size from the room that uploads share before it reads
    /// past the first message, then packed.
    async fn whole(mut self, packs: &Packs) -> Result<(), Status> {
        let size = self.digest.size_bytes;
        let room = packs.reserve(size).await?;
        let capacity = usize::try_from(size).unwrap_or_default();
        let mut held = in_flight::hold(0);
        let mut blob: Vec<u8> = Vec::new();
        while let Some(piece) = self.next().await? {
            // The buffer grows with the bytes that arrive, never past the size claimed.
            blob.reserve_exact(piece.len().min(capacity.saturating_sub(blob.len())));
            held.add(blob.capacity() as u64 - held.bytes());
            blob.extend_from_slice(&piece);
        }
        if (blob.len() as u64) != size {
            return Err(Status::invalid_argument(format!(
                "{} bytes were sent for {}, which names {size}",
                blob.len(),
                self.digest
            )));
        }
        let blob = in_flight::counted_with(blob, held);
        packs.put_in(vec![(self.digest, blob)], room).await
    }

    /// The bytes of the blob as a stream of pieces, for a blob that is not held whole.
    fn pieces(self) -> crate::proxy::Pieces {
        stream::unfold(self, |mut receiving| async move {
            match receiving.next().await {
                Ok(Some(piece)) => Some((Ok(piece), receiving)),
                Ok(None) => None,
                Err(status) => Some((Err(status), Receiving::ended(receiving))),
            }
        })
        .boxed()
    }

    /// `self` finished, so that the stream of pieces ends after an error.
    fn ended(mut self) -> Self {
        self.finished = true;
        self
    }
}
