//! Every call is authorized by the frontend's authorizers, as a proxy's are.

use futures::future::BoxFuture;
use kbf_auth::{AuthenticationMetadata, Authorizer, Authorizers};
use kbf_front::PackSettings;
use kbf_proto::google::bytestream::byte_stream_client::ByteStreamClient;
use kbf_proto::google::bytestream::{QueryWriteStatusRequest, ReadRequest, WriteRequest};
use kbf_proto::reapi::action_cache_client::ActionCacheClient;
use kbf_proto::reapi::content_addressable_storage_client::ContentAddressableStorageClient;
use kbf_proto::reapi::{
    BatchReadBlobsRequest, BatchUpdateBlobsRequest, FindMissingBlobsRequest,
    GetActionResultRequest, GetTreeRequest, RegisterChunkMappingRequest, SpliceBlobRequest,
    SplitBlobRequest,
};
use std::sync::Arc;
use tonic::{Code, Status};

use crate::bytestream::{read_name, upload_name};
use crate::rig::{Rig, blob_of};
use crate::support::Spy;

struct Deny;

impl Authorizer for Deny {
    fn authorize<'a>(
        &'a self,
        _: &'a AuthenticationMetadata,
        instance_names: &'a [&'a str],
    ) -> BoxFuture<'a, Vec<Result<(), Status>>> {
        let refused = instance_names
            .iter()
            .map(|_| Err(Status::permission_denied("denied")))
            .collect();
        Box::pin(futures::future::ready(refused))
    }
}

/// Asserts that when the authorizers refuse, every call of the three services is
/// PERMISSION_DENIED.
#[tokio::test]
async fn every_call_is_authorized() {
    let deny: Arc<dyn Authorizer> = Arc::new(Deny);
    let authorizers = Authorizers {
        cas_get: Arc::clone(&deny),
        cas_put: Arc::clone(&deny),
        cas_find_missing: Arc::clone(&deny),
        ac_get: deny,
        ..Authorizers::allow_all()
    };
    let rig = Rig::over(Spy::new(), PackSettings::default(), authorizers).await;
    let (digest, _) = blob_of(b"x");
    let (name, upload) = (read_name(&digest), upload_name(&digest));
    let mut cas = ContentAddressableStorageClient::new(rig.front.clone());
    let mut bytestream = ByteStreamClient::new(rig.front.clone());

    let refused = vec![
        cas.find_missing_blobs(FindMissingBlobsRequest::default())
            .await
            .err(),
        cas.batch_update_blobs(BatchUpdateBlobsRequest::default())
            .await
            .err(),
        cas.batch_read_blobs(BatchReadBlobsRequest::default())
            .await
            .err(),
        cas.get_tree(GetTreeRequest::default()).await.err(),
        cas.split_blob(SplitBlobRequest::default()).await.err(),
        cas.splice_blob(SpliceBlobRequest::default()).await.err(),
        cas.get_chunk_mapping(kbf_proto::reapi::GetChunkMappingRequest::default())
            .await
            .err(),
        cas.register_chunk_mapping(futures::stream::iter([
            RegisterChunkMappingRequest::default(),
        ]))
        .await
        .err(),
        bytestream
            .read(ReadRequest {
                resource_name: name,
                ..ReadRequest::default()
            })
            .await
            .err(),
        bytestream
            .write(futures::stream::iter([WriteRequest {
                resource_name: upload.clone(),
                finish_write: true,
                ..WriteRequest::default()
            }]))
            .await
            .err(),
        bytestream
            .query_write_status(QueryWriteStatusRequest {
                resource_name: upload,
            })
            .await
            .err(),
        ActionCacheClient::new(rig.front.clone())
            .get_action_result(GetActionResultRequest::default())
            .await
            .err(),
    ];

    for refusal in refused {
        assert_eq!(refusal.expect("refused").code(), Code::PermissionDenied);
    }
}
