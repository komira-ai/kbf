//! `kbf storage-and-daemon` on a farm of two, end to end: a real frontend over SQLite, two
//! storage nodes that register with it, and for each a daemon connected to the frontend
//! whose data plane is the one that mode gives its daemon half ([`member_cas`]) over its
//! own node's storage and its own node's map.
//!
//! The proof of the fix: a blob the daemon beside node A writes and the map places on node
//! B lands on B (and on no other node), where B's daemon reads it; a blob the map places on
//! A is written to A's storage with no call through the node dialler at all. Before this,
//! the daemon half wrote to its own node whatever the map said, and the other nodes' daemons
//! then looked for the blob at its owner and missed. The tests wait with deadlines that only
//! end a hang, and assert no duration.

mod support;

use std::future::pending;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use kbf_daemon::cas::Cas as _;
use kbf_daemon::{
    Daemon, DaemonConfig, FakeRuntime, LocalMember, NodeReport, Runtime, TlsFiles, member_cas,
};
use kbf_map::{Member, MemberId};
use kbf_mapsync::MapHolder;
use kbf_route::Nodes;
use kbf_storage::{Storage, StorageError};
use kbf_transfer::{CasError, RoutedCas, TransferConfig, proto_digest};
use support::farm::{NodeUnderTest, Pki, blob_owned_by, digest, front, map_of, node};
use support::scratch;

/// The node dialler a daemon half calls other members through, counting its calls by
/// member address: a call here is a call over the network.
struct Counted {
    inner: Arc<dyn Nodes>,
    dialled: Mutex<Vec<String>>,
}

impl Counted {
    fn dialled(&self) -> Vec<String> {
        self.dialled
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

impl Nodes for Counted {
    fn node(&self, member: &Member, generation: u64) -> Result<Arc<dyn Storage>, StorageError> {
        self.dialled
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(member.addr.clone());
        self.inner.node(member, generation)
    }
}

/// One storage-and-daemon process: its node, its daemon half's map and its data plane.
struct Process {
    node: NodeUnderTest,
    id: MemberId,
    cas: Arc<RoutedCas>,
    dialled: Arc<Counted>,
    tls: TlsFiles,
}

/// A daemon connected to the frontend at `front`, as `name`, holding the map it is sent.
fn daemon(pki: &Pki, front: &support::farm::Front, name: &str) -> (Arc<MapHolder>, TlsFiles) {
    let own = pki.cert(name, "daemon", &[name]);
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
        name.to_owned(),
    );
    config.reconnect_after = Duration::from_millis(100);
    let held = Arc::new(MapHolder::new());
    let daemon = Daemon::new(config, runtime, report)
        .expect("daemon config")
        .with_map(Arc::clone(&held));
    tokio::spawn(daemon.run(pending()));
    (held, tls)
}

/// The process of `node`: its daemon half's plane over `held`, its own member in-process.
fn process(node: NodeUnderTest, id: MemberId, held: Arc<MapHolder>, tls: &TlsFiles) -> Process {
    let local = LocalMember::new(
        node.addr.to_string(),
        Arc::clone(&node.storage),
        Arc::clone(&node.maps),
    );
    let dialled = Arc::new(Counted {
        inner: kbf_daemon::storage_nodes(tls).expect("TLS files"),
        dialled: Mutex::default(),
    });
    let cas = member_cas(
        held,
        local,
        Arc::clone(&dialled) as Arc<dyn Nodes>,
        &TransferConfig::default(),
    );
    Process {
        node,
        id,
        cas,
        dialled,
        tls: tls.clone(),
    }
}

/// A farm of two storage-and-daemon processes, A and B, each holding the two-member map,
/// and the frontend, which the farm lasts as long as.
async fn farm(name: &str) -> (Process, Process, Arc<kbf_map::Map>, support::farm::Front) {
    let dir = scratch(name);
    let pki = Pki::new(&dir);
    let front = front(&pki, &dir);
    let a = node(&pki, &front, "node-a", &dir.join("a")).await;
    let b = node(&pki, &front, "node-b", &dir.join("b")).await;
    let (held_a, tls_a) = daemon(&pki, &front, "daemon-a");
    let (held_b, tls_b) = daemon(&pki, &front, "daemon-b");
    let map = map_of("daemon A", &held_a, 2).await;
    map_of("daemon B", &held_b, 2).await;
    for node in [&a, &b] {
        map_of("a node", &node.maps, 2).await;
    }
    let id = |addr: std::net::SocketAddr| {
        map.members()
            .iter()
            .find(|m| m.addr == addr.to_string())
            .expect("the node is in the map at the address it bound")
            .id
    };
    let (id_a, id_b) = (id(a.addr), id(b.addr));
    assert_ne!(id_a, id_b);
    (
        process(a, id_a, held_a, &tls_a),
        process(b, id_b, held_b, &tls_b),
        map,
        front,
    )
}

/// Catches a daemon half that writes to its own node whatever the map says (the defect:
/// every blob its daemon wrote stayed on its node, and another node's daemon, reading by
/// the map, missed it), and a daemon half that dials a network address for a blob its own
/// node owns. A's daemon writes a blob owned by B: it lands on B and not on A, B's daemon
/// reads it with no call to any node, and A's daemon reads it from B. A's daemon writes a
/// blob owned by A: it lands on A, and A's daemon dialled no node for it, to write or to
/// read.
///
/// Mutants: a node set that returns the local storage for every member (the daemon half
/// always writes locally: the first blob lands on A, the dial count of A is 0 and B holds
/// nothing).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_daemon_half_writes_where_the_map_says_and_in_process_for_its_own_node() {
    let (a, b, map, _front) = farm("member-wiring").await;

    // Owned by B, written by A's daemon half.
    let bytes = blob_owned_by(&map, b.id, "b");
    let blob = digest(&bytes);
    let stored = a.cas.put(bytes.clone()).await.expect("stored");
    assert_eq!(stored, proto_digest(&blob));
    assert!(b.node.holds(&blob).await, "the blob is on its owner, B");
    assert!(!a.node.holds(&blob).await, "and not on A, which wrote it");
    assert_eq!(
        a.dialled.dialled(),
        [b.node.addr.to_string()],
        "A dialled B once and no one else"
    );
    // B's daemon half finds it on B, in-process.
    assert_eq!(b.cas.get(&stored).await.expect("B reads it"), bytes);
    assert_eq!(
        b.dialled.dialled(),
        Vec::<String>::new(),
        "B dialled no one"
    );
    // A's reads it from B.
    assert_eq!(a.cas.get(&stored).await.expect("A reads it"), bytes);

    // Owned by A, written by A's daemon half: no call to any node.
    let before = a.dialled.dialled().len();
    let bytes = blob_owned_by(&map, a.id, "a");
    let blob = digest(&bytes);
    let stored = a.cas.put(bytes.clone()).await.expect("stored");
    assert!(a.node.holds(&blob).await, "the blob is on its owner, A");
    assert!(!b.node.holds(&blob).await, "and not on B");
    assert_eq!(a.cas.get(&stored).await.expect("read back"), bytes);
    assert_eq!(
        a.dialled.dialled().len(),
        before,
        "a blob A owns crossed no network"
    );
    // B's daemon half reads it from A, over the network.
    assert_eq!(b.cas.get(&stored).await.expect("B reads it"), bytes);
    assert_eq!(b.dialled.dialled(), [a.node.addr.to_string()]);
}

/// A plane with the map of `source`'s node held in a holder of its own, and the local
/// member `local` whose node holds `node_map`.
fn plane_over(
    daemon_map: Option<&kbf_map::Map>,
    local: &Process,
    node_map: &Arc<MapHolder>,
    tls: &TlsFiles,
) -> Arc<RoutedCas> {
    let held = Arc::new(MapHolder::new());
    if let Some(map) = daemon_map {
        assert!(held.offer(map.clone()));
    }
    let member = LocalMember::new(
        local.node.addr.to_string(),
        Arc::clone(&local.node.storage),
        Arc::clone(node_map),
    );
    let nodes = kbf_daemon::storage_nodes(tls).expect("TLS files");
    member_cas(held, member, nodes, &TransferConfig::default())
}

/// Catches the no-map rule not holding. Until the daemon half holds a map, a write fails
/// UNAVAILABLE and reaches no node, its own included (the defect: a daemon with no map
/// wrote to its own node as if it were the owner). Until the storage half holds a map,
/// its own member is refused as its listener refuses every call, and a blob owned by
/// another member goes on to its owner.
///
/// Mutants: the gate on the node's own map removed (the second write lands on A with the
/// node's map empty); the router asked for a local write when no map is held (the first
/// write lands on A).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_daemon_half_writes_nowhere_until_it_and_its_node_hold_a_map() {
    let (a, b, map, _front) = farm("member-no-map").await;
    let tls = a.tls.clone();
    let own = blob_owned_by(&map, a.id, "own");
    let own_digest = digest(&own);

    // No map for the daemon half: nothing is written, not even to its own node.
    let cas = plane_over(None, &a, &a.node.maps, &tls);
    let refused = cas.put(own.clone()).await.expect_err("no map");
    assert!(matches!(refused, CasError::Unavailable(..)), "{refused:?}");
    assert!(!a.node.holds(&own_digest).await, "nothing reached A");

    // A map for the daemon half, none for its node: its own member is refused ...
    let empty = Arc::new(MapHolder::new());
    let cas = plane_over(Some(&map), &a, &empty, &tls);
    let refused = cas.put(own.clone()).await.expect_err("the node has no map");
    assert!(matches!(refused, CasError::Unavailable(..)), "{refused:?}");
    assert!(
        refused.to_string().contains("no storage map yet"),
        "{refused}"
    );
    assert!(!a.node.holds(&own_digest).await, "nothing reached A");
    // ... and a blob another member owns is not.
    let theirs = blob_owned_by(&map, b.id, "theirs");
    cas.put(theirs.clone()).await.expect("B takes it");
    assert!(b.node.holds(&digest(&theirs)).await);

    // The node holds a map: its own member is used.
    let cas = plane_over(Some(&map), &a, &a.node.maps, &tls);
    cas.put(own).await.expect("A takes it");
    assert!(a.node.holds(&own_digest).await);
}
