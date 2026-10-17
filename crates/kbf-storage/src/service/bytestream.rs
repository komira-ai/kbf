//! `ByteStream`: a ranged, streamed Read and a streamed Write over [`crate::Storage`].

use futures::StreamExt as _;
use futures::stream::BoxStream;
use kbf_proto::google::bytestream::byte_stream_server::ByteStream;
use kbf_proto::google::bytestream::{
    QueryWriteStatusRequest, QueryWriteStatusResponse, ReadRequest, ReadResponse, WriteRequest,
    WriteResponse,
};
use tonic::{Request, Response, Status, Streaming};

use super::{Endpoint, StorageService, generation, wire};

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
        self.check_map(generation::requested(&request)?).await?;
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

    /// Receives a blob from offset 0 and answers with its size once it is stored. A
    /// caller whose map generation is not this node's is refused first
    /// ([`crate::replicate`]); a blob already held is answered at once, unread.
    ///
    /// The bytes are checked against the digest in the resource name, and never more
    /// than it declares are taken. Where the store can write a blob as it arrives
    /// ([`crate::Storage::stream`]) they are written so and held in the few chunks in
    /// flight, whatever the blob's size; the primary of a chain streams each chunk on to
    /// the replica as well and answers only when both hold the blob. Where the store
    /// cannot, a node outside a chain holds the blob whole until it is complete
    /// ([`super::Config::write_bytes_in_flight`]).
    async fn write(
        &self,
        request: Request<Streaming<WriteRequest>>,
    ) -> Result<Response<WriteResponse>, Status> {
        let leg_from = generation::leg(&request)?;
        let endpoint = if leg_from.is_some() {
            Endpoint::WriteLeg
        } else {
            Endpoint::Write
        };
        self.admission.check(&request, endpoint)?;
        let requested = generation::requested(&request)?;
        let mut stream = request.into_inner();
        let size = self.write_in(requested, leg_from, &mut stream).await?;
        Ok(wire::committed(size))
    }

    /// The blob's full size and `complete` once it is held; 0 and incomplete before.
    async fn query_write_status(
        &self,
        request: Request<QueryWriteStatusRequest>,
    ) -> Result<Response<QueryWriteStatusResponse>, Status> {
        self.admission.check(&request, Endpoint::QueryWriteStatus)?;
        self.check_map(generation::requested(&request)?).await?;
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
