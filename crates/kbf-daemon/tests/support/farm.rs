//! A real farm in one test process: the CA and certificates that name roles, a frontend
//! over SQLite and storage nodes that register with it (what `kbf storage-and-daemon`
//! starts as its storage half). Shared by tests/map_wiring.rs and tests/member_wiring.rs.

use std::future::pending;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use kbf_map::{Map, MemberId};
use kbf_mapsync::MapHolder;
use kbf_server::frontend::{self, FrontendHandle, start_frontend};
use kbf_server::storage_half::{self, StorageHalf};
use kbf_server::storage_node;
use kbf_storage::Storage;
use kbf_storage::service::ROLE_URI_PREFIX;
use kbf_types::Digest;
use rcgen::{
    BasicConstraints, CertificateParams, CertifiedIssuer, DnType, ExtendedKeyUsagePurpose, IsCa,
    KeyPair, KeyUsagePurpose, SanType,
};
use tokio::sync::oneshot;
use tokio::time::timeout;

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
    let (addr, storage, maps) = (half.addr, half.storage(), half.maps());
    tokio::spawn(StorageHalf::serve(half));
    NodeUnderTest {
        addr,
        storage,
        maps,
    }
}

pub struct NodeUnderTest {
    pub addr: std::net::SocketAddr,
    pub storage: Arc<dyn Storage>,
    pub maps: Arc<MapHolder>,
}

impl NodeUnderTest {
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
