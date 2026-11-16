//! `ContentAddressableStorage` over [`Stored`]: FindMissingBlobs, BatchUpdateBlobs and
//! BatchReadBlobs from the packs and nothing else. A blob over the packs' bound is not in a
//! batch (a batch is at most [`MAX_BATCH_TOTAL_BYTES`]); it is ByteStream's.

use std::sync::Arc;

use futures::StreamExt as _;
use futures::stream;
use kbf_auth::{Authorizer, Authorizers, authorize};
use kbf_proto::reapi::content_addressable_storage_server::ContentAddressableStorage;
use kbf_proto::reapi::{
    self, BatchReadBlobsRequest, BatchReadBlobsResponse, BatchUpdateBlobsRequest,
    BatchUpdateBlobsResponse, FindMissingBlobsRequest, FindMissingBlobsResponse,
    GetChunkMappingRequest, GetChunkMappingResponse, GetTreeRequest, GetTreeResponse,
    RegisterChunkMappingRequest, RegisterChunkMappingResponse, SpliceBlobRequest,
    SpliceBlobResponse, SplitBlobRequest, SplitBlobResponse, batch_read_blobs_response,
};
use tonic::{Request, Response, Status, Streaming};

use super::Stored;
use crate::proxy::packed;
use crate::{MAX_BATCH_TOTAL_BYTES, wire};

macro_rules! cas_call {
    ($method:literal) => {
        concat!(
            "/build.bazel.remote.execution.v2.ContentAddressableStorage/",
            $method
        )
    };
}

/// The `ContentAddressableStorage` service of a frontend with no storage node.
pub(super) struct StoredCas {
    pub stored: Arc<Stored>,
    pub authorizers: Arc<Authorizers>,
}

impl StoredCas {
    async fn allow(
        authorizer: &Arc<dyn Authorizer>,
        call: &'static str,
        caller: &kbf_auth::AuthenticationMetadata,
        instance: &str,
    ) -> Result<(), Status> {
        authorize(&**authorizer, caller, call, instance).await
    }

    fn unimplemented<T>(call: &str) -> Result<T, Status> {
        Err(Status::unimplemented(format!(
            "{call} is not served by a frontend with no storage node"
        )))
    }
}

type Boxed<T> = futures::stream::BoxStream<'static, Result<T, Status>>;

#[tonic::async_trait]
impl ContentAddressableStorage for StoredCas {
    /// The digests the pack index has no row for, in the order asked.
    async fn find_missing_blobs(
        &self,
        request: Request<FindMissingBlobsRequest>,
    ) -> Result<Response<FindMissingBlobsResponse>, Status> {
        let caller = kbf_auth::metadata(&request);
        let request = request.into_inner();
        let call = cas_call!("FindMissingBlobs");
        let authorizer = &self.authorizers.cas_find_missing;
        Self::allow(authorizer, call, &caller, &request.instance_name).await?;
        wire::check_digest_function(request.digest_function)?;
        let asked = wire::digests(&request.blob_digests)?;
        let missing = self.stored.packs().missing(&asked).await?;
        Ok(Response::new(FindMissingBlobsResponse {
            missing_blob_digests: missing.iter().map(wire::digest_to_proto).collect(),
        }))
    }

    /// Verifies each blob against its digest and packs the ones that pass together; each
    /// answers alone, in request order.
    async fn batch_update_blobs(
        &self,
        request: Request<BatchUpdateBlobsRequest>,
    ) -> Result<Response<BatchUpdateBlobsResponse>, Status> {
        let caller = kbf_auth::metadata(&request);
        let request = request.into_inner();
        let call = cas_call!("BatchUpdateBlobs");
        let authorizer = &self.authorizers.cas_put;
        Self::allow(authorizer, call, &caller, &request.instance_name).await?;
        wire::check_digest_function(request.digest_function)?;
        let total: usize = request.requests.iter().map(|r| r.data.len()).sum();
        if total > MAX_BATCH_TOTAL_BYTES {
            return Err(Status::invalid_argument(format!(
                "batch carries {total} bytes; at most {MAX_BATCH_TOTAL_BYTES} are allowed"
            )));
        }
        let items: Vec<_> = request
            .requests
            .into_iter()
            .map(|r| {
                (
                    r.digest,
                    r.compressor,
                    kbf_storage::in_flight::counted(r.data),
                )
            })
            .collect();
        let responses = packed::store_batch(self.stored.packs(), items).await;
        Ok(Response::new(BatchUpdateBlobsResponse { responses }))
    }

    /// Reads the blobs of the batch from their packs together; each answers alone, in request
    /// order, and a blob the index lacks is NOT_FOUND.
    async fn batch_read_blobs(
        &self,
        request: Request<BatchReadBlobsRequest>,
    ) -> Result<Response<BatchReadBlobsResponse>, Status> {
        let caller = kbf_auth::metadata(&request);
        let request = request.into_inner();
        let call = cas_call!("BatchReadBlobs");
        let authorizer = &self.authorizers.cas_get;
        Self::allow(authorizer, call, &caller, &request.instance_name).await?;
        wire::check_digest_function(request.digest_function)?;
        let total = request
            .digests
            .iter()
            .map(|d| u64::try_from(d.size_bytes).unwrap_or(0))
            .fold(0u64, u64::saturating_add);
        if total > MAX_BATCH_TOTAL_BYTES as u64 {
            return Err(Status::invalid_argument(format!(
                "batch asks for {total} bytes; at most {MAX_BATCH_TOTAL_BYTES} are allowed"
            )));
        }
        let served = packed::read_many(self.stored.packs(), &request.digests).await;
        let responses = stream::iter(request.digests)
            .map(|asked| {
                let read = match wire::digest(Some(&asked)) {
                    Ok(digest) => match served.get(&digest).cloned().unwrap_or(Ok(None)) {
                        Ok(Some(bytes)) => Ok(bytes),
                        Ok(None) => Err(Status::not_found(format!(
                            "blob {digest} is not in the cache"
                        ))),
                        Err(status) => Err(status),
                    },
                    Err(status) => Err(status),
                };
                let (data, status) = match read {
                    Ok(bytes) => (bytes.to_vec(), wire::rpc_ok()),
                    Err(status) => (Vec::new(), wire::rpc_status(&status)),
                };
                batch_read_blobs_response::Response {
                    digest: Some(asked),
                    data,
                    compressor: reapi::compressor::Value::Identity as i32,
                    status: Some(status),
                }
            })
            .collect::<Vec<_>>()
            .await;
        Ok(Response::new(BatchReadBlobsResponse { responses }))
    }

    type GetTreeStream = Boxed<GetTreeResponse>;

    /// UNIMPLEMENTED, once authorized as a read.
    async fn get_tree(
        &self,
        request: Request<GetTreeRequest>,
    ) -> Result<Response<Self::GetTreeStream>, Status> {
        let caller = kbf_auth::metadata(&request);
        let instance = &request.get_ref().instance_name;
        let authorizer = &self.authorizers.cas_get;
        Self::allow(authorizer, cas_call!("GetTree"), &caller, instance).await?;
        Self::unimplemented("GetTree")
    }

    /// UNIMPLEMENTED, once authorized as a read.
    async fn split_blob(
        &self,
        request: Request<SplitBlobRequest>,
    ) -> Result<Response<SplitBlobResponse>, Status> {
        let caller = kbf_auth::metadata(&request);
        let instance = &request.get_ref().instance_name;
        let authorizer = &self.authorizers.cas_get;
        Self::allow(authorizer, cas_call!("SplitBlob"), &caller, instance).await?;
        Self::unimplemented("SplitBlob")
    }

    type GetChunkMappingStream = Boxed<GetChunkMappingResponse>;

    /// UNIMPLEMENTED, once authorized as a read.
    async fn get_chunk_mapping(
        &self,
        request: Request<GetChunkMappingRequest>,
    ) -> Result<Response<Self::GetChunkMappingStream>, Status> {
        let caller = kbf_auth::metadata(&request);
        let instance = &request.get_ref().instance_name;
        let authorizer = &self.authorizers.cas_get;
        Self::allow(authorizer, cas_call!("GetChunkMapping"), &caller, instance).await?;
        Self::unimplemented("GetChunkMapping")
    }

    /// UNIMPLEMENTED, once authorized as a write.
    async fn splice_blob(
        &self,
        request: Request<SpliceBlobRequest>,
    ) -> Result<Response<SpliceBlobResponse>, Status> {
        let caller = kbf_auth::metadata(&request);
        let instance = &request.get_ref().instance_name;
        let authorizer = &self.authorizers.cas_put;
        Self::allow(authorizer, cas_call!("SpliceBlob"), &caller, instance).await?;
        Self::unimplemented("SpliceBlob")
    }

    /// UNIMPLEMENTED, once authorized as a write against the first message's instance.
    async fn register_chunk_mapping(
        &self,
        request: Request<Streaming<RegisterChunkMappingRequest>>,
    ) -> Result<Response<RegisterChunkMappingResponse>, Status> {
        let caller = kbf_auth::metadata(&request);
        let first = request.into_inner().message().await?;
        let instance = first.map(|m| m.instance_name).unwrap_or_default();
        let authorizer = &self.authorizers.cas_put;
        Self::allow(
            authorizer,
            cas_call!("RegisterChunkMapping"),
            &caller,
            &instance,
        )
        .await?;
        Self::unimplemented("RegisterChunkMapping")
    }
}
