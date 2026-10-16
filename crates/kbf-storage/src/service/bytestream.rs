//! `ByteStream`: a ranged, streamed Read and a streamed Write over [`crate::Storage`].

use std::sync::{Arc, Mutex, PoisonError};

use bytes::Bytes;
use futures::StreamExt as _;
use futures::stream::BoxStream;
use kbf_proto::google::bytestream::byte_stream_server::ByteStream;
use kbf_proto::google::bytestream::{
    QueryWriteStatusRequest, QueryWriteStatusResponse, ReadRequest, ReadResponse, WriteRequest,
    WriteResponse,
};
use tonic::{Request, Response, Status, Streaming};

use super::{Endpoint, STREAM_FROM_BYTES, StorageService, wire};
use crate::StorageError;

#[tonic::async_trait]
impl ByteStream for StorageService {
    type ReadStream = BoxStream<'static, Result<ReadResponse, Status>>;

    /// `[read_offset, read_offset + read_limit)` of the blob (to its end when the limit is
    /// 0), in the chunks [`crate::Storage::read_range`] makes, none of them collected:
    /// the blob is verified before its first byte is sent (that call's rule), and every
    /// error that can be known then is the call's status.
    async fn read(
        &self,
        request: Request<ReadRequest>,
    ) -> Result<Response<Self::ReadStream>, Status> {
        self.admission.check(&request, Endpoint::Read)?;
        let request = request.into_inner();
        let digest = wire::read_resource(&request.resource_name)?;
        let offset = u64::try_from(request.read_offset).map_err(|_| {
            Status::out_of_range(format!("read_offset {} is negative", request.read_offset))
        })?;
        let limit = u64::try_from(request.read_limit).map_err(|_| {
            Status::invalid_argument(format!("read_limit {} is negative", request.read_limit))
        })?;
        let chunks = self.storage.read_range(&digest, offset, limit).await?;
        let messages = chunks.map(|chunk| match chunk {
            Ok(data) => Ok(ReadResponse {
                data: data.to_vec(),
            }),
            Err(e) => Err(Status::from(e)),
        });
        Ok(Response::new(messages.boxed()))
    }

    /// Receives a blob from offset 0 and stores it, hashed against the digest in the
    /// resource name, answering with its size only once it is stored. A blob already held
    /// is answered at once, unread. No blob is refused for its size.
    ///
    /// A blob of at most [`STREAM_FROM_BYTES`] is held whole until it is complete, then
    /// stored with [`crate::Storage::put`], which hashes it. A larger one is never held:
    /// its messages go to [`crate::Storage::put_streamed`] as they arrive, which hashes
    /// them on the way and stores nothing unless they hash to the digest. Room for what
    /// is held (at most [`STREAM_FROM_BYTES`] of it) is taken first
    /// ([`super::Config::write_bytes_in_flight`]). Order is not checked message by
    /// message: the blob is checked against its digest as a whole.
    async fn write(
        &self,
        request: Request<Streaming<WriteRequest>>,
    ) -> Result<Response<WriteResponse>, Status> {
        self.admission.check(&request, Endpoint::Write)?;
        let mut stream = request.into_inner();
        let mut message = stream
            .message()
            .await?
            .ok_or_else(|| Status::invalid_argument("write stream sent no message"))?;
        let digest = wire::write_resource(&message.resource_name)?;
        let size = digest.size_bytes;
        let committed = || {
            Response::new(WriteResponse {
                committed_size: i64::try_from(size).unwrap_or(i64::MAX),
            })
        };
        if self.storage.find_missing(&[digest]).await?.is_empty() {
            return Ok(committed());
        }
        let held = size.min(STREAM_FROM_BYTES);
        let room =
            u32::try_from(held).map_or(self.write_room_bytes, |s| s.min(self.write_room_bytes));
        // The semaphore is never closed.
        let _room = self.write_room.acquire_many(room).await.ok();
        if size > STREAM_FROM_BYTES {
            let fault = Arc::new(Mutex::new(None));
            let body = pieces(stream, message, size, Arc::clone(&fault));
            let claim = Box::pin(futures::future::ready(Some(digest)));
            let stored = self.storage.put_streamed(size, body, claim).await;
            // A fault of the sender's is INVALID_ARGUMENT, not the store's.
            if let Some(status) = fault.lock().unwrap_or_else(PoisonError::into_inner).take() {
                return Err(status);
            }
            stored?;
            return Ok(committed());
        }
        let mut received = Vec::with_capacity(usize::try_from(size).unwrap_or_default());
        loop {
            if (received.len() + message.data.len()) as u64 > size {
                return Err(Status::invalid_argument(format!(
                    "more than the {size} bytes {digest} names"
                )));
            }
            received.extend_from_slice(&message.data);
            if message.finish_write {
                break;
            }
            message = stream.message().await?.ok_or_else(|| {
                Status::invalid_argument("write stream ended without finish_write")
            })?;
        }
        let stored = self.storage.put(received.into()).await?;
        if stored != digest {
            return Err(StorageError::DigestMismatch {
                claimed: digest,
                actual: stored,
            }
            .into());
        }
        Ok(committed())
    }

    /// The blob's full size and `complete` once it is held; 0 and incomplete before.
    async fn query_write_status(
        &self,
        request: Request<QueryWriteStatusRequest>,
    ) -> Result<Response<QueryWriteStatusResponse>, Status> {
        self.admission.check(&request, Endpoint::QueryWriteStatus)?;
        let digest = wire::write_resource(&request.into_inner().resource_name)?;
        let complete = self.storage.find_missing(&[digest]).await?.is_empty();
        let committed_size = if complete {
            i64::try_from(digest.size_bytes).unwrap_or(i64::MAX)
        } else {
            0
        };
        Ok(Response::new(QueryWriteStatusResponse {
            committed_size,
            complete,
        }))
    }
}

/// The bytes of a `Write` of a blob of `size` bytes, message by message as they arrive,
/// starting with `first`: the stream [`crate::Storage::put_streamed`] takes. A message
/// stream that is wrong (more bytes than `size`, fewer at `finish_write`, ended without
/// it, or broken) fails the stream with the sender's fault noted in `fault`.
fn pieces(
    stream: Streaming<WriteRequest>,
    first: WriteRequest,
    size: u64,
    fault: Arc<Mutex<Option<Status>>>,
) -> BoxStream<'static, std::io::Result<Bytes>> {
    futures::stream::unfold(
        Incoming {
            stream,
            first: Some(first),
            got: 0,
            size,
            done: false,
            fault,
        },
        Incoming::step,
    )
    .boxed()
}

/// The state of [`pieces`].
struct Incoming {
    stream: Streaming<WriteRequest>,
    first: Option<WriteRequest>,
    got: u64,
    size: u64,
    done: bool,
    fault: Arc<Mutex<Option<Status>>>,
}

impl Incoming {
    /// The next message's bytes, or `None` after the last.
    async fn step(mut self) -> Option<(std::io::Result<Bytes>, Self)> {
        loop {
            if self.done {
                return None;
            }
            let message = match self.first.take() {
                Some(message) => message,
                None => match self.stream.message().await {
                    Ok(Some(message)) => message,
                    Ok(None) => {
                        let status =
                            Status::invalid_argument("write stream ended without finish_write");
                        return Some(self.refuse(status));
                    }
                    Err(status) => return Some(self.refuse(status)),
                },
            };
            self.got += message.data.len() as u64;
            if self.got > self.size {
                let status = Status::invalid_argument(format!(
                    "more than the {} bytes the resource names",
                    self.size
                ));
                return Some(self.refuse(status));
            }
            if message.finish_write {
                self.done = true;
                if self.got < self.size {
                    let status = Status::invalid_argument(format!(
                        "{} bytes, fewer than the {} the resource names",
                        self.got, self.size
                    ));
                    return Some(self.refuse(status));
                }
            }
            if !message.data.is_empty() {
                return Some((Ok(Bytes::from(message.data)), self));
            }
        }
    }

    /// Ends the stream in an error, noting `status` as the sender's fault.
    fn refuse(mut self, status: Status) -> (std::io::Result<Bytes>, Self) {
        let error = std::io::Error::other(status.message().to_owned());
        *self.fault.lock().unwrap_or_else(PoisonError::into_inner) = Some(status);
        self.done = true;
        (Err(error), self)
    }
}
