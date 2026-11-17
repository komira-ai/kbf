//! The invocation record end to end (docs/design/invocations.md, section 9, PR 3): a
//! REAPI client that sends a buck2-shaped `RequestMetadata` header on every call and the
//! server with its recorder. The rows a build gets for a cache hit and the counts of its blob
//! calls are read back from the server's recorder.
//!
//! The executor's own events (an operation answered or joined, a job granted, started or run
//! again) go to the recorder the executor is built over, which the test server does not share
//! with the one it binds: `src/executor/record_tests.rs` holds those.
//!
//! A module of the `execute` test binary, not a binary of its own.

use kbf_front::REQUEST_METADATA_HEADER;
use kbf_invocation::{How, Invocation, InvocationId, Outcome, RecordKey};
use kbf_proto::google::bytestream::byte_stream_client::ByteStreamClient;
use kbf_proto::google::bytestream::{ReadRequest, WriteRequest};
use kbf_proto::google::longrunning::Operation;
use kbf_proto::reapi::content_addressable_storage_client::ContentAddressableStorageClient;
use kbf_proto::reapi::execution_client::ExecutionClient;
use kbf_proto::reapi::{
    ActionResult, BatchReadBlobsRequest, BatchUpdateBlobsRequest, ExecuteRequest,
    FindMissingBlobsRequest, GetActionResultRequest, RequestMetadata, ToolDetails,
    action_cache_client::ActionCacheClient, batch_update_blobs_request,
};
use prost::Message;
use tonic::Code;
use tonic::metadata::MetadataValue;
use tonic::{Request, Streaming};

use crate::support::{Blob, Cell, Job, Ran, done};

/// The build's id, as buck2 prints it (`Build ID:`).
pub(crate) const BUILD: &str = "0b8c2f0e-6d1a-4c55-9a7e-2f43d1c0a9b4";

/// `message` with the `RequestMetadata` header buck2 sends, naming invocation `id`.
pub(crate) fn tagged<T>(id: &str, message: T) -> Request<T> {
    let metadata = RequestMetadata {
        tool_details: Some(ToolDetails {
            tool_name: "buck2".to_owned(),
            tool_version: "0.1.0".to_owned(),
        }),
        tool_invocation_id: id.to_owned(),
        ..Default::default()
    };
    let mut request = Request::new(message);
    request.metadata_mut().insert_bin(
        REQUEST_METADATA_HEADER,
        MetadataValue::from_bytes(&metadata.encode_to_vec()),
    );
    request
}

pub(crate) async fn execute(cell: &Cell, id: &str, action: &Blob) -> Streaming<Operation> {
    let request = ExecuteRequest {
        instance_name: "main".to_owned(),
        action_digest: Some(action.proto.clone()),
        ..Default::default()
    };
    ExecutionClient::new(cell.channel())
        .execute(tagged(id, request))
        .await
        .expect("Execute")
        .into_inner()
}

async fn get_action_result(cell: &Cell, id: &str, action: &Blob) -> Result<ActionResult, Code> {
    let request = GetActionResultRequest {
        instance_name: "main".to_owned(),
        action_digest: Some(action.proto.clone()),
        ..Default::default()
    };
    ActionCacheClient::new(cell.channel())
        .get_action_result(tagged(id, request))
        .await
        .map(tonic::Response::into_inner)
        .map_err(|s| s.code())
}

/// Uploads `blobs` in one BatchUpdateBlobs call naming `id`; their bytes.
pub(crate) async fn upload(cell: &Cell, id: &str, blobs: &[&Blob]) -> u64 {
    let request = BatchUpdateBlobsRequest {
        instance_name: "main".to_owned(),
        requests: blobs
            .iter()
            .map(|b| batch_update_blobs_request::Request {
                digest: Some(b.proto.clone()),
                data: b.data.clone(),
                ..Default::default()
            })
            .collect(),
        ..Default::default()
    };
    ContentAddressableStorageClient::new(cell.channel())
        .batch_update_blobs(tagged(id, request))
        .await
        .expect("BatchUpdateBlobs");
    blobs.iter().map(|b| b.data.len() as u64).sum()
}

/// The record of `id`, once the recorder has applied every event so far.
pub(crate) async fn record(cell: &Cell, id: &str) -> Invocation {
    cell.recorder.caught_up().await;
    cell.recorder
        .get(&RecordKey::owner(InvocationId::new(id)))
        .unwrap_or_else(|| panic!("no record of {id}"))
}

/// The digest of the ActionResult blob the server stored for `result`.
fn stored(result: &ActionResult) -> kbf_types::Digest {
    kbf_segments::sha256(&result.encode_to_vec())
}

/// Asserts that a build's cache lookups and uploads are recorded on its id: a GetActionResult hit
/// is a row that is a cache hit with the stored result's listing and its outcome, a miss makes no
/// row and counts as a miss, and the bytes the build uploaded are counted. An action built before
/// by a call with no id is cached but not recorded.
#[tokio::test]
async fn a_build_records_a_hit_a_miss_and_the_bytes_it_uploaded() {
    let cell = Cell::start().await;

    let hit = Job::new("built before", &[]);
    cell.upload(&hit.blobs()).await;
    cell.direct
        .will_run(&hit.action, Ran::writes("out", "built before"));
    let mut ops = cell.execute(&hit.action).await;
    cell.direct.complete(&hit.action).await;
    done(&mut ops).await;

    let cached = get_action_result(&cell, BUILD, &hit.action)
        .await
        .expect("a hit");
    let never = Job::new("never built", &[]);
    let miss = get_action_result(&cell, BUILD, &never.action).await;
    assert_eq!(miss, Err(Code::NotFound));
    let uploaded = upload(&cell, BUILD, &never.blobs()).await;

    let inv = record(&cell, BUILD).await;
    assert!(inv.complete(), "no event was dropped");
    assert_eq!(inv.tool(), (Some("buck2"), Some("0.1.0")));
    let [hit_row] = inv.rows() else {
        panic!("one row, the hit's: {:?}", inv.rows());
    };
    assert_eq!(
        (hit_row.action, hit_row.how),
        (hit.action.digest, How::CacheHit)
    );
    let listing = hit_row.listing.as_ref().expect("the hit's listing");
    assert_eq!(listing.result, Some(stored(&cached)));
    let outputs: Vec<_> = listing.outputs.iter().map(|o| o.path.as_str()).collect();
    assert_eq!(outputs, ["out"]);
    assert_eq!(hit_row.outcome, Some(Outcome::Ok { exit_code: 0 }));
    assert_eq!(hit_row.operation_id, None, "a cache hit waits on none");
    let c = inv.counts();
    assert_eq!((c.ac_hit, c.ac_miss), (1, 1));
    assert_eq!(c.bytes_uploaded, uploaded);
}

/// Asserts that a cached Execute is recorded as `cache_hit_execute` on the id it carried, and
/// that the CAS and ByteStream calls are counted (FindMissingBlobs asked and missing, bytes read
/// and written), each on the record of the id it carried.
#[tokio::test]
async fn cached_executes_and_blob_calls_are_recorded() {
    let cell = Cell::start().await;
    let job = Job::new("shared", &[]);
    cell.upload(&job.blobs()).await;
    let mut built = cell.execute(&job.action).await;
    cell.direct.complete(&job.action).await;
    done(&mut built).await;

    let mut again = execute(&cell, "second-build", &job.action).await;
    done(&mut again).await;
    let second = record(&cell, "second-build").await;
    let [row] = second.rows() else {
        panic!("one row: {:?}", second.rows());
    };
    assert_eq!(row.how, How::CacheHitExecute);
    let c = second.counts();
    assert_eq!((c.execute, c.cache_hit_execute), (1, 1));

    // The blob calls, under a third id.
    let id = "blob-calls";
    let blob = Blob::new("written over ByteStream");
    let absent = Blob::new("never uploaded");
    let find = FindMissingBlobsRequest {
        instance_name: "main".to_owned(),
        blob_digests: vec![job.input.proto.clone(), absent.proto.clone()],
        ..Default::default()
    };
    let mut cas = ContentAddressableStorageClient::new(cell.channel());
    cas.find_missing_blobs(tagged(id, find))
        .await
        .expect("FindMissingBlobs");
    let read = BatchReadBlobsRequest {
        instance_name: "main".to_owned(),
        digests: vec![job.input.proto.clone()],
        ..Default::default()
    };
    cas.batch_read_blobs(tagged(id, read))
        .await
        .expect("BatchReadBlobs");
    let resource = format!(
        "main/uploads/0f0e0d0c-0000-4000-8000-000000000001/blobs/{}/{}",
        blob.digest.hash_hex(),
        blob.digest.size_bytes
    );
    let write = WriteRequest {
        resource_name: resource,
        write_offset: 0,
        finish_write: true,
        data: blob.data.clone(),
    };
    let mut bytestream = ByteStreamClient::new(cell.channel());
    bytestream
        .write(tagged(id, futures::stream::iter([write])))
        .await
        .expect("Write");
    let read = ReadRequest {
        resource_name: format!(
            "main/blobs/{}/{}",
            blob.digest.hash_hex(),
            blob.digest.size_bytes
        ),
        ..Default::default()
    };
    let mut stream = bytestream
        .read(tagged(id, read))
        .await
        .expect("Read")
        .into_inner();
    while stream.message().await.expect("a chunk").is_some() {}

    let blobs = record(&cell, id).await;
    let c = blobs.counts();
    assert_eq!((c.blobs_asked, c.blobs_missing), (2, 1));
    assert_eq!(c.bytes_uploaded, blob.data.len() as u64);
    assert_eq!(
        c.bytes_downloaded,
        (job.input.data.len() + blob.data.len()) as u64
    );
    assert!(blobs.rows().is_empty(), "blob calls make no rows");
}

/// Asserts that a call with no invocation id sends no event to the recorder: an Execute that
/// runs to its answer under no id leaves the recorder with nothing, and a call that carries an
/// id sends one.
#[tokio::test]
async fn calls_without_an_id_send_no_events() {
    let cell = Cell::start().await;
    let run = Job::new("id-less", &[]);
    cell.upload(&run.blobs()).await;

    let mut ops = cell.execute(&run.action).await;
    cell.direct.complete(&run.action).await;
    done(&mut ops).await;
    cell.recorder.caught_up().await;
    assert_eq!(cell.recorder.sent(), 0, "no event for an id-less call");

    upload(&cell, BUILD, &[&Blob::new("with an id")]).await;
    assert_eq!(cell.recorder.sent(), 1);
}
