//! A real farm in one test process: the CA and certificates that name roles, a frontend
//! over SQLite and storage nodes that register with it (what `kbf storage-and-daemon`
//! starts as its storage half). Shared by tests/map_wiring.rs and tests/member_wiring.rs.

use std::future::pending;
use std::net::IpAddr;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use kbf_map::{Map, MemberId};
use kbf_mapsync::MapHolder;
use kbf_server::frontend::{self, FrontendHandle, start_frontend};
use kbf_server::storage_half::{self, StorageHalf};
use kbf_server::storage_node;
use kbf_storage::Storage;
use kbf_storage::repair::{NoLedger, Options, Repairs};
use kbf_storage::replicate::{Chain, ChainConfig, MapSource, Peers};
use kbf_storage::service::{Admission, Config, InProcess, ROLE_URI_PREFIX, StorageService};
use kbf_store::StdFs;
use kbf_types::Digest;
use rcgen::{
    BasicConstraints, CertificateParams, CertifiedIssuer, DnType, ExtendedKeyUsagePurpose, IsCa,
    KeyPair, KeyUsagePurpose, SanType,
};
use tokio::sync::oneshot;
use tokio::time::timeout;
use tonic::transport::server::TcpIncoming;
use tonic::transport::{Certificate, ClientTlsConfig, Identity, Server, ServerTlsConfig};

use super::storage_node::Spy;

/// A deadline that only ends a hang.
pub const HANG: Duration = Duration::from_secs(40);

/// The certificate authority of the test farm and the files its members use.
pub struct Pki {
    pub ca: CertifiedIssuer<'static, KeyPair>,
    pub dir: PathBuf,
}

/// The paths of one member's certificate and key.
pub struct Files {
    pub cert: PathBuf,
    pub key: PathBuf,
}

impl Pki {
    pub fn new(dir: &Path) -> Self {
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

    pub fn ca(&self) -> PathBuf {
        self.dir.join("ca.pem")
    }

    /// A certificate for the DNS names `dns` (and 127.0.0.1) that names `role`, good for
    /// serving and for calling. A daemon's names exactly one node, which the frontend reads.
    pub fn cert(&self, name: &str, role: &str, dns: &[&str]) -> Files {
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
pub struct Front {
    pub worker: std::net::SocketAddr,
    pub _stop: oneshot::Sender<()>,
}

pub fn front(pki: &Pki, dir: &Path) -> Front {
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
/// its storage half, serving in the background.
pub async fn node(pki: &Pki, front: &Front, name: &str, dir: &Path) -> NodeUnderTest {
    let own = pki.cert(name, "storage", &["localhost"]);
    let argv = [
        "storage".to_owned(),
        "--storage-listen=127.0.0.1:0".to_owned(),
        format!("--cold-store=file://{}", dir.join("cold").display()),
        format!("--meta-dir={}", dir.join("meta").display()),
        format!("--frontend=https://localhost:{}", front.worker.port()),
        format!("--storage-tls-cert={}", own.cert.display()),
        format!("--storage-tls-key={}", own.key.display()),
        format!("--storage-client-ca={}", pki.ca().display()),
    ];
    let flags = storage_node::parse(
        &storage_node::command()
            .try_get_matches_from(argv)
            .expect("the node's flags"),
    )
    .expect("parses");
    let half = storage_half::start(&flags, || Ok(pending::<()>()))
        .await
        .expect("the node starts");
    let (addr, storage, maps, in_process) =
        (half.addr, half.storage(), half.maps(), half.in_process());
    tokio::spawn(StorageHalf::serve(half));
    NodeUnderTest {
        addr,
        storage,
        in_process,
        maps,
        me: None,
        chain: None,
    }
}

/// What a chain node learns of the map: the holder the frontend's maps arrive in, and the
/// member id the frontend gave it (set by the test once the map names it).
struct HeldMap {
    maps: Arc<MapHolder>,
    me: AtomicU64,
}

impl MapSource for HeldMap {
    fn current(&self) -> Option<Arc<Map>> {
        self.maps.current()
    }

    fn me(&self) -> Option<MemberId> {
        match self.me.load(Ordering::SeqCst) {
            0 => None,
            id => Some(MemberId(id)),
        }
    }
}

/// A storage node that is a node of a chain (R = 2): its service judges every call by the
/// map the frontend sends it and, as the primary of a blob, copies it to the replica
/// before it answers. It registers with `front` as `kbf storage-and-daemon`'s half does.
/// The store is a [`Spy`] that takes a blob as a stream, which the real cache does not
/// yet and a chain's primary needs; the server does not wire a chain yet either, so the
/// service is built here.
pub async fn chain_node(pki: &Pki, front: &Front, name: &str, dir: &Path) -> NodeUnderTest {
    let own = pki.cert(name, "storage", &["localhost"]);
    let argv = [
        "storage".to_owned(),
        "--storage-listen=127.0.0.1:0".to_owned(),
        format!("--cold-store=file://{}", dir.join("cold").display()),
        format!("--meta-dir={}", dir.join("meta").display()),
        format!("--frontend=https://localhost:{}", front.worker.port()),
        format!("--storage-tls-cert={}", own.cert.display()),
        format!("--storage-tls-key={}", own.key.display()),
        format!("--storage-client-ca={}", pki.ca().display()),
    ];
    let flags = storage_node::parse(
        &storage_node::command()
            .try_get_matches_from(argv)
            .expect("the node's flags"),
    )
    .expect("parses");
    let read = |path: &Path| std::fs::read(path).expect("read a TLS file");
    let ca = Certificate::from_pem(read(&pki.ca()));
    let identity = Identity::from_pem(read(&own.cert), read(&own.key));
    let maps = Arc::new(MapHolder::new());
    let held = Arc::new(HeldMap {
        maps: Arc::clone(&maps),
        me: AtomicU64::new(0),
    });
    std::fs::create_dir_all(dir.join("repairs")).expect("repairs dir");
    let repairs = Arc::new(
        Repairs::open(
            StdFs::new(dir.join("repairs")).expect("repairs fs"),
            Options::default(),
            Arc::new(NoLedger),
        )
        .expect("a journal"),
    );
    let calls_as = ClientTlsConfig::new()
        .ca_certificate(ca.clone())
        .identity(identity.clone());
    let chain = Arc::new(Chain::new(
        Arc::clone(&held) as Arc<dyn MapSource>,
        Peers::tls(calls_as),
        Arc::clone(&repairs),
        ChainConfig::default(),
    ));
    let store = Spy::streaming();
    let service = Arc::new(
        StorageService::new(
            Arc::clone(&store) as Arc<dyn Storage>,
            Admission::Certified,
            Config {
                write_bytes_in_flight: 64 << 20,
            },
        )
        .with_chain(chain)
        .expect("a certified service"),
    );
    let incoming = TcpIncoming::bind(SocketAddr::from(([127, 0, 0, 1], 0))).expect("bind");
    let addr = incoming.local_addr().expect("address");
    let tls = ServerTlsConfig::new().identity(identity).client_ca_root(ca);
    let server = tokio::spawn(
        Server::builder()
            .tls_config(tls)
            .expect("TLS config")
            .add_routes(service.shared_routes())
            .serve_with_incoming(incoming),
    );
    let joiner = flags
        .joiner(addr, Arc::clone(&maps))
        .expect("the joiner")
        .expect("a frontend");
    tokio::spawn(joiner.run());
    NodeUnderTest {
        addr,
        storage: store as Arc<dyn Storage>,
        in_process: service.in_process(),
        maps,
        me: Some(held),
        chain: Some(ChainParts {
            repairs,
            server: server.abort_handle(),
        }),
    }
}

pub struct NodeUnderTest {
    pub addr: std::net::SocketAddr,
    /// The node's store itself.
    pub storage: Arc<dyn Storage>,
    /// The node's service, called in-process: what a daemon half reaches its own node by.
    pub in_process: InProcess,
    pub maps: Arc<MapHolder>,
    /// A chain node's map source, whose member id the test sets.
    me: Option<Arc<HeldMap>>,
    /// A chain node's repair journal and the task that serves its listener.
    chain: Option<ChainParts>,
}

struct ChainParts {
    repairs: Arc<Repairs>,
    server: tokio::task::AbortHandle,
}

impl NodeUnderTest {
    /// Tells a chain node which member of the map it is (the frontend assigned the id at
    /// registration; the map names it by the address the node registered).
    pub fn is_member(&self, id: MemberId) {
        let held = self.me.as_ref().expect("a chain node");
        held.me.store(id.0, Ordering::SeqCst);
    }

    /// Stops a chain node's listener: its port refuses connections from here on (a leg
    /// to it fails). No call had been made to it before, so no connection outlives this.
    pub fn stop_server(&self) {
        self.chain.as_ref().expect("a chain node").server.abort();
    }

    /// The copies a chain node owes, from its repair journal: (digest, replica).
    pub fn owed(&self) -> Vec<(Digest, MemberId)> {
        let chain = self.chain.as_ref().expect("a chain node");
        chain
            .repairs
            .open_records()
            .into_iter()
            .map(|r| (r.digest, r.replica))
            .collect()
    }

    pub async fn holds(&self, digest: &Digest) -> bool {
        self.storage
            .find_missing(std::slice::from_ref(digest))
            .await
            .expect("find missing")
            .is_empty()
    }
}

/// The map in `holder` once it has `members` members.
pub async fn map_of(who: &str, holder: &MapHolder, members: usize) -> Arc<Map> {
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
pub fn digest(bytes: &[u8]) -> Digest {
    kbf_segments::sha256(bytes)
}

/// A blob whose primary owner in `map` is `member`.
pub fn blob_owned_by(map: &Map, member: MemberId, tag: &str) -> Vec<u8> {
    (0_u64..100_000)
        .map(|i| format!("{tag} blob number {i}").into_bytes())
        .find(|bytes| map.write_owners(&digest(bytes)).first() == Some(&member))
        .expect("a blob this member owns")
}
