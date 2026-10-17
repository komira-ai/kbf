//! The storage map, end to end, through the real binaries' start paths: a `kbf frontend`
//! with a SQLite state store, two storage nodes that register with it
//! (`kbf_server::storage_half::start`, the storage half of `kbf storage-and-daemon`, which
//! registers as `kbf storage --frontend` does), and a daemon connected to the frontend's
//! worker listener. Everything is over mutual TLS with certificates that name their roles.
//!
//! The proof of the rehearsal: the frontend hands the members' map to the daemon with no
//! test in the middle, the daemon's data plane ([`FarmCas`]) then writes a blob to its
//! rendezvous owner and to no other node, and reads it back; each node holds the map its
//! own registration and heartbeats brought it. The tests wait with deadlines that only end
//! a hang, and assert no duration.

mod support;

use std::future::pending;
use std::sync::Arc;
use std::time::Duration;

use kbf_daemon::cas::Cas as _;
use kbf_daemon::{Daemon, DaemonConfig, FakeRuntime, FarmCas, NodeReport, Runtime, TlsFiles};
use kbf_mapsync::MapHolder;
use kbf_transfer::{TransferConfig, Transfers, proto_digest};
use support::farm::{Pki, blob_owned_by, digest, front, map_of, node};
use support::scratch;
use tonic::transport::Endpoint;

/// Catches a farm whose map stops at the frontend: both nodes register, the frontend hands
/// the daemon the map of both members (`Welcome.map` or a `MapUpdate`) and each node holds
/// the map its heartbeat brings; the daemon's blobs then go to their rendezvous owner and
/// nowhere else, and read back. Mutants: the frontend started with no map source (the
/// daemon never holds a map: the wait for it never ends); a node that does not keep the map
/// it is sent (its holder stays empty); a plane that writes through the frontend instead of
/// the owner (a blob lands on neither node).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_blob_written_by_a_daemon_lands_on_its_owner_through_the_maps_the_frontend_sends() {
    let dir = scratch("map-wiring");
    let pki = Pki::new(&dir);
    let front = front(&pki, &dir);
    let a = node(&pki, &front, "node-a", &dir.join("a")).await;
    let b = node(&pki, &front, "node-b", &dir.join("b")).await;

    // The daemon connects with its own certificate, and holds what it is sent.
    let own = pki.cert("daemon", "daemon", &["node-1"]);
    let tls = TlsFiles {
        ca_cert: pki.ca(),
        cert: own.cert,
        key: own.key,
        server_name: Some("localhost".to_owned()),
    };
    let runtime = Arc::new(FakeRuntime::new(Duration::ZERO));
    let report = NodeReport::detect(&[runtime.driver()]).expect("detect this node");
    let mut config = DaemonConfig::new(
        format!("https://localhost:{}", front.worker.port()),
        tls.clone(),
        "node-1".to_owned(),
    );
    config.reconnect_after = Duration::from_millis(100);
    let held = Arc::new(MapHolder::new());
    let daemon = Daemon::new(config, runtime, report)
        .expect("daemon config")
        .with_map(Arc::clone(&held));
    tokio::spawn(daemon.run(pending()));

    // The frontend pushed it: both members, generation 1 or later.
    let map = map_of("the daemon", &held, 2).await;
    assert!(map.generation() >= 1);
    let (id_a, id_b) = {
        let id = |addr: std::net::SocketAddr| {
            map.members()
                .iter()
                .find(|m| m.addr == addr.to_string())
                .expect("the node is in the map at the address it bound")
                .id
        };
        (id(a.addr), id(b.addr))
    };
    assert_ne!(id_a, id_b);

    // The nodes hold the map their heartbeats brought.
    for node in [&a, &b] {
        let at = map_of("a node", &node.maps, 2).await;
        assert!(at.generation() >= 1);
    }

    // The daemon's plane, over the map it was pushed.
    let nodes = kbf_daemon::storage_nodes(&tls).expect("TLS files");
    let channel = Endpoint::from_shared("http://127.0.0.1:1")
        .expect("endpoint")
        .connect_lazy();
    let engine = Arc::new(Transfers::over(channel, TransferConfig::default()));
    let cas = Arc::new(FarmCas::new(
        engine,
        Arc::clone(&held),
        nodes,
        &TransferConfig::default(),
    ));
    for (owner, other, tag) in [(id_a, &b, "a"), (id_b, &a, "b")] {
        let here = if owner == id_a { &a } else { &b };
        let elsewhere = other;
        let bytes = blob_owned_by(&map, owner, tag);
        let stored = cas.put(bytes.clone()).await.expect("stored");
        let blob = digest(&bytes);
        assert_eq!(stored, proto_digest(&blob), "the digest it answered");
        assert!(here.holds(&blob).await, "{tag}: on its rendezvous owner");
        // The primary only: the chain that copies to the second owner is av2-11's.
        assert!(!elsewhere.holds(&blob).await, "{tag}: and not elsewhere");
        assert_eq!(cas.get(&stored).await.expect("read back"), bytes);
    }
}
