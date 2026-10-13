//! Runs against a kbf-server in this process (the in-memory cache): every blob of a
//! 500-file tree sent and read back by each method, over the worker listener's mutual
//! TLS and over the REAPI listener, checked byte for byte by a client of the test's
//! own; FindMissingBlobs; failures counted and reported; the deadline.

mod support;

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::time::Duration;

use futures::stream;
use kbf_bench::bench::{self, Method, Options};
use kbf_bench::connect::{self, Target};
use kbf_bench::report::{Kind, Report};
use kbf_bench::tree::{self, Shape, Tree};
use kbf_proto::google::bytestream::byte_stream_server::{ByteStream, ByteStreamServer};
use kbf_proto::google::bytestream::{
    QueryWriteStatusRequest, QueryWriteStatusResponse, ReadRequest, ReadResponse, WriteRequest,
    WriteResponse,
};
use kbf_proto::google::rpc;
use kbf_proto::reapi::content_addressable_storage_server::{
    ContentAddressableStorage, ContentAddressableStorageServer,
};
use kbf_proto::reapi::{
    BatchReadBlobsRequest, BatchReadBlobsResponse, BatchUpdateBlobsRequest,
    BatchUpdateBlobsResponse, FindMissingBlobsRequest, FindMissingBlobsResponse,
    GetChunkMappingRequest, GetChunkMappingResponse, GetTreeRequest, GetTreeResponse,
    RegisterChunkMappingRequest, RegisterChunkMappingResponse, SpliceBlobRequest,
    SpliceBlobResponse, SplitBlobRequest, SplitBlobResponse, batch_read_blobs_response,
};
use support::{Server, plain};
use tonic::transport::Server as TonicServer;
use tonic::transport::server::TcpIncoming;
use tonic::{Request, Response, Status, Streaming};

const SHAPE: Shape = Shape {
    files: 500,
    empty: 5,
    dirs: 20,
    total_bytes: 2_000_000,
};

fn options(method: Method) -> Options {
    Options {
        method,
        in_flight: 16,
        find_missing: false,
        batch_bytes: 4 << 20,
        timeout: None,
        progress: None,
    }
}

async fn upload(target: &Target, tree: &Tree, options: Options) -> Report {
    bench::upload(connect::channel(target).expect("channel"), tree, options).await
}

async fn read(target: &Target, tree: &Tree, options: Options) -> Report {
    bench::read(connect::channel(target).expect("channel"), tree, options).await
}

fn bytes_of(tree: &Tree) -> u64 {
    tree.blobs.iter().map(|b| b.bytes.len() as u64).sum()
}

/// Each kind of request and how many of it the report timed.
fn counts(report: &Report) -> BTreeMap<Kind, u64> {
    report.requests.iter().map(|(k, l)| (*k, l.count)).collect()
}

/// Catches a run that loses or skips blobs, miscounts them or their bytes, reports a
/// rate that is not bytes over wall time, or times no request (a timer that never
/// records shows as zero times).
fn assert_moved_all(report: &Report, tree: &Tree) {
    let blobs = tree.blobs.len() as u64;
    assert!(report.complete(), "{}", report.table());
    assert_eq!(report.blobs_planned, blobs, "{}", report.table());
    assert_eq!(report.blobs_done, blobs, "{}", report.table());
    assert_eq!(report.bytes_done, bytes_of(tree));
    assert_eq!(report.requests_done, report.requests_planned);
    let timed: u64 = report.requests.values().map(|l| l.count).sum();
    assert_eq!(timed, report.requests_done, "every request timed once");
    for (kind, l) in &report.requests {
        assert!(
            0.0 < l.p50_ms && l.p50_ms <= l.p90_ms && l.p90_ms <= l.p99_ms && l.p99_ms <= l.max_ms,
            "{kind:?}: {l:?}"
        );
    }
    let rate = report.bytes_done as f64 / 1e6 / report.wall_secs;
    assert!((report.mb_per_sec - rate).abs() < 1e-9 * rate.max(1.0));
    let files = report.tree.files as f64 / report.wall_secs;
    assert!((report.files_per_sec - files).abs() < 1e-9 * files);
    assert_eq!(report.failed, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn five_hundred_files_round_trip_by_bytestream_over_the_worker_listener() {
    let server = Server::start("bytestream");
    let tree = tree::generate(&SHAPE, 1).expect("tree");
    assert_eq!(
        tree.blobs.len(),
        495 + 2,
        "the files, the empty blob, the Tree"
    );
    let target = server.worker_target();

    let sent = upload(&target, &tree, options(Method::Bytestream)).await;
    assert_moved_all(&sent, &tree);
    assert_eq!(counts(&sent), BTreeMap::from([(Kind::Write, 497)]));
    assert_eq!(sent.tree.files, 500);
    assert_eq!(sent.seed, 1);
    server.assert_holds(&tree).await;
    let table = sent.table();
    assert!(
        table.starts_with("upload bytestream, 16 in flight, find-missing off, seed 1\n"),
        "{table}"
    );
    assert!(table.contains("\nwrite              497 "), "{table}");

    let got = read(&target, &tree, options(Method::Bytestream)).await;
    assert_moved_all(&got, &tree);
    assert_eq!(counts(&got), BTreeMap::from([(Kind::Read, 497)]));
    assert!(
        got.table().contains("\nread               497 "),
        "{}",
        got.table()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn five_hundred_files_round_trip_by_batch_over_the_reapi_listener() {
    let server = Server::start("batch");
    let tree = tree::generate(&SHAPE, 2).expect("tree");
    let budget = 16 << 10;
    let large = tree
        .blobs
        .iter()
        .filter(|b| b.bytes.len() as u64 > budget)
        .count() as u64;
    let small_bytes: u64 = tree
        .blobs
        .iter()
        .map(|b| b.bytes.len() as u64)
        .filter(|&n| n <= budget)
        .sum();
    assert!(large > 0, "some blob is larger than a batch");
    let target = server.reapi_target();
    let mut options = options(Method::Batch);
    options.batch_bytes = budget;

    let sent = upload(&target, &tree, options).await;
    assert_moved_all(&sent, &tree);
    let counts_sent = counts(&sent);
    assert_eq!(
        counts_sent[&Kind::Write],
        large,
        "each large blob by ByteStream"
    );
    // A batch closes only when the next blob does not fit, so it holds more than half
    // of the budget, but never more than all of it.
    let batches = counts_sent[&Kind::BatchUpdate];
    assert!(
        small_bytes.div_ceil(budget) <= batches && batches <= 2 * small_bytes / budget + 1,
        "{batches} batches for {small_bytes} bytes"
    );
    server.assert_holds(&tree).await;
    assert!(sent.table().contains("\nbatch_update "), "{}", sent.table());

    let got = read(&target, &tree, options).await;
    assert_moved_all(&got, &tree);
    let counts_got = counts(&got);
    assert_eq!(counts_got[&Kind::Read], large);
    assert_eq!(
        counts_got[&Kind::BatchRead],
        batches,
        "packed as the upload was"
    );
    assert!(got.table().contains("\nbatch_read "), "{}", got.table());
}

/// Catches FindMissingBlobs not asked, its answer ignored (everything sent again), or
/// what it reports missing not sent.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn find_missing_sends_only_the_missing_blobs() {
    let server = Server::start("find-missing");
    let target = server.reapi_target();
    let shape = Shape {
        files: 1500,
        ..SHAPE
    };
    let first = tree::generate(&shape, 3).expect("tree");
    assert_moved_all(
        &upload(&target, &first, options(Method::Bytestream)).await,
        &first,
    );

    let mut find = options(Method::Batch);
    find.find_missing = true;
    let again = upload(&target, &first, find).await;
    assert!(again.complete(), "{}", again.table());
    assert_eq!((again.blobs_planned, again.blobs_done), (0, 0));
    assert_eq!(
        counts(&again),
        BTreeMap::from([(Kind::FindMissing, 2)]),
        "1000 digests a call"
    );
    assert!(
        again.table().contains("find-missing on"),
        "{}",
        again.table()
    );
    assert!(
        again.table().contains("\nfind_missing         2 "),
        "{}",
        again.table()
    );

    find.method = Method::Bytestream;
    let second = tree::generate(&shape, 4).expect("tree");
    let fresh = upload(&target, &second, find).await;
    assert!(fresh.complete(), "{}", fresh.table());
    // The empty blob is always present.
    let missing = second.blobs.len() as u64 - 1;
    assert_eq!((fresh.blobs_planned, fresh.blobs_done), (missing, missing));
    assert_eq!(
        counts(&fresh),
        BTreeMap::from([(Kind::FindMissing, 2), (Kind::Write, missing)])
    );
    server.assert_holds(&second).await;
}

/// Catches a blob that could not be read counted as read, or failures not counted:
/// nothing of this seed was sent, so every blob but the empty one is missing. Ten
/// messages are kept.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reading_blobs_never_sent_fails_each_one() {
    let server = Server::start("never-sent");
    let tree = tree::generate(&SHAPE, 5).expect("tree");
    let missing = tree.blobs.len() as u64 - 1;
    for method in [Method::Bytestream, Method::Batch] {
        let got = read(&server.reapi_target(), &tree, options(method)).await;
        assert!(!got.complete());
        assert_eq!((got.blobs_done, got.failed), (1, missing), "{method:?}");
        assert_eq!(got.errors.len(), 10);
        assert!(
            got.table().contains(&format!("failed: {missing}\n  ")),
            "{}",
            got.table()
        );
    }
}

/// Catches a call the worker listener refused (no client certificate) counted as done,
/// for every kind of call.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_call_without_a_node_certificate_fails() {
    let server = Server::start("anonymous");
    let tree = tree::generate(
        &Shape {
            files: 20,
            empty: 1,
            dirs: 2,
            total_bytes: 20_000,
        },
        6,
    )
    .expect("tree");
    let mut target = server.worker_target();
    (target.cert, target.key) = (None, None);
    let mut find = options(Method::Bytestream);
    find.find_missing = true;
    let runs = [
        upload(&target, &tree, options(Method::Bytestream)).await,
        upload(&target, &tree, options(Method::Batch)).await,
        upload(&target, &tree, find).await,
        read(&target, &tree, options(Method::Bytestream)).await,
        read(&target, &tree, options(Method::Batch)).await,
    ];
    for report in runs {
        assert!(!report.complete(), "{}", report.table());
        assert_eq!(report.blobs_done, 0);
        assert_eq!(report.failed, report.requests_planned, "{}", report.table());
        assert!(report.failed > 0);
    }
}

/// Catches a deadline that does not stop a run whose server never answers (the run
/// would hang, as the pilot's upload did), and a timed-out run reported complete. The
/// progress line is printed from the start.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_deadline_stops_a_run_the_server_never_answers() {
    // Accepts connections (the kernel does) and never speaks HTTP/2.
    let silent = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let target = plain(silent.local_addr().expect("addr"));
    let tree = tree::generate(
        &Shape {
            files: 10,
            empty: 0,
            dirs: 0,
            total_bytes: 1000,
        },
        7,
    )
    .expect("tree");
    for find_missing in [true, false] {
        let mut options = options(Method::Bytestream);
        options.find_missing = find_missing;
        options.timeout = Some(Duration::from_millis(500));
        options.progress = Some(Duration::from_millis(100));
        let report = tokio::time::timeout(Duration::from_secs(20), upload(&target, &tree, options))
            .await
            .expect("the deadline stopped the run");
        assert!(report.timed_out);
        assert!(!report.complete());
        assert_eq!(report.requests_done, 0);
        assert!(report.wall_secs >= 0.5, "{}", report.wall_secs);
        assert!(
            report.table().contains("failed: 0, timed out\n"),
            "{}",
            report.table()
        );
    }
}

/// Answers every read with bytes the digest does not name.
struct Liar;

#[tonic::async_trait]
impl ByteStream for Liar {
    type ReadStream = stream::Iter<std::vec::IntoIter<Result<ReadResponse, Status>>>;

    async fn read(&self, _: Request<ReadRequest>) -> Result<Response<Self::ReadStream>, Status> {
        let lie = ReadResponse {
            data: b"lie".to_vec(),
        };
        Ok(Response::new(stream::iter(vec![Ok(lie)])))
    }

    async fn write(
        &self,
        _: Request<Streaming<WriteRequest>>,
    ) -> Result<Response<WriteResponse>, Status> {
        Err(Status::unimplemented("liar"))
    }

    async fn query_write_status(
        &self,
        _: Request<QueryWriteStatusRequest>,
    ) -> Result<Response<QueryWriteStatusResponse>, Status> {
        Err(Status::unimplemented("liar"))
    }
}

#[tonic::async_trait]
impl ContentAddressableStorage for Liar {
    async fn find_missing_blobs(
        &self,
        _: Request<FindMissingBlobsRequest>,
    ) -> Result<Response<FindMissingBlobsResponse>, Status> {
        Err(Status::unimplemented("liar"))
    }

    async fn batch_update_blobs(
        &self,
        _: Request<BatchUpdateBlobsRequest>,
    ) -> Result<Response<BatchUpdateBlobsResponse>, Status> {
        Err(Status::unimplemented("liar"))
    }

    async fn batch_read_blobs(
        &self,
        request: Request<BatchReadBlobsRequest>,
    ) -> Result<Response<BatchReadBlobsResponse>, Status> {
        let responses = request
            .into_inner()
            .digests
            .into_iter()
            .map(|digest| batch_read_blobs_response::Response {
                digest: Some(digest),
                data: b"lie".to_vec(),
                status: Some(rpc::Status::default()),
                ..batch_read_blobs_response::Response::default()
            })
            .collect();
        Ok(Response::new(BatchReadBlobsResponse { responses }))
    }

    type GetTreeStream = stream::Iter<std::vec::IntoIter<Result<GetTreeResponse, Status>>>;

    async fn get_tree(
        &self,
        _: Request<GetTreeRequest>,
    ) -> Result<Response<Self::GetTreeStream>, Status> {
        Err(Status::unimplemented("liar"))
    }

    async fn split_blob(
        &self,
        _: Request<SplitBlobRequest>,
    ) -> Result<Response<SplitBlobResponse>, Status> {
        Err(Status::unimplemented("liar"))
    }

    type GetChunkMappingStream =
        stream::Iter<std::vec::IntoIter<Result<GetChunkMappingResponse, Status>>>;

    async fn get_chunk_mapping(
        &self,
        _: Request<GetChunkMappingRequest>,
    ) -> Result<Response<Self::GetChunkMappingStream>, Status> {
        Err(Status::unimplemented("liar"))
    }

    async fn splice_blob(
        &self,
        _: Request<SpliceBlobRequest>,
    ) -> Result<Response<SpliceBlobResponse>, Status> {
        Err(Status::unimplemented("liar"))
    }

    async fn register_chunk_mapping(
        &self,
        _: Request<Streaming<RegisterChunkMappingRequest>>,
    ) -> Result<Response<RegisterChunkMappingResponse>, Status> {
        Err(Status::unimplemented("liar"))
    }
}

/// Catches a read that trusts the server: bytes that do not hash to the digest asked
/// for are a failure, by either method.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_read_of_other_bytes_fails() {
    let incoming = TcpIncoming::bind(SocketAddr::from(([127, 0, 0, 1], 0))).expect("bind");
    let target = plain(incoming.local_addr().expect("addr"));
    tokio::spawn(
        TonicServer::builder()
            .add_service(ByteStreamServer::new(Liar))
            .add_service(ContentAddressableStorageServer::new(Liar))
            .serve_with_incoming(incoming),
    );
    let tree = tree::generate(
        &Shape {
            files: 10,
            empty: 0,
            dirs: 0,
            total_bytes: 1000,
        },
        8,
    )
    .expect("tree");
    for method in [Method::Bytestream, Method::Batch] {
        let got = read(&target, &tree, options(method)).await;
        assert_eq!(
            (got.blobs_done, got.failed),
            (0, 11),
            "{method:?}: {}",
            got.table()
        );
    }
}
