//! The container driver's `collect` through the daemon's transfer engine to kbf-front's
//! CAS: how many FindMissing calls a large output tree costs.

use std::net::SocketAddr;
use std::sync::Arc;

use kbf_daemon::cas::{Cas, digest_of};
use kbf_driver_container::tree::{OutputLimits, collect};
use kbf_front::{Cache, VerifiedBlob};
use kbf_proto::reapi::ActionResult;
use kbf_transfer::{TransferConfig, Transfers};
use kbf_types::LeaseId;
use tonic::service::Routes;
use tonic::transport::server::TcpIncoming;
use tonic::transport::{Endpoint, Server};

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
