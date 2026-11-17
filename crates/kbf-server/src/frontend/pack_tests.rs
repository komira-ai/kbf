//! A frontend started with the pack flags, over an S3 service on loopback and an in-memory pack
//! index: its CAS calls keep blobs in packs, its metrics count them, and the checks of its start
//! refuse what cannot work. Without the flags it keeps nothing in packs and contacts no store.

use std::error::Error;
use std::future::pending;
use std::net::SocketAddr;
use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
use std::sync::Arc;

use kbf_objstore::s3::fake::FakeS3;
use kbf_pack_index::memory::MemoryPackIndex;
use kbf_pack_index::{BlobDigest, PackIndex};
use kbf_proto::reapi::content_addressable_storage_client::ContentAddressableStorageClient;
use kbf_proto::reapi::{
    self, BatchReadBlobsRequest, BatchUpdateBlobsRequest, FindMissingBlobsRequest,
    batch_update_blobs_request,
};
use kbf_state::ManualClock;
use kbf_types::Digest;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpStream;
use tonic::Code;
use tonic::transport::{Channel, Endpoint};

use super::{FrontendFlags, FrontendHandle, Prepared, boxed, command, parse};
use crate::test_dir::scratch_dir;

/// The flags of a frontend on loopback ports, with a Postgres state store that is only parsed
/// (the index is handed to the start) and `--pack-index`, and `more`.
pub(super) fn flags_of(more: &[String]) -> FrontendFlags {
    flags_over(Some("--storage=http://127.0.0.1:1"), more)
}

/// [`flags_of`] with the `--storage` flag `storage` (none: a frontend with no storage node).
pub(super) fn flags_over(storage: Option<&str>, more: &[String]) -> FrontendFlags {
    let matches = command()
        .try_get_matches_from(argv_over(storage, more))
        .expect("flags");
    parse(&matches).expect("a frontend's flags")
}

/// The command line [`flags_over`] parses.
pub(super) fn argv_over(storage: Option<&str>, more: &[String]) -> Vec<String> {
    let mut argv: Vec<String> = ["frontend"]
        .into_iter()
        .chain(storage)
        .chain([
            "--listen=127.0.0.1:0",
            "--worker-listen=127.0.0.1:0",
            "--api-listen=127.0.0.1:0",
            "--state-store=postgres:///kbf?host=/run/postgresql",
            "--pack-index",
        ])
        .map(ToString::to_string)
        .collect();
    argv.extend(more.iter().cloned());
    argv
}

/// The pack flags for the S3 service at `endpoint`, with a credentials file in `dir`.
pub(super) fn pack_flags(endpoint: &str, dir: &Path) -> Vec<String> {
    let file = dir.join("pack-credentials.json");
    std::fs::write(&file, r#"{"access_key_id": "k", "secret_access_key": "s"}"#).expect("written");
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).expect("mode");
    vec![
        format!("--pack-store-endpoint={endpoint}"),
        "--pack-store-bucket=kbf-fake".to_owned(),
        format!("--pack-store-credentials-file={}", file.display()),
        "--pack-writer=fe9".to_owned(),
    ]
}

pub(super) fn index() -> Arc<dyn PackIndex> {
    Arc::new(MemoryPackIndex::new(Arc::new(ManualClock::default())))
}

/// Starts the frontend `flags` describe over `index`, bound and not yet serving.
pub(super) fn started(
    flags: &FrontendFlags,
    index: Option<Arc<dyn PackIndex>>,
) -> Result<FrontendHandle, Box<dyn Error>> {
    let shutdown = boxed(|| Ok(pending::<()>()));
    Prepared::new(flags)?.start(None, index, shutdown, None)
}

async fn connect(addr: SocketAddr) -> ContentAddressableStorageClient<Channel> {
    let endpoint = Endpoint::from_shared(format!("http://{addr}")).expect("endpoint");
    ContentAddressableStorageClient::new(endpoint.connect().await.expect("connects"))
}

fn proto(digest: &Digest) -> reapi::Digest {
    kbf_front::digest_to_proto(digest)
}

/// The status code of each item of a BatchUpdateBlobs of `blobs`.
pub(super) async fn upload(addr: SocketAddr, blobs: &[(Digest, &[u8])]) -> Vec<i32> {
    let requests = blobs
        .iter()
        .map(|(digest, bytes)| batch_update_blobs_request::Request {
            digest: Some(proto(digest)),
            data: bytes.to_vec(),
            compressor: reapi::compressor::Value::Identity as i32,
        })
        .collect();
    let response = connect(addr)
        .await
        .batch_update_blobs(BatchUpdateBlobsRequest {
            requests,
            ..BatchUpdateBlobsRequest::default()
        })
        .await
        .expect("answered");
    let responses = response.into_inner().responses;
    responses
        .iter()
        .map(|r| r.status.as_ref().expect("a status").code)
        .collect()
}

/// The text `GET /metrics` of the operator API at `addr` answers.
pub(super) async fn scrape(addr: SocketAddr) -> String {
    get(addr, "/metrics").await
}

/// The whole answer (head and body) of `GET path` on the operator listener at `addr`.
async fn get(addr: SocketAddr, path: &str) -> String {
    let mut stream = TcpStream::connect(addr).await.expect("connects");
    let request = format!("GET {path} HTTP/1.1\r\nHost: kbf\r\nConnection: close\r\n\r\n");
    stream.write_all(request.as_bytes()).await.expect("sent");
    let mut text = String::new();
    stream.read_to_string(&mut text).await.expect("answered");
    text
}

fn index_form(digest: &Digest) -> BlobDigest {
    BlobDigest {
        hash: digest.hash,
        size: digest.size_bytes,
    }
}

/// Asserts what a frontend started with the pack flags does: it passes its start check on the
/// store, keeps a BatchUpdateBlobs in one pack of the store and in the index, answers
/// FindMissingBlobs and BatchReadBlobs from them while its storage node is unreachable, and
/// counts it all on `GET /metrics`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_frontend_with_the_pack_flags_keeps_uploads_in_packs() {
    let s3 = FakeS3::start().await;
    let dir = scratch_dir("pack-frontend");
    let flags = flags_of(&pack_flags(s3.endpoint(), dir.path()));
    let index = index();
    let handle = started(&flags, Some(Arc::clone(&index))).expect("starts");
    assert!(handle.packs.is_some());
    let (front, api) = (handle.reapi, handle.api.expect("the operator API"));
    let served = tokio::spawn(handle.serve());
    let blobs: [(Digest, &[u8]); 2] = [
        (kbf_segments::sha256(b"packed one"), b"packed one"),
        (kbf_segments::sha256(b"packed two"), b"packed two"),
    ];

    assert_eq!(upload(front, &blobs).await, [0, 0]);

    let keys = s3.keys();
    assert_eq!(keys.len(), 1, "{keys:?}");
    assert!(keys[0].starts_with("cas/f/fe9/"), "{keys:?}");
    let asked = [index_form(&blobs[0].0), index_form(&blobs[1].0)];
    let located = index.locate(&asked).expect("located");
    assert!(located.iter().all(Option::is_some));
    let digests = blobs.iter().map(|(digest, _)| proto(digest)).collect();
    let missing = connect(front)
        .await
        .find_missing_blobs(FindMissingBlobsRequest {
            blob_digests: digests,
            ..FindMissingBlobsRequest::default()
        })
        .await
        .expect("answered");
    assert!(missing.into_inner().missing_blob_digests.is_empty());
    let read = connect(front)
        .await
        .batch_read_blobs(BatchReadBlobsRequest {
            digests: blobs.iter().map(|(digest, _)| proto(digest)).collect(),
            ..BatchReadBlobsRequest::default()
        })
        .await
        .expect("answered");
    for (got, (_, bytes)) in read.into_inner().responses.iter().zip(&blobs) {
        assert_eq!(got.status.as_ref().expect("a status").code, 0);
        assert_eq!(got.data.as_slice(), *bytes);
    }
    let text = scrape(api).await;
    assert!(text.contains("kbf_pack_writes_total 1\n"), "{text}");
    assert!(text.contains("kbf_pack_reads_total 2\n"), "{text}");
    served.abort();
}

/// A loopback endpoint nothing listens on: a port the system handed out and then took back, so
/// a connection to it is refused.
fn refused_endpoint() -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("a loopback port");
    let addr = listener.local_addr().expect("its address");
    drop(listener);
    format!("http://{addr}")
}

/// Asserts that a frontend given two `--pack-store-endpoint`s, the first of which refuses every
/// connection, reaches the store through the second: it passes its start check and keeps a
/// BatchUpdateBlobs in one pack that the index records, so one endpoint of the farm's being
/// down does not stop uploads.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_frontend_with_two_pack_endpoints_reaches_the_store_through_the_live_one() {
    let s3 = FakeS3::start().await;
    let dir = scratch_dir("pack-two-endpoints");
    let mut more = pack_flags(s3.endpoint(), dir.path());
    more.insert(0, format!("--pack-store-endpoint={}", refused_endpoint()));
    let flags = flags_of(&more);
    assert_eq!(flags.packs.as_ref().map(|p| p.endpoints.len()), Some(2));
    let index = index();
    let handle = started(&flags, Some(Arc::clone(&index))).expect("starts");
    let front = handle.reapi;
    let served = tokio::spawn(handle.serve());
    let blob = (kbf_segments::sha256(b"through two"), &b"through two"[..]);

    assert_eq!(upload(front, &[blob]).await, [0]);

    let keys = s3.keys();
    assert_eq!(keys.len(), 1, "{keys:?}");
    let located = index.locate(&[index_form(&blob.0)]).expect("located");
    assert_eq!(
        located[0].as_ref().map(|l| l.pack_key.as_str()),
        Some(keys[0].as_str())
    );
    served.abort();
}

/// Asserts that a frontend given `--pack-index` and no pack store flags keeps nothing in packs:
/// it carries no packs, an upload goes to its storage node (which is not there, so it fails
/// UNAVAILABLE) and leaves no row, and its metrics have no pack family.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_frontend_without_the_pack_flags_keeps_nothing_in_packs() {
    let flags = flags_of(&[]);
    let index = index();
    let handle = started(&flags, Some(Arc::clone(&index))).expect("starts");
    assert!(handle.packs.is_none());
    assert!(handle.pack_index().is_some());
    let (front, api) = (handle.reapi, handle.api.expect("the operator API"));
    let served = tokio::spawn(handle.serve());
    let blob = (kbf_segments::sha256(b"not packed"), &b"not packed"[..]);

    let codes = upload(front, &[blob]).await;
    assert_ne!(codes, [Code::Ok as i32], "the node is not there");

    let located = index.locate(&[index_form(&blob.0)]).expect("located");
    assert!(located[0].is_none());
    let text = scrape(api).await;
    assert!(!text.contains("kbf_pack_"), "{text}");
    served.abort();
}

/// Asserts that a frontend started with no `--worker-listen` has no worker address, says none in
/// its start line, and still serves its REAPI listener, which accepts a connection. Catches: a
/// start that needs the worker listener to serve, and one that names a worker address it does
/// not have.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_frontend_with_no_worker_listener_still_serves_its_reapi_listener() {
    let mut argv = argv_over(Some("--storage=http://127.0.0.1:1"), &[]);
    argv.retain(|flag| !flag.starts_with("--worker-listen"));
    let matches = command().try_get_matches_from(argv).expect("flags");
    let flags = parse(&matches).expect("a frontend's flags");
    let handle = started(&flags, Some(index())).expect("starts");
    assert!(handle.worker.is_none());
    assert!(
        !handle.start_line().contains("worker="),
        "{}",
        handle.start_line()
    );
    let reapi = handle.reapi;
    let serving = tokio::spawn(handle.serve());

    tokio::net::TcpStream::connect(reapi)
        .await
        .expect("the REAPI listener accepts");
    assert!(!serving.is_finished());

    serving.abort();
}

/// Asserts that a frontend writes `kbf_execute_failed_actions_total` on `GET /metrics` with a
/// series of each of the four classes at 0 before any Execute, the page the beta gate reads.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_frontend_exports_its_failed_actions_by_class() {
    let flags = flags_of(&[]);
    let handle = started(&flags, Some(index())).expect("starts");
    let api = handle.api.expect("the operator API");
    let served = tokio::spawn(handle.serve());
    let text = scrape(api).await;
    assert!(
        text.contains("# TYPE kbf_execute_failed_actions_total counter\n"),
        "{text}"
    );
    for class in ["farm", "request", "action", "ambiguous"] {
        let sample = format!("kbf_execute_failed_actions_total{{class=\"{class}\"}} 0\n");
        assert!(text.contains(&sample), "{sample}{text}");
    }
    served.abort();
}

/// Asserts that a frontend writes `kbf_scheduler_core` on `GET /metrics`: the one series,
/// `legacy` at 1, whether `--scheduler-core legacy` is given or left out, and no series for
/// the js core.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_frontend_exports_the_scheduler_core_it_runs() {
    for given in [None, Some("--scheduler-core=legacy")] {
        let more: Vec<String> = given.into_iter().map(str::to_owned).collect();
        let flags = flags_of(&more);
        let handle = started(&flags, Some(index())).expect("starts");
        let api = handle.api.expect("the operator API");
        let served = tokio::spawn(handle.serve());
        let text = scrape(api).await;
        assert!(text.contains("# TYPE kbf_scheduler_core gauge\n"), "{text}");
        let on = "kbf_scheduler_core{core=\"legacy\"} 1\n";
        assert!(text.contains(on), "{given:?}: {on}{text}");
        assert!(!text.contains("core=\"js\""), "{given:?}: {text}");
        served.abort();
    }
}

/// Asserts that a frontend whose pack store cannot be written to does not serve: the start check
/// fails and `serve` says so naming the flag.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pack_store_that_cannot_be_written_fails_the_start_check() {
    let dir = scratch_dir("pack-unreachable");
    let flags = flags_of(&pack_flags("http://127.0.0.1:1", dir.path()));
    let handle = started(&flags, Some(index())).expect("binds");

    let failed = handle.serve().await;

    let said = failed.expect_err("not served").to_string();
    assert!(said.starts_with("--pack-store-endpoint: "), "{said}");
}

/// Asserts that pack flags with no pack index open refuse the start, naming the flag, though
/// the command line parser accepts the flags: a start is handed the index by the flag that opens
/// it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pack_flags_without_the_pack_index_refuse_the_start() {
    let dir = scratch_dir("pack-no-index");
    let flags = flags_of(&pack_flags("http://127.0.0.1:1", dir.path()));

    let refused = started(&flags, None).err().expect("refused");

    assert!(refused.to_string().contains("--pack-index"), "{refused}");
}
