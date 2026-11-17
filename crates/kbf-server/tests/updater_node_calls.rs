//! A host's updater against the frontend's real worker listener, over mutual TLS and with no
//! token: the one call about its own node (`kbf_server::node_self`), and everything else
//! refused. The updater side is the real code of `kbf-updater` (its client and its storage
//! health check); the frontend is the one `bind_server_with_state` starts, with storage members
//! and the enrollment registry in the state store.
//!
//! The refusals are tried twice: with the updater's own client over HTTP/1.1 (what the agent
//! speaks), and with raw HTTP/2 requests under a certificate of each other role.

use std::net::SocketAddr;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use http_body_util::Full;
use kbf_enroll_registry::{EnrollRequest, IssuedRole, TokenSpec, create_token, enroll_txn};
use kbf_front::Cache;
use kbf_server::members::{
    Fill, Members, MembershipConfig, NodeFacts, PROTOCOL, Progress, Registrant,
};
use kbf_server::storage_health::{Debts, Reported};
use kbf_server::{DenyList, Listeners, StateStore, WorkerTls, bind_server_with_state};
use kbf_state::fake::{Fake, Flaw};
use kbf_state::{ManualClock, Store};
use kbf_testkit::dir::TestDir;
use kbf_tls::{Alpn, TlsSource};
use kbf_types::{NodeId, Role, RoleSet, TokenId, TokenSecret};
use kbf_updater::agent::coordinator::{Endpoint, TlsFiles};
use kbf_updater::identity::identity_of;
use kbf_updater::inventory::{Host, Role as Kind};
use kbf_updater::node_health::NodeHealth;
use kbf_updater::operator::OperatorApi;
use kbf_updater::ports::{Health, HealthCheck};
use kbf_updater::version::Version;
use rcgen::{
    BasicConstraints, CertificateParams, CertifiedIssuer, ExtendedKeyUsagePurpose, IsCa, KeyPair,
    KeyUsagePurpose, SanType,
};
use tonic::codegen::{Service, http};
use tonic::transport::{Certificate, Channel, ClientTlsConfig, Endpoint as GrpcEndpoint, Identity};

const DAY: u64 = 24 * 3600;
const PROMPT: Duration = Duration::from_secs(10);

/// The release the health checks are asked about, and the commit it names.
const TARGET: &str = "0.1.7+g0123456789ab-3";
const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";

fn uri(text: &str) -> SanType {
    SanType::URI(text.try_into().expect("an IA5 name"))
}

fn dns(name: &str) -> SanType {
    SanType::DnsName(name.try_into().expect("an IA5 name"))
}

/// A cell CA and the certificates of every role on demand.
struct Pki {
    ca: CertifiedIssuer<'static, KeyPair>,
}

/// A certificate and its key, as PEM, and the certificate as DER.
struct Leaf {
    cert: String,
    key: String,
    der: Vec<u8>,
}

impl Pki {
    fn new() -> Self {
        let mut ca = CertificateParams::new(Vec::<String>::new()).expect("CA params");
        ca.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        ca.key_usages = vec![
            KeyUsagePurpose::KeyCertSign,
            KeyUsagePurpose::DigitalSignature,
        ];
        Self {
            ca: CertifiedIssuer::self_signed(ca, KeyPair::generate().expect("key")).expect("CA"),
        }
    }

    fn leaf(&self, names: Vec<SanType>, usage: ExtendedKeyUsagePurpose) -> Leaf {
        let mut params = CertificateParams::new(Vec::<String>::new()).expect("params");
        params.subject_alt_names = names;
        params.extended_key_usages = vec![usage];
        let key = KeyPair::generate().expect("key");
        let cert = params.signed_by(&key, &self.ca).expect("sign");
        Leaf {
            cert: cert.pem(),
            key: key.serialize_pem(),
            der: cert.der().to_vec(),
        }
    }

    /// The frontend's worker certificate: `localhost`, and the role `frontend` the updater
    /// checks the server for.
    fn worker_server(&self) -> Leaf {
        let names = vec![dns("localhost"), uri("urn:kbf:role:frontend")];
        self.leaf(names, ExtendedKeyUsagePurpose::ServerAuth)
    }

    /// A client certificate for `node` with the role `role`.
    fn client(&self, role: &str, node: &str) -> Leaf {
        let names = vec![dns(node), uri(&format!("urn:kbf:role:{role}"))];
        self.leaf(names, ExtendedKeyUsagePurpose::ClientAuth)
    }

    /// An updater's certificate for `node`, running `host_role`.
    fn updater(&self, node: &str, host_role: &str) -> Leaf {
        let names = vec![
            dns(node),
            uri("urn:kbf:role:updater"),
            uri(&format!("urn:kbf:host-role:{host_role}")),
        ];
        self.leaf(names, ExtendedKeyUsagePurpose::ClientAuth)
    }

    fn worker_tls(&self, deny_list: Option<DenyList>) -> WorkerTls {
        let leaf = self.worker_server();
        let server = TlsSource::fixed_alpn(
            "worker",
            leaf.cert.as_bytes(),
            leaf.key.as_bytes(),
            Some(self.ca.pem().as_bytes()),
            Alpn::H2AndHttp1,
        )
        .expect("worker TLS");
        WorkerTls { server, deny_list }
    }

    /// A gRPC/HTTP2 connection to the worker listener at `addr` presenting `leaf`.
    async fn channel(&self, addr: SocketAddr, leaf: &Leaf) -> Channel {
        let tls = ClientTlsConfig::new()
            .ca_certificate(Certificate::from_pem(self.ca.pem()))
            .domain_name("localhost")
            .identity(Identity::from_pem(&leaf.cert, &leaf.key));
        GrpcEndpoint::from_shared(format!("https://{addr}"))
            .expect("endpoint")
            .tls_config(tls)
            .expect("TLS")
            .connect()
            .await
            .expect("connect")
    }

    /// The files of an updater client for `leaf`, written in `dir`, and who it is.
    fn files(&self, dir: &Path, leaf: &Leaf) -> (TlsFiles, kbf_updater::identity::Identity) {
        let write = |name: &str, text: &str| -> PathBuf {
            let path = dir.join(name);
            std::fs::write(&path, text).expect("write");
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).expect("chmod");
            path
        };
        let files = TlsFiles {
            cert: write("updater.pem", &leaf.cert),
            key: write("updater.key", &leaf.key),
            ca: write("ca.pem", &self.ca.pem()),
        };
        (
            files,
            identity_of(&leaf.der).expect("an updater's certificate"),
        )
    }
}

fn config() -> MembershipConfig {
    MembershipConfig {
        ttl: Duration::from_secs(30),
        dwell: Duration::from_secs(120),
        replication: 2,
    }
}

fn listeners(worker_tls: WorkerTls) -> Listeners {
    let loopback: SocketAddr = "127.0.0.1:0".parse().expect("addr");
    Listeners {
        reapi: loopback,
        worker: loopback,
        reapi_tls: None,
        worker_tls: Some(worker_tls),
        executor: None,
        finished_retention: kbf_server::executor::FINISHED_RETENTION,
        shutdown_timeout: Duration::from_secs(10),
        store_probe_timeout: kbf_server::health::STORE_PROBE_TIMEOUT,
        invocation_memory: kbf_front::DEFAULT_INVOCATION_MEMORY,
        streams: kbf_server::StreamTiming::default(),
        stream_limits: kbf_server::StreamLimits::DEFAULT,
    }
}

/// [`frontend`]: a frontend with a state store, serving; its worker listener's address.
fn frontend(worker_tls: WorkerTls, fake: &Arc<Fake>) -> SocketAddr {
    let state = StateStore {
        store: Arc::clone(fake) as Arc<dyn Store>,
        members: config(),
        history: None,
    };
    let bound = bind_server_with_state(
        Arc::new(Cache::memory()),
        listeners(worker_tls),
        None,
        kbf_auth::Policy::allow_all(),
        Some(state),
        std::future::pending::<()>(),
    )
    .expect("binds");
    tokio::spawn(bound.serving);
    bound.worker
}

fn store() -> Arc<Fake> {
    let clock = Arc::new(ManualClock::default());
    clock.advance(Duration::from_secs(10 * DAY));
    Arc::new(Fake::with_flaw(clock, Flaw::None))
}

/// A raw HTTP/2 call on `channel`: the status the listener answered with.
async fn status_of(channel: &Channel, method: &str, path: &str, body: &str) -> u16 {
    let mut service = channel.clone();
    std::future::poll_fn(|cx| service.poll_ready(cx))
        .await
        .expect("ready");
    let request = http::Request::builder()
        .method(method)
        .uri(path)
        .header("content-type", "application/json")
        .body(tonic::body::Body::new(Full::new(Bytes::from(
            body.to_owned(),
        ))))
        .expect("a request");
    let answer = tokio::time::timeout(PROMPT, service.call(request))
        .await
        .expect("an answer in time")
        .expect("an answer");
    answer.status().as_u16()
}

fn host(alias: &str, role: Kind, node: &str) -> Host {
    Host {
        alias: alias.to_owned(),
        address: "host.farm.example".to_owned(),
        role,
        host_key: "ssh-ed25519 AAAA".to_owned(),
        node_id: Some(node.to_owned()),
        member: None,
    }
}

fn target() -> Version {
    Version::parse(TARGET).expect("version")
}

fn scratch(name: &str) -> TestDir {
    TestDir::under(env!("CARGO_TARGET_TMPDIR"), &format!("updater-node-{name}"))
}

/// A directory named `name` inside `dir`, made.
fn sub(dir: &TestDir, name: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::create_dir_all(&path).expect("a directory");
    path
}

/// The updater's client for the frontend at `addr`.
fn api(addr: SocketAddr, files: TlsFiles, who: kbf_updater::identity::Identity) -> OperatorApi {
    let endpoint = Endpoint::parse(&format!("localhost:{}", addr.port())).expect("endpoint");
    OperatorApi::as_updater(endpoint, files, who).with_timeout(PROMPT)
}

/// Runs the blocking updater code `f` off the runtime's threads.
async fn blocking<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
    tokio::task::spawn_blocking(f).await.expect("the task")
}

/// What an updater certificate is refused: another node's storage health, every route of the
/// removed node calls (drain, uncordon and status, of its own node as well as another), every
/// operator route and a path that is none. Each is a 403.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_updater_is_refused_another_node_an_operator_route_and_every_other_verb() {
    let dir = scratch("refused");
    let pki = Pki::new();
    let addr = frontend(pki.worker_tls(None), &store());

    let (files, who) = pki.files(&sub(&dir, "a"), &pki.updater("node-a", "daemon"));
    let client = api(addr, files, who);
    let c = client.clone();
    let answers = blocking(move || {
        let mut out = Vec::new();
        for (method, path, body) in [
            ("GET", "/v1/nodes/node-b/storage-health", ""),
            ("POST", "/v1/nodes/node-b:drain", r#"{"deadline_secs": 5}"#),
            ("POST", "/v1/nodes/node-b:uncordon", ""),
            ("GET", "/v1/nodes/node-b", ""),
            ("POST", "/v1/nodes/node-a:drain", r#"{"deadline_secs": 5}"#),
            ("POST", "/v1/nodes/node-a:uncordon", ""),
            ("POST", "/v1/nodes/node-a:cordon", ""),
            ("GET", "/v1/nodes/node-a", ""),
            ("POST", "/v1/nodes/node-a", ""),
            ("POST", "/v1/nodes/node-a/storage-health", ""),
            ("GET", "/v1/nodes/node-a:drain", ""),
            ("GET", "/v1/nodes/node-a/storage-health?x=1", ""),
            ("GET", "/v1/nodes", ""),
            ("GET", "/v1/members", ""),
            ("GET", "/v1/members/node-a/safe-to-stop", ""),
            ("POST", "/v1/members/node-a", r#"{"state": "draining"}"#),
            ("GET", "/v1/enroll/nodes", ""),
            ("POST", "/v1/enroll/tokens", "{}"),
            ("GET", "/v1/frontend/drain", ""),
            ("POST", "/v1/frontend/drain", "{}"),
            ("POST", "/v1/denied", "{}"),
            ("GET", "/v1/nope", ""),
        ] {
            let reply = if method == "GET" {
                c.get(path)
            } else {
                c.post(path, body)
            };
            out.push((method, path, reply.expect("an answer").status));
        }
        out
    })
    .await;
    for (method, path, status) in answers {
        assert_eq!(status, 403, "{method} {path}");
    }
}

/// A certificate of any role but the updater's, even one for the very node of the call, is
/// refused the storage-health call, and so is an updater's certificate for another node: 403
/// each, over HTTP/2. The removed node routes are refused node-a's own updater as well.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_other_role_gets_the_node_calls() {
    let pki = Pki::new();
    let addr = frontend(pki.worker_tls(None), &store());

    let calls = [
        ("GET", "/v1/nodes/node-a/storage-health", ""),
        ("POST", "/v1/nodes/node-a:drain", r#"{"deadline_secs": 5}"#),
        ("POST", "/v1/nodes/node-a:uncordon", ""),
        ("GET", "/v1/nodes/node-a", ""),
        ("GET", "/v1/nodes", ""),
        ("GET", "/v1/members", ""),
    ];
    let certificates = [
        pki.client("daemon", "node-a"),
        pki.client("storage", "node-a"),
        pki.client("frontend", "node-a"),
        pki.client("coordinator", "node-a"),
        pki.updater("node-b", "daemon"),
    ];
    for leaf in &certificates {
        let channel = pki.channel(addr, leaf).await;
        for (method, path, body) in calls {
            assert_eq!(
                status_of(&channel, method, path, body).await,
                403,
                "{method} {path}"
            );
        }
    }

    let own = pki.channel(addr, &pki.updater("node-a", "daemon")).await;
    for (method, path, body) in &calls[1..] {
        assert_eq!(
            status_of(&own, method, path, body).await,
            403,
            "{method} {path}"
        );
    }
    assert_eq!(
        status_of(&own, "POST", "/v1/nodes/node-a:cordon", "").await,
        403
    );
}

/// The worker listener's deny list applies to an updater's node: a listed node is refused its
/// own storage-health call (403), another is served.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_deny_list_refuses_a_listed_nodes_updater() {
    let dir = scratch("denied");
    let pki = Pki::new();
    let list = dir.join("denied");
    std::fs::write(&list, "node node-a\n").expect("write");
    let deny = DenyList::open(&list).expect("a list");
    let fake = store();
    let addr = frontend(pki.worker_tls(Some(deny)), &fake);
    enroll(&*fake, 1, "node-a", &[Role::Storage], "192.0.2.1:8982");
    enroll(&*fake, 2, "node-b", &[Role::Storage], "192.0.2.2:8982");

    let (files, who) = pki.files(&sub(&dir, "a"), &pki.updater("node-a", "storage"));
    let denied = api(addr, files, who);
    let listed = blocking(move || {
        denied
            .get("/v1/nodes/node-a/storage-health")
            .expect("an answer")
            .status
    })
    .await;
    assert_eq!(listed, 403);
    let (files, who) = pki.files(&sub(&dir, "b"), &pki.updater("node-b", "storage"));
    let served = api(addr, files, who);
    let ok = blocking(move || {
        served
            .get("/v1/nodes/node-b/storage-health")
            .expect("an answer")
            .status
    })
    .await;
    assert_eq!(ok, 200);
}

fn registrant(i: u32) -> Registrant {
    Registrant {
        instance_id: format!("node-{i}"),
        addr: format!("192.0.2.{i}:8982"),
        zone: String::new(),
        weight: 1,
        protocol: PROTOCOL,
        progress: Progress::CaughtUp,
    }
}

/// The certificate the registry keeps for `role` of the node enrolled with token `seed`.
fn issued_role(role: Role, seed: u8, i: usize, now: u64) -> IssuedRole {
    let mut spki = [0u8; 32];
    spki[0] = seed;
    spki[1] = u8::try_from(i).expect("small");
    IssuedRole {
        role,
        spki_sha256: spki,
        serial: vec![0, seed, spki[1]],
        not_before: now - 300,
        not_after: now + 7 * DAY,
        cert_der: vec![seed, spki[1]],
    }
}

/// The node `name` enrolled in the registry of `store` with `roles`, advertising `advertise`.
fn enroll(store: &dyn Store, n: u8, name: &str, roles: &[Role], advertise: &str) {
    let spec = TokenSpec {
        roles: RoleSet::new(roles.iter().copied()).expect("roles"),
        host_role: None,
        max_uses: 1,
        ttl: None,
        node_id: None,
        node_prefix: None,
        names: Vec::new(),
        replace: false,
        label: "lab".to_owned(),
        created_by: "tester".to_owned(),
    };
    let token = TokenId::from_bits(u64::from(n));
    let secret = TokenSecret::from_bytes([n; 32]);
    create_token(store, &token, &secret, &spec).expect("token");
    let node: NodeId = name.parse().expect("a node id");
    let csrs: Vec<(Role, &[u8])> = roles.iter().map(|r| (*r, b"csr".as_slice())).collect();
    let now = store.now().as_duration().as_secs();
    let request = EnrollRequest {
        token_id: &token,
        secret: &[n; 32],
        node_id: &node,
        csrs: &csrs,
        advertise,
        zone: "z",
    };
    let mut sign = || {
        let issued = roles.iter().enumerate();
        Ok(issued
            .map(|(i, role)| issued_role(*role, n, i, now))
            .collect())
    };
    enroll_txn(store, &request, &mut sign).expect("enrolled");
}

/// A storage host's updater reads its own storage health from the frontend and judges its
/// update by it: healthy when its member is active on the target's commit and nothing is owed,
/// waiting when the build is another, and it can be told it has no storage half. No other
/// member or address is in what it reads.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_storage_hosts_updater_reads_its_own_storage_health() {
    let dir = scratch("storage");
    let pki = Pki::new();
    let fake = store();
    let addr = frontend(pki.worker_tls(None), &fake);

    let members = Members::new(Arc::clone(&fake) as Arc<dyn Store>, config());
    let registered = [1, 2, 3].map(|i| members.register(&registrant(i)).expect("register"));
    let beat = |index: usize, commit: Option<&str>| {
        let facts = NodeFacts {
            build_commit: commit.map(str::to_owned),
            ..NodeFacts::default()
        };
        let said = Reported {
            debts: Debts::Known(0),
            ..Reported::default()
        };
        let node = &registered[index];
        members
            .heartbeat_reporting(
                node.id,
                node.boot,
                Progress::CaughtUp,
                said,
                (Fill::default(), &facts),
                None,
            )
            .expect("a beat");
    };
    beat(0, Some(COMMIT));
    beat(1, Some("fedcba9876543210fedcba9876543210fedcba98"));
    beat(2, Some(COMMIT));
    enroll(
        &*fake,
        1,
        "linux-1",
        &[Role::Storage, Role::Daemon],
        "192.0.2.1:8982",
    );
    enroll(&*fake, 2, "linux-2", &[Role::Storage], "192.0.2.2:8982");
    enroll(&*fake, 3, "linux-3", &[Role::Daemon], "");
    enroll(&*fake, 4, "linux-4", &[Role::Storage], "192.0.2.9:8982");

    let judged = |node: &'static str| {
        let (files, who) = pki.files(&sub(&dir, node), &pki.updater(node, "storage"));
        let client = api(addr, files, who);
        let health = NodeHealth::for_updater(client);
        let storage = host(node, Kind::Storage, node);
        blocking(move || health.check(&storage, &target()))
    };
    assert_eq!(judged("linux-1").await, Ok(Health::Healthy));
    match judged("linux-2").await {
        Ok(Health::Waiting(why)) => assert!(why.contains("not the target's"), "{why}"),
        other => panic!("{other:?}"),
    }
    match judged("linux-3").await {
        Ok(Health::Abandoned(why)) => assert!(why.contains("no storage"), "{why}"),
        other => panic!("{other:?}"),
    }
    match judged("linux-4").await {
        Ok(Health::Waiting(why)) => assert!(why.contains("no storage member"), "{why}"),
        other => panic!("{other:?}"),
    }

    // The raw answer holds the host's own member and the two numbers, and nothing of the others.
    let channel = pki.channel(addr, &pki.updater("linux-1", "storage")).await;
    assert_eq!(
        status_of(&channel, "GET", "/v1/nodes/linux-1/storage-health", "").await,
        200
    );
    assert_eq!(
        status_of(&channel, "GET", "/v1/nodes/linux-2/storage-health", "").await,
        403
    );
}
