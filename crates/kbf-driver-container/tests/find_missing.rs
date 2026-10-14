//! The container driver's `collect` through the daemon's transfer engine to kbf-front's
//! CAS: how many FindMissing calls a large output tree costs, and how its missing
//! blobs are sent (one segment upload where the server serves `Segments`, one
//! ByteStream write each where it does not).

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use kbf_daemon::cas::{Cas, digest_of};
use kbf_driver_container::tree::{OutputLimits, collect};
use kbf_front::{Cache, Connections, MemoryMetaLog, SegmentsConfig, SegmentsService, VerifiedBlob};
use kbf_objstore::MemoryStore;
use kbf_proto::reapi::ActionResult;
use kbf_proto::worker::segments_server::{Segments, SegmentsServer};
use kbf_proto::worker::{
    BeginRequest, BeginResponse, CommitRequest, CommitResponse, LocateRequest, LocateResponse,
    PutRangeRequest, PutRangeResponse, ReadRangeRequest, ReadRangeResponse,
};
use kbf_transfer::{TransferConfig, Transfers};
use kbf_types::LeaseId;
use tonic::service::Routes;
use tonic::transport::server::TcpIncoming;
use tonic::transport::{Endpoint, Server};
use tonic::{Request, Response, Status};

/// Serves `routes` on a loopback port.
async fn serve(routes: Routes) -> SocketAddr {
    let incoming = TcpIncoming::bind(SocketAddr::from(([127, 0, 0, 1], 0))).expect("bind");
    let addr = incoming.local_addr().expect("addr");
    tokio::spawn(
        Server::builder()
            .add_routes(routes)
            .serve_with_incoming(incoming),
    );
    addr
}

/// The upload of a large output tree the server mostly holds, the shape of an
/// unpacked toolchain rebuilt (on the farm 14,245 distinct outputs, 13,529 of them
/// held, took 785 s to upload: each upload asked FindMissing for itself, 64 uploads at
/// a time). 15,000 files in one directory, the
/// server holding 14,000: 15,002 distinct digests with the Directory and the Tree, so
/// 16 FindMissing calls at the engine's chunk of 1000; before this change, each upload
/// asking for itself, this test saw 783. The lease's counters keep their meaning:
/// every digest asked about once, the 14,000 held ones present, the others written.
/// Mutants seen red: no `missing` call, each put asking for itself (516 calls); the
/// answer ignored, every blob put (510 calls: the held ones ask for themselves).
/// This front serves no `Segments`: the spool's upload is answered UNIMPLEMENTED and
/// its blobs go one ByteStream write each, so this is also the fallback's test.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_large_tree_costs_one_find_missing_call_per_thousand_digests() {
    let upper = std::path::Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("kbf-driver-container-find-missing")
        .join(std::process::id().to_string());
    let _ = std::fs::remove_dir_all(&upper);
    std::fs::create_dir_all(upper.join("out")).expect("mkdir");
    let cache = Arc::new(Cache::memory());
    let mut held = Vec::new();
    for f in 0..15_000 {
        let bytes = format!("output file {f}").into_bytes();
        std::fs::write(upper.join(format!("out/f{f:05}")), &bytes).expect("write");
        if f % 15 != 0 {
            held.push(VerifiedBlob::hashed(bytes.into()));
        }
    }
    cache.store_blobs(held).await.expect("the held blobs");
    let addr = serve(kbf_front::routes(Arc::clone(&cache))).await;
    let endpoint = Endpoint::from_shared(format!("http://{addr}")).expect("endpoint");
    let engine = Arc::new(Transfers::new(endpoint, TransferConfig::default()));
    let scope = engine.lease(&LeaseId::new(9, 1));
    let mut result = ActionResult::default();
    let paths = ["out".to_owned()];
    collect(
        &scope,
        &upper,
        &kbf_daemon::cpu_pool::CpuPool::new(4),
        "",
        &paths,
        OutputLimits::DEFAULT,
        &mut result,
    )
    .await
    .expect("collect");
    assert_eq!(engine.stats().find_missing_calls, 16, "FindMissing calls");
    let stats = scope.stats();
    assert_eq!(
        (
            stats.find_missing_digests,
            stats.outputs_present,
            stats.written_blobs
        ),
        (15_002, 14_000, 1002)
    );
    // Everything the result names is stored: the Tree, and a file the server lacked.
    let tree = result.output_directories[0]
        .tree_digest
        .clone()
        .expect("tree");
    assert!(scope.get(&tree).await.is_ok(), "the Tree");
    let new = digest_of(b"output file 0");
    assert!(scope.get(&new).await.is_ok(), "a file the server lacked");
    let _ = std::fs::remove_dir_all(&upper);
}

/// kbf-front's `Segments`, counting the `Begin`s, the segments they declare and the
/// `Commit`s.
struct CountingSegments {
    inner: SegmentsService<MemoryMetaLog, MemoryStore>,
    begins: Arc<AtomicUsize>,
    segments: Arc<AtomicUsize>,
    commits: Arc<AtomicUsize>,
}

#[tonic::async_trait]
impl Segments for CountingSegments {
    async fn begin(&self, r: Request<BeginRequest>) -> Result<Response<BeginResponse>, Status> {
        self.begins.fetch_add(1, Ordering::SeqCst);
        let declared = r.get_ref().segments.len();
        self.segments.fetch_add(declared, Ordering::SeqCst);
        self.inner.begin(r).await
    }

    async fn put_range(
        &self,
        r: Request<PutRangeRequest>,
    ) -> Result<Response<PutRangeResponse>, Status> {
        self.inner.put_range(r).await
    }

    async fn commit(&self, r: Request<CommitRequest>) -> Result<Response<CommitResponse>, Status> {
        self.commits.fetch_add(1, Ordering::SeqCst);
        self.inner.commit(r).await
    }

    async fn locate(&self, r: Request<LocateRequest>) -> Result<Response<LocateResponse>, Status> {
        self.inner.locate(r).await
    }

    async fn read_range(
        &self,
        r: Request<ReadRangeRequest>,
    ) -> Result<Response<ReadRangeResponse>, Status> {
        self.inner.read_range(r).await
    }
}

/// The same tree as above against a front that serves `Segments`, the way a lease's
/// outputs reach a current server: the 1002 blobs the server lacks go as one segment
/// upload (one `Begin` declaring one segment, one `Commit`), not 1002 ByteStream
/// writes, and FindMissing is still asked 16 times (the spool's upload does not ask
/// again about digests `missing` answered). The lease's counters keep their meaning.
/// Mutant seen red: `Pending::store_missing` storing each missing blob with
/// `put`/`put_chunks` on the lease's store (the per-file path): 0 `Begin`s and 1002
/// ByteStream writes.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_large_tree_is_one_segment_upload() {
    let upper = std::path::Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("kbf-driver-container-segments")
        .join(std::process::id().to_string());
    let _ = std::fs::remove_dir_all(&upper);
    std::fs::create_dir_all(upper.join("out")).expect("mkdir");
    let cache = Arc::new(Cache::memory());
    let mut held = Vec::new();
    for f in 0..15_000 {
        let bytes = format!("output file {f}").into_bytes();
        std::fs::write(upper.join(format!("out/f{f:05}")), &bytes).expect("write");
        if f % 15 != 0 {
            held.push(VerifiedBlob::hashed(bytes.into()));
        }
    }
    cache.store_blobs(held).await.expect("the held blobs");
    let (begins, segments, commits) = Default::default();
    let counting = CountingSegments {
        inner: SegmentsService::new(
            Arc::clone(&cache),
            SegmentsConfig::DEFAULT,
            Connections::new(),
        ),
        begins: Arc::clone(&begins),
        segments: Arc::clone(&segments),
        commits: Arc::clone(&commits),
    };
    let routes = kbf_front::routes(Arc::clone(&cache)).add_service(
        SegmentsServer::new(counting)
            .max_decoding_message_size(kbf_front::MAX_MESSAGE_BYTES)
            .max_encoding_message_size(kbf_front::MAX_MESSAGE_BYTES),
    );
    let addr = serve(routes).await;
    let endpoint = Endpoint::from_shared(format!("http://{addr}")).expect("endpoint");
    let engine = Arc::new(Transfers::new(endpoint, TransferConfig::default()));
    let scope = engine.lease(&LeaseId::new(9, 2));
    let mut result = ActionResult::default();
    let paths = ["out".to_owned()];
    collect(
        &scope,
        &upper,
        &kbf_daemon::cpu_pool::CpuPool::new(4),
        "",
        &paths,
        OutputLimits::DEFAULT,
        &mut result,
    )
    .await
    .expect("collect");
    let calls = (
        begins.load(Ordering::SeqCst),
        segments.load(Ordering::SeqCst),
        commits.load(Ordering::SeqCst),
        cache.metrics().write.total.get(),
    );
    assert_eq!(
        calls,
        (1, 1, 1, 0),
        "Begins, segments, Commits, ByteStream writes"
    );
    assert_eq!(engine.stats().find_missing_calls, 16, "FindMissing calls");
    let stats = scope.stats();
    assert_eq!(
        (
            stats.find_missing_digests,
            stats.outputs_present,
            stats.written_blobs
        ),
        (15_002, 14_000, 1002)
    );
    let tree = result.output_directories[0]
        .tree_digest
        .clone()
        .expect("tree");
    assert!(scope.get(&tree).await.is_ok(), "the Tree");
    let new = digest_of(b"output file 0");
    assert!(scope.get(&new).await.is_ok(), "a file the server lacked");
    let _ = std::fs::remove_dir_all(&upper);
}
