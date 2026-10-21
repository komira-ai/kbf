//! The conformance checks against the remote implementation: [`Transfers`] over one
//! HTTP/2 connection to kbf-front's real CAS and `Segments` services, served on a
//! loopback port of this process. The in-process implementation a combined binary will
//! have runs the same checks (`kbf_cas_conformance::tests!`).

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use kbf_cas_conformance::Subject;
use kbf_front::{Cache, Connections, SegmentsConfig, SegmentsService};
use kbf_proto::worker::segments_server::SegmentsServer;
use kbf_transfer::{TransferConfig, Transfers};
use tonic::transport::server::TcpIncoming;
use tonic::transport::{Endpoint, Server};

/// A server per [`Subject::fresh`], over an empty in-memory cache.
struct Remote {
    scratch: PathBuf,
}

impl Remote {
    fn new() -> Self {
        let scratch = Path::new(env!("CARGO_TARGET_TMPDIR")).join("remote-conformance");
        std::fs::create_dir_all(&scratch).expect("scratch directory");
        Self { scratch }
    }
}

/// kbf-front's services over a new cache, on a loopback port.
async fn serve() -> SocketAddr {
    let cache = Arc::new(Cache::memory());
    let segments = SegmentsServer::new(SegmentsService::new(
        Arc::clone(&cache),
        SegmentsConfig::DEFAULT,
        Connections::new(),
    ))
    .max_decoding_message_size(kbf_front::MAX_MESSAGE_BYTES)
    .max_encoding_message_size(kbf_front::MAX_MESSAGE_BYTES);
    let routes = kbf_front::routes(cache).add_service(segments);
    let incoming = TcpIncoming::bind(SocketAddr::from(([127, 0, 0, 1], 0))).expect("bind");
    let addr = incoming.local_addr().expect("address");
    tokio::spawn(
        Server::builder()
            .add_routes(routes)
            .serve_with_incoming(incoming),
    );
    addr
}

impl Subject for Remote {
    type Cas = Transfers;

    async fn fresh(&self) -> Arc<Transfers> {
        let endpoint =
            Endpoint::from_shared(format!("http://{}", serve().await)).expect("endpoint");
        Arc::new(Transfers::new(endpoint, TransferConfig::default()))
    }

    fn scratch(&self) -> &Path {
        &self.scratch
    }
}

kbf_cas_conformance::tests!(Remote::new());
