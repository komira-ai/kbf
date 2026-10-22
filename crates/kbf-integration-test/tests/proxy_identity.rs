//! `x-kbf-*` is the child's contract: the real `kbf-proxy` in front of the real `kbf-server`
//! gate (`ProxyAuthLayer`, U4a), over real sockets and real TLS.
//!
//! The child here is a gRPC service that answers with what the gate decided about the call
//! (the `Trust` the layer put in the request's extensions) and how many `x-kbf-*` headers
//! were left on the request for a handler to read. The gate is the production one: it loads
//! the secret with `ProxySecret::from_file` (exactly 32 bytes, this user's, mode 0600 or
//! 0400) and compares `x-kbf-proxy-auth` in constant time. So the test fails if the proxy
//! writes a secret file the child refuses (a trailing newline, mode 0644), spells a header
//! another way, or sets the identity of anyone but the verified certificate.

use std::net::{Ipv4Addr, SocketAddr};
use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use kbf_proto::kbf::map::v1 as pb;
use kbf_proto::kbf::map::v1::membership_client::MembershipClient;
use kbf_proxy::config::ProxyConfig;
use kbf_proxy::control::NoUpdates;
use kbf_proxy::slot::Slot;
use kbf_proxy::{Proxy, ProxyHandle};
use kbf_server::members::{Members, MembershipConfig, PROTOCOL};
use kbf_server::members_rpc::MembershipService;
use kbf_server::proxy_auth::{ProxyAuthLayer, ProxySecret, Trust};
use kbf_state::ManualClock;
use kbf_state::Store;
use kbf_state::fake::{Fake, Flaw};
use kbf_storage::service::Admission;
use kbf_testkit::dir::TestDir;
use rcgen::{
    BasicConstraints, CertificateParams, CertifiedIssuer, DnType, ExtendedKeyUsagePurpose, IsCa,
    Issuer, KeyPair, KeyUsagePurpose, SanType, SigningKey,
};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};
use tokio::net::TcpStream;
use tonic::codegen::{Service, http};
use tonic::server::NamedService;
use tonic::transport::server::TcpIncoming;
use tonic::transport::{Certificate, Channel, ClientTlsConfig, Endpoint, Identity, Server};

/// The child: answers every call of the worker service with the gate's decision.
#[derive(Clone)]
struct Seen;

impl NamedService for Seen {
    const NAME: &'static str = "kbf.worker.v1.Worker";
}

impl Service<http::Request<tonic::body::Body>> for Seen {
    type Response = http::Response<tonic::body::Body>;
    type Error = std::convert::Infallible;
    type Future = std::future::Ready<Result<Self::Response, Self::Error>>;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, request: http::Request<tonic::body::Body>) -> Self::Future {
        let trust = request
            .extensions()
            .get::<Trust>()
            .map_or_else(|| "no gate".to_owned(), |trust| format!("{trust:?}"));
        let left = request
            .headers()
            .keys()
            .filter(|name| name.as_str().starts_with("x-kbf-"))
            .count();
        let answer = http::Response::builder()
            .header("content-type", "application/grpc")
            .header("grpc-status", "0")
            .header("x-trust", trust)
            .header("x-left", left.to_string())
            .body(tonic::body::Body::empty())
            .expect("a response");
        std::future::ready(Ok(answer))
    }
}

struct Pki {
    issuer: CertifiedIssuer<'static, KeyPair>,
    ca_pem: String,
}

struct Leaf {
    cert: String,
    key: String,
}

impl Pki {
    fn new() -> Self {
        let mut params = CertificateParams::new(Vec::<String>::new()).expect("CA params");
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        params
            .distinguished_name
            .push(DnType::CommonName, "proxy identity test CA");
        params.key_usages = vec![
            KeyUsagePurpose::KeyCertSign,
            KeyUsagePurpose::DigitalSignature,
        ];
        let issuer = CertifiedIssuer::self_signed(params, KeyPair::generate().expect("key"))
            .expect("CA certificate");
        let ca_pem = issuer.pem();
        Self { issuer, ca_pem }
    }

    fn leaf(&self, dns: &str, role: Option<&str>, usage: ExtendedKeyUsagePurpose) -> Leaf {
        leaf_under(&self.issuer, dns, role, usage, &[])
    }

    /// A client leaf that names `ips` besides its DNS name and role.
    fn leaf_with_ips(&self, dns: &str, role: &str, ips: &[&str]) -> Leaf {
        leaf_under(
            &self.issuer,
            dns,
            Some(role),
            ExtendedKeyUsagePurpose::ClientAuth,
            ips,
        )
    }

    /// A client leaf signed by an intermediate CA that itself names `intermediate_ip`; the
    /// chain the client presents is the leaf and then the intermediate.
    fn leaf_via_intermediate(
        &self,
        dns: &str,
        role: &str,
        ips: &[&str],
        intermediate_ip: &str,
    ) -> Leaf {
        let mut params = CertificateParams::new(Vec::<String>::new()).expect("CA params");
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        params
            .distinguished_name
            .push(DnType::CommonName, "proxy identity test intermediate");
        params.key_usages = vec![
            KeyUsagePurpose::KeyCertSign,
            KeyUsagePurpose::DigitalSignature,
        ];
        params.subject_alt_names = vec![SanType::IpAddress(intermediate_ip.parse().expect("ip"))];
        let intermediate =
            CertifiedIssuer::signed_by(params, KeyPair::generate().expect("key"), &self.issuer)
                .expect("intermediate certificate");
        let mut leaf = leaf_under(
            &intermediate,
            dns,
            Some(role),
            ExtendedKeyUsagePurpose::ClientAuth,
            ips,
        );
        leaf.cert.push_str(&intermediate.pem());
        leaf
    }
}

fn leaf_under(
    issuer: &Issuer<'_, impl SigningKey>,
    dns: &str,
    role: Option<&str>,
    usage: ExtendedKeyUsagePurpose,
    ips: &[&str],
) -> Leaf {
    let mut params = CertificateParams::new(Vec::<String>::new()).expect("params");
    params.subject_alt_names = vec![SanType::DnsName(dns.try_into().expect("ia5"))];
    params.subject_alt_names.extend(
        ips.iter()
            .map(|ip| SanType::IpAddress(ip.parse().expect("an IP address"))),
    );
    if let Some(role) = role {
        let uri = format!("urn:kbf:role:{role}");
        params
            .subject_alt_names
            .push(SanType::URI(uri.try_into().expect("ia5")));
    }
    params.extended_key_usages = vec![usage];
    let key = KeyPair::generate().expect("key");
    let cert = params.signed_by(&key, issuer).expect("sign");
    Leaf {
        cert: cert.pem(),
        key: key.serialize_pem(),
    }
}

fn dir(name: &str) -> TestDir {
    TestDir::under(
        env!("CARGO_TARGET_TMPDIR"),
        &format!("proxy-identity-{name}"),
    )
}

fn private(path: &Path, content: &[u8]) {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::write(path, content).expect("write");
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).expect("chmod");
}

/// A proxy over `base`'s ports, with slot A active; `leaf` is its server certificate.
fn start_proxy(dir: &Path, pki: &Pki, base: u16) -> (ProxyHandle, SocketAddr, PathBuf) {
    let server = pki.leaf("localhost", None, ExtendedKeyUsagePurpose::ServerAuth);
    private(&dir.join("server.pem"), server.cert.as_bytes());
    private(&dir.join("server.key"), server.key.as_bytes());
    private(&dir.join("ca.pem"), pki.ca_pem.as_bytes());
    private(
        &dir.join("operator.token"),
        b"operator-token-0123456789-abcdefghijkl",
    );
    let secret = dir.join("proxy-auth");
    let socket = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind");
    let public = socket.local_addr().expect("address");
    let config = ProxyConfig::from_toml_str(&format!(
        "operator_token_file = {:?}\nserver_cert_file = {:?}\nserver_key_file = {:?}\n\
         client_ca_file = {:?}\nproxy_auth_file = {secret:?}\n[upstream]\nbase_port = {base}\n",
        dir.join("operator.token"),
        dir.join("server.pem"),
        dir.join("server.key"),
        dir.join("ca.pem"),
    ))
    .expect("a valid table");
    let (handle, runner) = Proxy::bind(config, OwnedFd::from(socket)).expect("bind the proxy");
    std::thread::spawn(move || runner.run(NoUpdates));
    handle.slots().adopt(Slot::A, None);
    handle.slots().release_into(Slot::A).expect("A is open");
    (handle, public, secret)
}

/// One gRPC-shaped call over `io`; the response headers.
async fn call(
    io: impl tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
    extra: &[(&str, &str)],
) -> http::HeaderMap {
    let (send, connection) = h2::client::handshake(io).await.expect("h2");
    tokio::spawn(connection);
    let mut request = http::Request::builder()
        .method("POST")
        .uri("http://frontend.test/kbf.worker.v1.Worker/Session")
        .header("content-type", "application/grpc");
    for (name, value) in extra {
        request = request.header(*name, *value);
    }
    let (response, _) = send
        .ready()
        .await
        .expect("ready")
        .send_request(request.body(()).expect("request"), true)
        .expect("send");
    response.await.expect("a response").headers().clone()
}

async fn tls(
    proxy: SocketAddr,
    pki: &Pki,
    identity: &Leaf,
) -> tokio_rustls::client::TlsStream<TcpStream> {
    let mut roots = rustls::RootCertStore::empty();
    for der in CertificateDer::pem_slice_iter(pki.ca_pem.as_bytes()) {
        roots.add(der.expect("CA PEM")).expect("root");
    }
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .expect("versions")
        .with_root_certificates(roots)
        .with_client_auth_cert(
            CertificateDer::pem_slice_iter(identity.cert.as_bytes())
                .collect::<Result<_, _>>()
                .expect("chain"),
            PrivateKeyDer::from_pem_slice(identity.key.as_bytes()).expect("key"),
        )
        .expect("identity");
    config.alpn_protocols = vec![b"h2".to_vec()];
    let tcp = TcpStream::connect(proxy).await.expect("connect");
    tokio_rustls::TlsConnector::from(Arc::new(config))
        .connect(ServerName::try_from("localhost").expect("name"), tcp)
        .await
        .expect("handshake")
}

fn header(headers: &http::HeaderMap, name: &str) -> String {
    headers
        .get(name)
        .map_or_else(String::new, |v| v.to_str().expect("text").to_owned())
}

/// A proxy in front of a bound child port: everything a test needs but the child itself.
struct Rig {
    pki: Arc<Pki>,
    handle: ProxyHandle,
    proxy: SocketAddr,
    child: SocketAddr,
    incoming: TcpIncoming,
    secret: ProxySecret,
    /// Held so the directory outlives the proxy.
    _dir: TestDir,
}

async fn rig(name: &str) -> Rig {
    let dir = dir(name);
    let started = dir.path().to_path_buf();
    let pki = Arc::new(Pki::new());
    let for_proxy = Arc::clone(&pki);
    // The child's port is the worker listener of slot A: base + 1.
    let incoming = TcpIncoming::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0))).expect("bind");
    let child = incoming.local_addr().expect("address");
    let base = child.port() - 1;
    // The proxy builds its own runtime, which must not be built or dropped inside this one.
    let (handle, proxy, secret_file) =
        tokio::task::spawn_blocking(move || start_proxy(&started, &for_proxy, base))
            .await
            .expect("the proxy starts");
    let secret = ProxySecret::from_file(&secret_file)
        .expect("the child accepts the file the proxy wrote: 32 bytes, this user's, mode 0600");
    Rig {
        pki,
        handle,
        proxy,
        child,
        incoming,
        secret,
        _dir: dir,
    }
}

/// The proxy's secret file is one the child's gate accepts, and a call through the proxy is
/// `Proxied` with the verified certificate's node and role, whatever the client forged; the
/// same headers sent straight to the child are not believed.
///
/// Mutants seen red: a header spelled differently; the secret written with a trailing
/// newline or mode 0644; identity taken from a client header.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn child_accepts_proxy_headers() {
    let Rig {
        pki,
        handle,
        proxy,
        child,
        incoming,
        secret,
        _dir,
    } = rig("accepts").await;
    let layer = ProxyAuthLayer::new(Some(Arc::new(secret)));
    tokio::spawn(
        Server::builder()
            .layer(layer)
            .add_service(Seen)
            .serve_with_incoming(incoming),
    );

    let daemon = pki.leaf(
        "node-5.farm",
        Some("daemon"),
        ExtendedKeyUsagePurpose::ClientAuth,
    );
    let forged = [
        ("x-kbf-node-id", "forged-node"),
        ("x-kbf-role", "storage"),
        ("x-kbf-proxy-auth", "guess"),
        ("x-kbf-extra", "1"),
        ("x-kbf-node-ips", "198.51.100.9"),
    ];
    let answer = call(tls(proxy, &pki, &daemon).await, &forged).await;
    assert_eq!(
        header(&answer, "x-trust"),
        r#"Proxied(Identity { node_id: Some("node-5.farm"), role: Some(Daemon), ips: [] })"#,
        "a forged x-kbf-node-ips does not survive a certificate with no IP name"
    );
    assert_eq!(
        header(&answer, "x-left"),
        "0",
        "the gate leaves no x-kbf-* for a handler"
    );

    // The same claims straight to the child, without the secret, are not believed.
    let direct = call(TcpStream::connect(child).await.expect("connect"), &forged).await;
    assert_eq!(header(&direct, "x-trust"), "Unverified");

    // An admin call carries the secret and no identity.
    let admin: Vec<(String, String)> = handle
        .admin_headers()
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_str().expect("text").to_owned()))
        .collect();
    let admin: Vec<(&str, &str)> = admin
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    let answer = call(TcpStream::connect(child).await.expect("connect"), &admin).await;
    assert_eq!(
        header(&answer, "x-trust"),
        "Proxied(Identity { node_id: None, role: None, ips: [] })"
    );
    handle.shutdown();
}

/// The IP names of the verified leaf reach the child as `x-kbf-node-ips`: all of them,
/// sorted and compressed, whatever order the certificate lists them in and whatever the
/// client forged, and only the leaf's: an intermediate CA's own IP name is not the caller's.
///
/// Mutants seen red: the proxy omits the header; the proxy does not strip an inbound copy
/// (the DNS-only certificate in `child_accepts_proxy_headers`, and here a forged value
/// beside a leaf's); the addresses taken from the intermediate (the chain's last
/// certificate) instead of the leaf.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_proxy_forwards_the_leafs_ips_and_no_others() {
    let Rig {
        pki,
        handle,
        proxy,
        incoming,
        secret,
        _dir,
        ..
    } = rig("ips").await;
    tokio::spawn(
        Server::builder()
            .layer(ProxyAuthLayer::new(Some(Arc::new(secret))))
            .add_service(Seen)
            .serve_with_incoming(incoming),
    );
    let forged = [("x-kbf-node-ips", "198.51.100.9")];

    let storage = pki.leaf_with_ips(
        "spoolr-node-01",
        "storage",
        &["192.0.2.10", "fd00:0:0:0:0:0:0:1", "192.0.2.1", "192.0.2.1"],
    );
    let answer = call(tls(proxy, &pki, &storage).await, &forged).await;
    assert_eq!(
        header(&answer, "x-trust"),
        r#"Proxied(Identity { node_id: Some("spoolr-node-01"), role: Some(Storage), ips: [192.0.2.1, 192.0.2.10, fd00::1] })"#
    );
    assert_eq!(header(&answer, "x-left"), "0");

    let via =
        pki.leaf_via_intermediate("spoolr-node-02", "storage", &["192.0.2.2"], "198.51.100.7");
    let answer = call(tls(proxy, &pki, &via).await, &[]).await;
    assert_eq!(
        header(&answer, "x-trust"),
        r#"Proxied(Identity { node_id: Some("spoolr-node-02"), role: Some(Storage), ips: [192.0.2.2] })"#,
        "the intermediate's own address is not the caller's"
    );

    let none = pki.leaf_via_intermediate("spoolr-node-03", "storage", &[], "198.51.100.7");
    let answer = call(tls(proxy, &pki, &none).await, &[]).await;
    assert_eq!(
        header(&answer, "x-trust"),
        r#"Proxied(Identity { node_id: Some("spoolr-node-03"), role: Some(Storage), ips: [] })"#,
        "a leaf with no IP name has none, whatever its issuer names"
    );
    handle.shutdown();
}

/// The farm's own case, end to end: a storage node whose certificate names its DNS name and
/// the IP it advertises registers as `<ip>:8983` through the real proxy and the real
/// Membership service, and heartbeats; the node next door, whose certificate names another
/// address, can neither register as the first node's address nor beat for its member.
///
/// Mutants seen red: the proxy omits the header; the child ignores it or drops it when it
/// builds the proxied identity; the heartbeat check ignores the addresses.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_storage_node_registers_by_its_ip_through_the_proxy() {
    let Rig {
        pki,
        handle,
        proxy,
        incoming,
        secret,
        _dir,
        ..
    } = rig("register").await;
    let clock = Arc::new(ManualClock::default());
    let members = Arc::new(Members::new(
        Arc::new(Fake::with_flaw(clock, Flaw::None)) as Arc<dyn Store>,
        MembershipConfig {
            ttl: Duration::from_secs(60),
            dwell: Duration::from_secs(1),
            replication: 2,
        },
    ));
    tokio::spawn(
        Server::builder()
            .layer(ProxyAuthLayer::new(Some(Arc::new(secret))))
            .add_service(MembershipService::new(members, Admission::Certified).server())
            .serve_with_incoming(incoming),
    );

    let one = pki.leaf_with_ips("spoolr-node-01", "storage", &["192.0.2.1"]);
    let two = pki.leaf_with_ips("spoolr-node-02", "storage", &["192.0.2.2"]);
    let mut first = MembershipClient::new(channel(proxy, &pki, &one).await);
    let mut second = MembershipClient::new(channel(proxy, &pki, &two).await);
    let by_ip = |at: &str| pb::RegisterRequest {
        role: pb::Role::Storage.into(),
        instance_id: at.to_owned(),
        addr: at.to_owned(),
        weight: 1,
        zone: "a".to_owned(),
        protocol: PROTOCOL,
        catching_up: false,
        last_member_id: 0,
        last_boot: 0,
        ..pb::RegisterRequest::default()
    };
    let member = first
        .register(by_ip("192.0.2.1:8983"))
        .await
        .expect("the node registers by the IP its certificate names")
        .into_inner();
    let beat = |member: &pb::RegisterResponse| pb::HeartbeatRequest {
        member_id: member.member_id,
        boot: member.boot,
        ..pb::HeartbeatRequest::default()
    };
    first
        .heartbeat(beat(&member))
        .await
        .expect("and beats for its member");

    let taken = second
        .register(by_ip("192.0.2.1:8983"))
        .await
        .expect_err("another node's address");
    assert_eq!(taken.code(), tonic::Code::PermissionDenied, "{taken}");
    let stolen = second
        .heartbeat(beat(&member))
        .await
        .expect_err("another node's member");
    assert_eq!(stolen.code(), tonic::Code::PermissionDenied, "{stolen}");
    second
        .register(by_ip("192.0.2.2:8983"))
        .await
        .expect("each registers its own");
    handle.shutdown();
}

/// A gRPC channel to the proxy that presents `identity` as its client certificate.
async fn channel(proxy: SocketAddr, pki: &Pki, identity: &Leaf) -> Channel {
    let tls = ClientTlsConfig::new()
        .domain_name("localhost")
        .ca_certificate(Certificate::from_pem(&pki.ca_pem))
        .identity(Identity::from_pem(&identity.cert, &identity.key));
    Endpoint::from_shared(format!("https://localhost:{}", proxy.port()))
        .expect("an endpoint")
        .tls_config(tls)
        .expect("tls")
        .connect()
        .await
        .expect("connect through the proxy")
}
