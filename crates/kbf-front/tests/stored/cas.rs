//! FindMissingBlobs, BatchUpdateBlobs and BatchReadBlobs of a frontend with no storage node, and
//! the calls it does not serve.

use bytes::Bytes;
use futures::stream;
use kbf_front::MAX_BATCH_TOTAL_BYTES;
use kbf_proto::reapi::content_addressable_storage_client::ContentAddressableStorageClient;
use kbf_proto::reapi::{
    self, BatchReadBlobsRequest, BatchUpdateBlobsRequest, FindMissingBlobsRequest, GetTreeRequest,
    RegisterChunkMappingRequest, SpliceBlobRequest, SplitBlobRequest, batch_update_blobs_request,
};
use kbf_types::Digest;
use tonic::{Code, Status};

use crate::rig::{Rig, blob_len, blob_of};

fn proto(digest: &Digest) -> reapi::Digest {
    reapi::Digest {
        hash: digest.hash_hex(),
        size_bytes: digest.size_bytes as i64,
    }
}

pub fn item(digest: &Digest, bytes: &Bytes) -> batch_update_blobs_request::Request {
    batch_update_blobs_request::Request {
        digest: Some(proto(digest)),
        data: bytes.to_vec(),
        compressor: reapi::compressor::Value::Identity as i32,
    }
}

/// The status code of each item of a BatchUpdateBlobs of `requests`.
pub async fn update(
    rig: &Rig,
    requests: Vec<batch_update_blobs_request::Request>,
) -> Result<Vec<i32>, Status> {
    let response = ContentAddressableStorageClient::new(rig.front.clone())
        .batch_update_blobs(BatchUpdateBlobsRequest {
            requests,
            ..BatchUpdateBlobsRequest::default()
        })
        .await?;
    let responses = response.into_inner().responses;
    Ok(responses
        .iter()
        .map(|r| r.status.as_ref().map_or(-1, |s| s.code))
        .collect())
}

/// The digests of `asked` the frontend says it lacks.
pub async fn missing(rig: &Rig, asked: &[Digest]) -> Result<Vec<Digest>, Status> {
    let response = ContentAddressableStorageClient::new(rig.front.clone())
        .find_missing_blobs(FindMissingBlobsRequest {
            blob_digests: asked.iter().map(proto).collect(),
            ..FindMissingBlobsRequest::default()
        })
        .await?;
    let lacking = response.into_inner().missing_blob_digests;
    Ok(asked
        .iter()
        .filter(|d| lacking.iter().any(|l| l.hash == d.hash_hex()))
        .copied()
        .collect())
}

/// The status code and bytes of each item of a BatchReadBlobs of `asked`.
pub async fn read(rig: &Rig, asked: &[Digest]) -> Result<Vec<(i32, Vec<u8>)>, Status> {
    let response = ContentAddressableStorageClient::new(rig.front.clone())
        .batch_read_blobs(BatchReadBlobsRequest {
            digests: asked.iter().map(proto).collect(),
            ..BatchReadBlobsRequest::default()
        })
        .await?;
    let responses = response.into_inner().responses;
    Ok(responses
        .into_iter()
        .map(|r| (r.status.map_or(-1, |s| s.code), r.data))
        .collect())
}

const OK: i32 = Code::Ok as i32;

/// Asserts that a blob uploaded in a batch is held and read back as it was, and that the
/// frontend answers for blobs it was never given as missing, with no storage node to ask.
#[tokio::test]
async fn a_batch_upload_is_held_and_read_back() {
    let rig = Rig::new().await;
    let (one, one_bytes) = blob_of(b"one");
    let (two, two_bytes) = blob_of(b"two");
    let (never, _) = blob_of(b"never uploaded");

    let stored = update(&rig, vec![item(&one, &one_bytes), item(&two, &two_bytes)]).await;

    assert_eq!(stored.expect("answered"), vec![OK, OK]);
    let lacking = missing(&rig, &[one, never, two]).await.expect("answered");
    assert_eq!(lacking, vec![never]);
    let read = read(&rig, &[two, one]).await.expect("answered");
    assert_eq!(
        read,
        vec![(OK, two_bytes.to_vec()), (OK, one_bytes.to_vec())]
    );
}

/// Asserts that a blob whose bytes are not its digest is refused INVALID_ARGUMENT alone and is
/// not held, while the other blobs of the batch are stored, and that compressed data is refused
/// the same way.
#[tokio::test]
async fn a_blob_that_is_not_its_digest_is_refused_alone() {
    let rig = Rig::new().await;
    let (good, good_bytes) = blob_of(b"good");
    let (lying, _) = blob_of(b"claimed");
    let (zipped, zipped_bytes) = blob_of(b"zipped");
    let mut compressed = item(&zipped, &zipped_bytes);
    compressed.compressor = reapi::compressor::Value::Zstd as i32;

    let stored = update(
        &rig,
        vec![
            item(&lying, &Bytes::from_static(b"other bytes")),
            item(&good, &good_bytes),
            compressed,
        ],
    )
    .await;

    let invalid = Code::InvalidArgument as i32;
    assert_eq!(stored.expect("answered"), vec![invalid, OK, invalid]);
    let lacking = missing(&rig, &[lying, good, zipped])
        .await
        .expect("answered");
    assert_eq!(lacking, vec![lying, zipped]);
}

/// Asserts that a blob of a batch the frontend does not hold is NOT_FOUND alone, the others
/// served, and that a digest that does not parse is INVALID_ARGUMENT alone.
#[tokio::test]
async fn a_batch_read_answers_each_blob_alone() {
    let rig = Rig::new().await;
    let (held, held_bytes) = blob_of(b"held");
    let (absent, _) = blob_of(b"absent");
    update(&rig, vec![item(&held, &held_bytes)])
        .await
        .expect("stored");
    let mut asked = vec![proto(&absent), proto(&held)];
    asked.push(reapi::Digest {
        hash: "not a hash".to_owned(),
        size_bytes: 3,
    });

    let response = ContentAddressableStorageClient::new(rig.front.clone())
        .batch_read_blobs(BatchReadBlobsRequest {
            digests: asked,
            ..BatchReadBlobsRequest::default()
        })
        .await
        .expect("answered")
        .into_inner();

    let codes: Vec<i32> = response
        .responses
        .iter()
        .map(|r| r.status.as_ref().map_or(-1, |s| s.code))
        .collect();
    let (not_found, invalid) = (Code::NotFound as i32, Code::InvalidArgument as i32);
    assert_eq!(codes, vec![not_found, OK, invalid]);
    assert_eq!(response.responses[1].data, held_bytes.to_vec());
}

/// Asserts that when the object store cannot be read, the blobs of a batch that are in packs
/// fail UNAVAILABLE each (not NOT_FOUND: the blob is held), and that when it can be again they
/// are served.
#[tokio::test]
async fn a_pack_that_cannot_be_read_fails_unavailable_not_not_found() {
    let rig = Rig::new().await;
    let (held, held_bytes) = blob_of(b"held");
    update(&rig, vec![item(&held, &held_bytes)])
        .await
        .expect("stored");
    rig.store.reject_reads(1);

    let failed = read(&rig, &[held]).await.expect("answered");
    let served = read(&rig, &[held]).await.expect("answered");

    assert_eq!(failed[0].0, Code::Unavailable as i32);
    assert_eq!(served, vec![(OK, held_bytes.to_vec())]);
}

/// Asserts that when the object store refuses the pack, every blob of the batch that was to be
/// in it fails UNAVAILABLE and none is held afterwards.
#[tokio::test]
async fn a_pack_the_store_refuses_leaves_no_blob_held() {
    let rig = Rig::new().await;
    let (one, one_bytes) = blob_of(b"one");
    let (two, two_bytes) = blob_of(b"two");
    rig.store.reject_puts(8);

    let stored = update(&rig, vec![item(&one, &one_bytes), item(&two, &two_bytes)]).await;

    let unavailable = Code::Unavailable as i32;
    assert_eq!(stored.expect("answered"), vec![unavailable, unavailable]);
    assert_eq!(
        missing(&rig, &[one, two]).await.expect("answered"),
        vec![one, two]
    );
}

/// Asserts that FindMissingBlobs fails UNAVAILABLE when the pack index cannot answer, rather
/// than reporting every blob missing (which would make a client upload everything again).
#[tokio::test]
async fn find_missing_fails_unavailable_when_the_index_cannot_answer() {
    let rig = Rig::new().await;
    let (digest, _) = blob_of(b"anything");
    rig.index.fail_find(1);

    let failed = missing(&rig, &[digest])
        .await
        .expect_err("the index failed");

    assert_eq!(failed.code(), Code::Unavailable);
}

/// Asserts that a batch over the size limit is refused INVALID_ARGUMENT in both directions
/// before any blob is read or written, and that a digest function other than SHA-256 is too.
#[tokio::test]
async fn an_oversized_batch_and_another_digest_function_are_refused() {
    let rig = Rig::new().await;
    let mut cas = ContentAddressableStorageClient::new(rig.front.clone());
    let (_, big) = blob_len(MAX_BATCH_TOTAL_BYTES + 1, 1);
    let (digest, _) = blob_of(b"x");

    let upload = cas
        .batch_update_blobs(BatchUpdateBlobsRequest {
            requests: vec![item(&digest, &big)],
            ..BatchUpdateBlobsRequest::default()
        })
        .await
        .expect_err("over the limit");
    let download = cas
        .batch_read_blobs(BatchReadBlobsRequest {
            digests: vec![reapi::Digest {
                hash: digest.hash_hex(),
                size_bytes: MAX_BATCH_TOTAL_BYTES as i64 + 1,
            }],
            ..BatchReadBlobsRequest::default()
        })
        .await
        .expect_err("over the limit");
    let other = cas
        .find_missing_blobs(FindMissingBlobsRequest {
            blob_digests: vec![proto(&digest)],
            digest_function: reapi::digest_function::Value::Sha1 as i32,
            ..FindMissingBlobsRequest::default()
        })
        .await
        .expect_err("not SHA-256");

    for refused in [upload, download, other] {
        assert_eq!(refused.code(), Code::InvalidArgument);
    }
}

/// Asserts that the calls this frontend does not serve answer UNIMPLEMENTED, and do not reach
/// anything that is not there.
#[tokio::test]
async fn the_calls_that_are_not_served_are_unimplemented() {
    let rig = Rig::new().await;
    let mut cas = ContentAddressableStorageClient::new(rig.front.clone());

    let tree = cas.get_tree(GetTreeRequest::default()).await.err();
    let split = cas.split_blob(SplitBlobRequest::default()).await.err();
    let splice = cas.splice_blob(SpliceBlobRequest::default()).await.err();
    let mapping = cas
        .get_chunk_mapping(reapi::GetChunkMappingRequest::default())
        .await
        .err();
    let register = cas
        .register_chunk_mapping(stream::iter([RegisterChunkMappingRequest::default()]))
        .await
        .err();
    let register_nothing = cas
        .register_chunk_mapping(stream::iter(Vec::<RegisterChunkMappingRequest>::new()))
        .await
        .err();

    for refused in [tree, split, splice, mapping, register, register_nothing] {
        assert_eq!(refused.expect("refused").code(), Code::Unimplemented);
    }
}
