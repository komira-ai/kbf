//! Mutual TLS to real storage nodes: each is verified against its own name.

use std::net::SocketAddr;
use std::sync::Arc;

use kbf_map::{Member, MemberId, MemberState};
use kbf_route::{Router, TlsNodes};
use kbf_storage::service::{Admission, Config, StorageService};
use kbf_storage::{Storage, StorageError};
use rcgen::{
    BasicConstraints, CertificateParams, CertifiedIssuer, DnType, ExtendedKeyUsagePurpose, IsCa,
    KeyPair, KeyUsagePurpose, SanType,
};
use tonic::transport::server::TcpIncoming;
use tonic::transport::{Certificate, Identity, Server, ServerTlsConfig};

use super::support::{Cell, Node, blob_where, collect, map, view};

/// The farm's one CA.
struct Pki {
    ca: CertifiedIssuer<'static, KeyPair>,
}

impl Pki {
    fn new() -> Self {
        let mut params = CertificateParams::new(Vec::<String>::new()).expect("CA params");
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        params
            .distinguished_name
            .push(DnType::CommonName, "kbf test CA");
        params.key_usages = vec![
            KeyUsagePurpose::KeyCertSign,
            KeyUsagePurpose::DigitalSignature,
        ];
        let ca =
            CertifiedIssuer::self_signed(params, KeyPair::generate().expect("key")).expect("CA");
        Self { ca }
    }

    /// A certificate for the name `san`, signed by the CA, with this EKU.
    fn leaf(&self, san: SanType, eku: ExtendedKeyUsagePurpose) -> Identity {
        let mut params = CertificateParams::new(Vec::<String>::new()).expect("params");
        params.subject_alt_names = vec![san];
        params.extended_key_usages = vec![eku];
        let key = KeyPair::generate().expect("key");
        let cert = params.signed_by(&key, &self.ca).expect("sign");
        Identity::from_pem(cert.pem(), key.serialize_pem())
    }

    fn server(&self, san: SanType) -> Identity {
        self.leaf(san, ExtendedKeyUsagePurpose::ServerAuth)
    }

    fn client(&self) -> Identity {
        let name = SanType::DnsName("a-daemon".try_into().expect("an IA5 name"));
        self.leaf(name, ExtendedKeyUsagePurpose::ClientAuth)
    }

    fn ca(&self) -> Certificate {
        Certificate::from_pem(self.ca.pem())
    }

    /// Nodes that trust the CA and present a client certificate.
    fn nodes(&self) -> Arc<TlsNodes> {
        Arc::new(TlsNodes::new(self.ca(), self.client()))
    }

    /// `node` served over mutual TLS with `identity` as its certificate.
    async fn serve(&self, node: &Arc<Node>, identity: Identity) -> u16 {
        let storage: Arc<dyn Storage> = node.clone();
        let service = StorageService::new(
            storage,
            Admission::Open,
            Config {
                write_bytes_in_flight: 1 << 20,
            },
        );
        let tls = ServerTlsConfig::new()
            .identity(identity)
            .client_ca_root(self.ca());
        let incoming = TcpIncoming::bind(SocketAddr::from(([127, 0, 0, 1], 0))).expect("bind");
        let port = incoming.local_addr().expect("address").port();
        let mut server = Server::builder().tls_config(tls).expect("TLS config");
        tokio::spawn(
            server
                .add_routes(service.routes())
                .serve_with_incoming(incoming),
        );
        port
    }
}

fn dns(name: &str) -> SanType {
    SanType::DnsName(name.try_into().expect("an IA5 name"))
}

fn ip() -> SanType {
    SanType::IpAddress([127, 0, 0, 1].into())
}

fn at(id: u64, addr: String) -> Member {
    Member {
        id: MemberId(id),
        addr,
        zone: String::new(),
        weight: 1,
        state: MemberState::Active,
    }
}

/// A blob that the map of replication 1 over members 1 and 2 reads from `owner`.
fn blob_of(map: &kbf_map::Map, owner: u64) -> (Vec<u8>, kbf_types::Digest) {
    blob_where(|d| map.read_order(d) == [MemberId(owner)])
}

/// Catches a router that cannot reach nodes over mutual TLS, and the control for the
/// refusals below: node 1 is dialled as `localhost` and presents a certificate for it, node
/// 2 is dialled as `127.0.0.1` and presents a certificate for that, and a blob of each is
/// read.
#[tokio::test]
async fn each_node_is_reached_by_its_own_name() {
    let pki = Pki::new();
    let (one, two) = (Node::new(), Node::new());
    let port_1 = pki.serve(&one, pki.server(dns("localhost"))).await;
    let port_2 = pki.serve(&two, pki.server(ip())).await;
    let members = vec![
        at(1, format!("localhost:{port_1}")),
        at(2, format!("127.0.0.1:{port_2}")),
    ];
    let map = map(1, 1, members);
    let router = Router::new(Cell::with(view(&map)), pki.nodes());
    let (blob_1, digest_1) = blob_of(&map, 1);
    let (blob_2, digest_2) = blob_of(&map, 2);
    one.hold(&blob_1);
    two.hold(&blob_2);

    for (blob, digest) in [(blob_1, digest_1), (blob_2, digest_2)] {
        let (bytes, _) = collect(router.read_range(&digest, 0, 0).await.expect("a read"))
            .await
            .expect("bytes");
        assert_eq!(bytes, blob);
    }
}

/// Catches one name checked for every node (a `--tls-server-name` for the farm): node-03,
/// dialled as `127.0.0.1`, presents the certificate that node-02, `localhost`, holds. The
/// farm's CA signed it, but it is not node-03's: the call is refused, and the node never
/// saw a request. Node 02 is still reached by its own name.
#[tokio::test]
async fn one_nodes_certificate_is_refused_when_dialling_another() {
    let pki = Pki::new();
    let (node_02, node_03) = (Node::new(), Node::new());
    let port_02 = pki.serve(&node_02, pki.server(dns("localhost"))).await;
    // Node 03 answers with node 02's name.
    let port_03 = pki.serve(&node_03, pki.server(dns("localhost"))).await;
    let members = vec![
        at(1, format!("localhost:{port_02}")),
        at(2, format!("127.0.0.1:{port_03}")),
    ];
    let map = map(1, 1, members);
    let router = Router::new(Cell::with(view(&map)), pki.nodes());
    let (blob_02, digest_02) = blob_of(&map, 1);
    let (blob_03, digest_03) = blob_of(&map, 2);
    node_02.hold(&blob_02);
    node_03.hold(&blob_03);

    let error = router
        .read_range(&digest_03, 0, 0)
        .await
        .err()
        .expect("refused");
    assert!(matches!(error, StorageError::Unavailable(_)), "{error:?}");
    assert_eq!(
        node_03.read_count(),
        0,
        "the handshake failed before any call"
    );

    let (bytes, _) = collect(router.read_range(&digest_02, 0, 0).await.expect("a read"))
        .await
        .expect("bytes");
    assert_eq!(bytes, blob_02);
}

/// Catches a connection kept for a member that moved: member 1 is at node A, then at node
/// B under the same id, and a blob only B holds is read.
#[tokio::test]
async fn a_member_that_moves_is_dialled_at_its_new_address() {
    let pki = Pki::new();
    let (a, b) = (Node::new(), Node::new());
    let port_a = pki.serve(&a, pki.server(dns("localhost"))).await;
    let port_b = pki.serve(&b, pki.server(ip())).await;
    let first = map(1, 1, vec![at(1, format!("localhost:{port_a}"))]);
    let moved = map(2, 1, vec![at(1, format!("127.0.0.1:{port_b}"))]);
    let cell = Cell::with(view(&first));
    let router = Router::new(cell.clone(), pki.nodes());
    let digest = b.hold(b"only the new address has this");
    a.hold(b"only the old address has that");

    let error = router
        .read_range(&digest, 0, 0)
        .await
        .err()
        .expect("not at A");
    assert!(matches!(error, StorageError::NotFound(_)), "{error:?}");
    cell.set(view(&moved));
    let (bytes, _) = collect(router.read_range(&digest, 0, 0).await.expect("a read"))
        .await
        .expect("bytes");
    assert_eq!(bytes, b"only the new address has this");
}

/// Catches an address that is not one dialled anyway, and a TLS configuration that is not
/// usable reported as an outage of the node: both are internal errors naming the member.
#[tokio::test]
async fn a_member_that_cannot_be_dialled_is_an_internal_error() {
    let pki = Pki::new();
    let bad = at(7, "not a host:1".to_owned());
    let error = pki.nodes().channel(&bad).expect_err("not an address");
    assert!(matches!(error, StorageError::Internal(_)), "{error:?}");
    assert!(error.to_string().contains("m7"), "{error}");

    let junk = TlsNodes::new(
        Certificate::from_pem("not a certificate"),
        Identity::from_pem("not a certificate", "not a key"),
    );
    let error = junk
        .channel(&at(8, "localhost:1".to_owned()))
        .expect_err("not usable");
    assert!(matches!(error, StorageError::Internal(_)), "{error:?}");
    assert!(error.to_string().contains("TLS"), "{error}");
}
