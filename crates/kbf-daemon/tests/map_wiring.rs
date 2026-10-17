//! The storage map, end to end, through the real binaries' start paths: a `kbf frontend`
//! with a SQLite state store, two storage nodes that register with it
//! (`kbf_server::storage_half::start`, the storage half of `kbf storage-and-daemon`, which
//! registers as `kbf storage --frontend` does), and a daemon connected to the frontend's
//! worker listener. Everything is over mutual TLS with certificates that name their roles.
//!
//! The proof of the rehearsal: the frontend hands the members' map to the daemon with no
//! test in the middle, the daemon's data plane ([`FarmCas`]) then writes a blob to its
//! first owner, which copies it to the second (R = 2), and reads it back; each node holds
//! the map its own registration and heartbeats brought it. With the second owner down the
//! write is still answered, the first owner owes the copy in its repair journal, and its
//! repair worker makes it when the owner returns. The tests wait with deadlines that only
//! end a hang, and assert no duration.

mod support;

use std::future::{Future, pending};
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use kbf_daemon::cas::Cas as _;
use kbf_daemon::{Daemon, DaemonConfig, FakeRuntime, FarmCas, NodeReport, Runtime, TlsFiles};
use kbf_map::{Map, MemberId};
use kbf_mapsync::MapHolder;
use kbf_server::frontend::{self, FrontendHandle, start_frontend};
use kbf_server::storage_half::{self, StorageHalf};
use kbf_server::storage_node;
use kbf_storage::Storage;
use kbf_storage::repair::Repairs;
use kbf_storage::service::ROLE_URI_PREFIX;
use kbf_transfer::{TransferConfig, Transfers, proto_digest};
use kbf_types::Digest;
use rcgen::{
    BasicConstraints, CertificateParams, CertifiedIssuer, DnType, ExtendedKeyUsagePurpose, IsCa,
    KeyPair, KeyUsagePurpose, SanType,
};
use support::scratch;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio::time::timeout;
use tonic::transport::Endpoint;

/// A deadline that only ends a hang.
const HANG: Duration = Duration::from_secs(40);

/// The certificate authority of the test farm and the files its members use.
struct Pki {
    ca: CertifiedIssuer<'static, KeyPair>,
    dir: PathBuf,
}

/// The paths of one member's certificate and key.
struct Files {
    cert: PathBuf,
    key: PathBuf,
}

impl Pki {
    fn new(dir: &Path) -> Self {
        let mut ca = CertificateParams::new(Vec::<String>::new()).expect("CA params");
        ca.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        ca.distinguished_name
            .push(DnType::CommonName, "kbf test CA");
        ca.key_usages = vec![
            KeyUsagePurpose::KeyCertSign,
            KeyUsagePurpose::DigitalSignature,
        ];
        let ca = CertifiedIssuer::self_signed(ca, KeyPair::generate().expect("key")).expect("CA");
        std::fs::write(dir.join("ca.pem"), ca.pem()).expect("write the CA");
        Self {
            ca,
            dir: dir.to_owned(),
        }
    }

    fn ca(&self) -> PathBuf {
        self.dir.join("ca.pem")
    }

    /// A certificate for the DNS names `dns` (and 127.0.0.1) that names `role`, good for
    /// serving and for calling. A daemon's names exactly one node, which the frontend reads.
    fn cert(&self, name: &str, role: &str, dns: &[&str]) -> Files {
        let mut params =
            CertificateParams::new(dns.iter().map(|d| (*d).to_owned()).collect::<Vec<_>>())
                .expect("params");
        params
            .subject_alt_names
            .push(SanType::IpAddress(IpAddr::from([127, 0, 0, 1])));
        params.subject_alt_names.push(SanType::URI(
            format!("{ROLE_URI_PREFIX}{role}").try_into().expect("IA5"),
        ));
        params.distinguished_name.push(DnType::CommonName, name);
        params.extended_key_usages = vec![
            ExtendedKeyUsagePurpose::ServerAuth,
            ExtendedKeyUsagePurpose::ClientAuth,
        ];
        let key = KeyPair::generate().expect("key");
        let cert = params.signed_by(&key, &self.ca).expect("sign");
        let files = Files {
            cert: self.dir.join(format!("{name}.pem")),
            key: self.dir.join(format!("{name}.key")),
        };
        std::fs::write(&files.cert, cert.pem()).expect("write a certificate");
        std::fs::write(&files.key, key.serialize_pem()).expect("write a key");
        files
    }
}

/// The frontend, started as `kbf frontend --state-store=sqlite://... ` starts it, serving
/// until dropped.
struct Front {
    worker: std::net::SocketAddr,
    _stop: oneshot::Sender<()>,
}

fn front(pki: &Pki, dir: &Path) -> Front {
    let own = pki.cert("frontend", "frontend", &["localhost"]);
    let argv = [
        "frontend".to_owned(),
        "--listen=127.0.0.1:0".to_owned(),
        "--worker-listen=127.0.0.1:0".to_owned(),
        // Never dialled: this test sends the frontend no REAPI call.
        "--storage=http://127.0.0.1:1".to_owned(),
        format!("--state-store=sqlite://{}", dir.join("state.db").display()),
        // A beat every second, a row that outlives a test.
        "--member-ttl-secs=3".to_owned(),
        "--member-dwell-secs=600".to_owned(),
        format!("--worker-tls-cert={}", own.cert.display()),
        format!("--worker-tls-key={}", own.key.display()),
        format!("--worker-client-ca={}", pki.ca().display()),
    ];
    let flags = frontend::parse(
        &frontend::command()
            .try_get_matches_from(argv)
            .expect("the frontend's flags"),
    )
    .expect("parses");
    let (stop, stopped) = oneshot::channel::<()>();
    let handle: FrontendHandle = start_frontend(&flags, || {
        Ok(async move {
            let _ = stopped.await;
        })
    })
    .expect("starts");
    let worker = handle.worker;
    tokio::spawn(handle.serve());
    Front {
        worker,
        _stop: stop,
    }
}

/// A storage node that registers with `front`, started as `kbf storage-and-daemon` starts
/// its storage half, serving in the background until stopped or dropped. It listens on
/// `listen` (a restarted node keeps its address, and so its member).
async fn try_node(
    pki: &Pki,
    front: &Front,
    name: &str,
    dir: &Path,
    listen: &str,
) -> Result<NodeUnderTest, Box<dyn std::error::Error>> {
    let own = pki.cert(name, "storage", &["localhost"]);
    let argv = [
        "storage".to_owned(),
        format!("--storage-listen={listen}"),
        format!("--cold-store=file://{}", dir.join("cold").display()),
        format!("--meta-dir={}", dir.join("meta").display()),
        format!("--frontend=https://localhost:{}", front.worker.port()),
        format!("--storage-tls-cert={}", own.cert.display()),
        format!("--storage-tls-key={}", own.key.display()),
        format!("--storage-client-ca={}", pki.ca().display()),
    ];
    let mut flags = storage_node::parse(
        &storage_node::command()
            .try_get_matches_from(argv)
            .expect("the node's flags"),
    )
    .expect("parses");
    // Passes often, so a debt is settled soon after its owner returns.
    flags.repair.interval = Duration::from_millis(50);
    let (stop, stopped) = oneshot::channel::<()>();
    let half = storage_half::start(&flags, || {
        Ok(async move {
            let _ = stopped.await;
        })
    })
    .await?;
    let (addr, storage, maps, repairs) = (half.addr, half.storage(), half.maps(), half.repairs());
    let serving = tokio::spawn(StorageHalf::serve(half));
    Ok(NodeUnderTest {
        addr,
        storage,
        maps,
        repairs,
        stop,
        serving,
    })
}

async fn node(pki: &Pki, front: &Front, name: &str, dir: &Path) -> NodeUnderTest {
    try_node(pki, front, name, dir, "127.0.0.1:0")
        .await
        .expect("the node starts")
}

struct NodeUnderTest {
    addr: std::net::SocketAddr,
    storage: Arc<dyn Storage>,
    maps: Arc<MapHolder>,
    repairs: Option<Arc<Repairs>>,
    stop: oneshot::Sender<()>,
    serving: JoinHandle<Result<(), kbf_server::ServeError>>,
}

impl NodeUnderTest {
    async fn holds(&self, digest: &Digest) -> bool {
        self.storage
            .find_missing(std::slice::from_ref(digest))
            .await
            .expect("find missing")
            .is_empty()
    }

    /// The copies this node owes.
    fn owes(&self) -> Vec<Digest> {
        self.repairs
            .as_ref()
            .expect("a node of a farm replicates, so it keeps a repair journal")
            .open_records()
            .iter()
            .map(|record| record.digest)
            .collect()
    }

    /// Stops the node and waits for its listener to end; what it held (the cache, and
    /// with it the lock on its metadata directory) is released with it.
    async fn stop(self) {
        let Self { stop, serving, .. } = self;
        let _ = stop.send(());
        serving
            .await
            .expect("the node's task ends")
            .expect("the node serves until stopped");
    }
}

/// `condition`'s answer once it has one, polled; a deadline only ends a hang.
async fn eventually<T, F: Future<Output = Option<T>>>(
    what: &str,
    mut condition: impl FnMut() -> F,
) -> T {
    let found = timeout(HANG, async {
        loop {
            if let Some(found) = condition().await {
                return found;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    found.unwrap_or_else(|_| panic!("never happened: {what}"))
}

/// The map in `holder` once it has `members` members.
async fn map_of(who: &str, holder: &MapHolder, members: usize) -> Arc<Map> {
    let mut changes = holder.changes();
    let arrived = timeout(HANG, async {
        loop {
            if let Some(map) = holder.current().filter(|m| m.members().len() == members) {
                return map;
            }
            changes.changed().await.expect("the holder is open");
        }
    })
    .await;
    arrived.unwrap_or_else(|_| {
        let held = holder
            .current()
            .map(|m| (m.generation(), m.members().len()));
        panic!("{who}: the map of {members} members never arrived; it holds {held:?}")
    })
}

/// The digest of `bytes` as the stores name it.
fn digest(bytes: &[u8]) -> Digest {
    kbf_segments::sha256(bytes)
}

/// A blob whose primary owner in `map` is `member`.
fn blob_owned_by(map: &Map, member: MemberId, tag: &str) -> Vec<u8> {
    (0_u64..100_000)
        .map(|i| format!("{tag} blob number {i}").into_bytes())
        .find(|bytes| map.write_owners(&digest(bytes)).first() == Some(&member))
        .expect("a blob this member owns")
}

/// A frontend, two storage nodes registered with it, and a daemon that holds the map the
/// frontend sent it, with its data plane.
struct Farm {
    pki: Pki,
    front: Front,
    dir: PathBuf,
    a: NodeUnderTest,
    b: NodeUnderTest,
    id_a: MemberId,
    id_b: MemberId,
    map: Arc<Map>,
    cas: Arc<FarmCas>,
}

async fn farm(name: &str) -> Farm {
    let dir = scratch(name);
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
    let id = |addr: std::net::SocketAddr| {
        map.members()
            .iter()
            .find(|m| m.addr == addr.to_string())
            .expect("the node is in the map at the address it bound")
            .id
    };
    let (id_a, id_b) = (id(a.addr), id(b.addr));
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
    Farm {
        pki,
        front,
        dir,
        a,
        b,
        id_a,
        id_b,
        map,
        cas,
    }
}

/// Catches a farm whose map stops at the frontend: both nodes register, the frontend hands
/// the daemon the map of both members (`Welcome.map` or a `MapUpdate`) and each node holds
/// the map its heartbeat brings; the daemon's blobs then go to their first owner, which
/// copies them to the second (R = 2), so both nodes hold each, and read back. Mutants: the
/// frontend started with no map source (the daemon never holds a map: the wait for it never
/// ends); a node that does not keep the map it is sent (its holder stays empty); a plane
/// that writes through the frontend instead of the owner (a blob lands on neither node); a
/// storage service that judges by the map but has no chain (the second owner never
/// holds the blob).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_blob_written_by_a_daemon_lands_on_both_its_owners_through_the_maps_the_frontend_sends() {
    let Farm {
        a,
        b,
        id_a,
        id_b,
        map,
        cas,
        ..
    } = farm("map-wiring").await;
    for (owner, tag) in [(id_a, "a"), (id_b, "b")] {
        let bytes = blob_owned_by(&map, owner, tag);
        let stored = cas.put(bytes.clone()).await.expect("stored");
        let blob = digest(&bytes);
        assert_eq!(stored, proto_digest(&blob), "the digest it answered");
        assert!(a.holds(&blob).await, "{tag}: on the first node");
        assert!(b.holds(&blob).await, "{tag}: and on the second");
        assert!(a.owes().is_empty() && b.owes().is_empty(), "{tag}: no debt");
        assert_eq!(cas.get(&stored).await.expect("read back"), bytes);
    }
}

/// Catches an R = 2 farm that loses the second copy when its owner is away, or never makes
/// it: with the second owner stopped a write is still answered (one copy, not an error),
/// the first owner keeps a record of the copy it owes, and when the second owner returns
/// (the same address, so the same member, with the cache it had) the first owner's repair
/// worker copies the blob and the record is settled. Mutants: no repair worker started (the
/// debt stays for ever); a write that fails when the second owner is away; a worker that
/// settles the debt without making the copy (the returned node does not hold the blob).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn with_the_second_owner_away_the_write_is_answered_and_the_copy_is_made_when_it_returns() {
    let Farm {
        pki,
        front,
        dir,
        a,
        b,
        id_a,
        map,
        cas,
        ..
    } = farm("map-wiring-repair").await;
    let bytes = blob_owned_by(&map, id_a, "away");
    let blob = digest(&bytes);

    // The second owner goes away; its address is kept for its return.
    let addr_b = b.addr;
    b.stop().await;

    let stored = cas.put(bytes.clone()).await.expect("stored with one copy");
    assert_eq!(stored, proto_digest(&blob));
    assert!(a.holds(&blob).await, "on the first owner");
    assert_eq!(a.owes(), [blob], "and owed to the second");

    // The second owner returns, with the files it had.
    let b = eventually("the second owner to start again", || async {
        try_node(&pki, &front, "node-b", &dir.join("b"), &addr_b.to_string())
            .await
            .ok()
    })
    .await;
    assert_eq!(b.addr, addr_b);
    eventually("the first owner's worker to make the copy", || async {
        (a.owes().is_empty() && b.holds(&blob).await).then_some(())
    })
    .await;
    assert_eq!(cas.get(&stored).await.expect("read back"), bytes);
}
