//! `ByteStream`: a ranged, streamed Read and a streamed Write over [`crate::Storage`].

use futures::StreamExt as _;
use futures::stream::BoxStream;
use kbf_proto::google::bytestream::byte_stream_server::ByteStream;
use kbf_proto::google::bytestream::{
    QueryWriteStatusRequest, QueryWriteStatusResponse, ReadRequest, ReadResponse, WriteRequest,
    WriteResponse,
};
use tonic::{Request, Response, Status, Streaming};

use super::{Endpoint, MAX_BLOB_BYTES, StorageService, wire};
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

    /// Receives a blob from offset 0, stores it with [`crate::Storage::put`], which
    /// hashes it, and only then answers with its size. A blob already held is answered
    /// at once, unread.
    ///
    /// The bytes are held whole until they are complete, never more than the digest in
    /// the resource name declares (and never a blob over [`MAX_BLOB_BYTES`]), and room
    /// for them is taken first ([`super::Config::write_bytes_in_flight`]). Order is not
    /// checked message by message: the blob is checked against its digest as a whole.
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
        if size > MAX_BLOB_BYTES {
            return Err(Status::invalid_argument(format!(
                "{digest} is larger than the largest blob this node accepts ({MAX_BLOB_BYTES} \
                 bytes)"
            )));
        }
        if self.storage.find_missing(&[digest]).await?.is_empty() {
            return Ok(committed());
        }
        let room =
            u32::try_from(size).map_or(self.write_room_bytes, |s| s.min(self.write_room_bytes));
        // The semaphore is never closed.
        let _room = self.write_room.acquire_many(room).await.ok();
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
