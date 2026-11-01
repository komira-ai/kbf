//! The operator's deny list at the membership service, over real mutual TLS on the worker
//! listener of a frontend with a state store: `Register` reads the `deny/` rows (an entry
//! counts at once, and a refusal writes nothing), and `Heartbeat` reads the map's list
//! before it touches the member's row, so a refused node's row lapses and the member goes
//! `Down`.

#[path = "support/hooked.rs"]
mod hooked;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use hooked::Hooked;
use kbf_front::Cache;
use kbf_map::{DenyEntry, MemberId, MemberState};
use kbf_proto::kbf::map::v1 as pb;
use kbf_proto::kbf::map::v1::membership_client::MembershipClient;
use kbf_server::members::{Members, MembershipConfig, PROTOCOL};
use kbf_server::proxy_auth::ProxySecret;
use kbf_server::{Behind, EpochPolicy, Listeners, StateStore, WorkerTls, bind_server_with_options};
use kbf_state::Store;
use kbf_storage::service::ROLE_URI_PREFIX;
use kbf_tls::TlsSource;
use rcgen::{
    BasicConstraints, CertificateParams, CertifiedIssuer, DnType, ExtendedKeyUsagePurpose, IsCa,
    KeyPair, KeyUsagePurpose, PublicKeyData, SanType, SerialNumber,
};
use sha2::{Digest, Sha256};
use tonic::Code;
use tonic::transport::{Certificate, Channel, ClientTlsConfig, Endpoint, Identity};

const SECOND: Duration = Duration::from_secs(1);

fn config() -> MembershipConfig {
    MembershipConfig {
        ttl: Duration::from_secs(60),
        dwell: Duration::from_secs(60),
        replication: 2,
    }
}

/// A storage node's client certificate and what a deny entry spells about it.
struct Node {
    client: MembershipClient<Channel>,
    host: String,
    spki_sha256: String,
}

/// A cell CA, the worker listener's certificate, and storage certificates on demand.
struct Pki {
    ca: CertifiedIssuer<'static, KeyPair>,
}

impl Pki {
    fn new() -> Self {
        let mut ca = CertificateParams::new(Vec::<String>::new()).expect("CA params");
        ca.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        ca.distinguished_name
            .push(DnType::CommonName, "kbf test CA");
        ca.key_usages = vec![
            KeyUsagePurpose::KeyCertSign,
            KeyUsagePurpose::DigitalSignature,
        ];
        Self {
            ca: CertifiedIssuer::self_signed(ca, KeyPair::generate().expect("key")).expect("CA"),
        }
    }

    fn worker_tls(&self) -> WorkerTls {
        let mut params = CertificateParams::new(vec!["localhost".to_owned()]).expect("params");
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        let key = KeyPair::generate().expect("key");
        let cert = params.signed_by(&key, &self.ca).expect("sign");
        WorkerTls {
            server: TlsSource::fixed(
                "worker",
                cert.pem().as_bytes(),
                key.serialize_pem().as_bytes(),
                Some(self.ca.pem().as_bytes()),
            )
            .expect("server TLS"),
            deny_list: None,
        }
    }

    /// A storage node whose certificate carries the DNS name `name` and this `serial`, and
    /// that is dialled at `name:8982`, connected to the worker listener at `worker`.
    async fn node(&self, worker: SocketAddr, name: &str, serial: u64) -> Node {
        let mut params = CertificateParams::new(Vec::<String>::new()).expect("params");
        params.subject_alt_names = vec![
            SanType::DnsName(name.try_into().expect("an IA5 name")),
            SanType::URI(
                format!("{ROLE_URI_PREFIX}storage")
                    .try_into()
                    .expect("an IA5 name"),
            ),
        ];
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
        params.serial_number = Some(SerialNumber::from(serial));
        let key = KeyPair::generate().expect("key");
        let spki_sha256 = Sha256::digest(key.subject_public_key_info())
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        let cert = params.signed_by(&key, &self.ca).expect("sign");
        let tls = ClientTlsConfig::new()
            .ca_certificate(Certificate::from_pem(self.ca.pem()))
            .domain_name("localhost")
            .identity(Identity::from_pem(cert.pem(), key.serialize_pem()));
        let channel = Endpoint::from_shared(format!("https://{worker}"))
            .expect("endpoint")
            .tls_config(tls)
            .expect("TLS")
            .connect()
            .await
            .expect("connect");
        Node {
            client: MembershipClient::new(channel),
            host: format!("{name}:8982"),
            spki_sha256,
        }
    }
}

impl Node {
    fn registration(&self) -> pb::RegisterRequest {
        pb::RegisterRequest {
            role: pb::Role::Storage.into(),
            instance_id: self.host.clone(),
            addr: self.host.clone(),
            weight: 1,
            protocol: PROTOCOL,
            ..pb::RegisterRequest::default()
        }
    }
}

fn beat(joined: &pb::RegisterResponse) -> pb::HeartbeatRequest {
    pb::HeartbeatRequest {
        member_id: joined.member_id,
        boot: joined.boot,
        ..pb::HeartbeatRequest::default()
    }
}

fn listeners(worker_tls: WorkerTls) -> Listeners {
    let loopback: SocketAddr = "127.0.0.1:0".parse().expect("addr");
    Listeners {
        reapi: loopback,
        worker: loopback,
        reapi_tls: None,
        worker_tls: Some(worker_tls),
        heartbeat_interval: Duration::from_secs(5),
        hello_wait: Duration::from_secs(10),
        tick: Duration::from_millis(50),
        unservable_wait: kbf_sched::UNSERVABLE_WAIT,
        default_platform: kbf_front::DefaultPlatform::NONE,
        qos_allowed: kbf_front::QosAllow::DEFAULT,
        memory_ladder: kbf_server::MemoryLadder::default(),
        memory_climb: true,
        vm_host_floor: kbf_server::HostFloor::default(),
        learned_sizes: kbf_server::LearnedSizes::Off,
        exclusive_actions: false,
        scheduler_core: kbf_server::SchedulerCore::under_test(),
        placement_load: kbf_server::PlacementLoad::Off,
        placement_predictions: kbf_server::PlacementPredictions::Off,
        usage_profiles: kbf_server::UsageProfiles::Off,
        whole_node: None,
        finished_retention: kbf_sched::FINISHED_RETENTION,
        shutdown_timeout: Duration::from_secs(10),
        store_probe_timeout: kbf_server::health::STORE_PROBE_TIMEOUT,
        invocation_memory: kbf_front::DEFAULT_INVOCATION_MEMORY,
        segments: kbf_front::SegmentsConfig::DEFAULT,
        streams: kbf_server::StreamTiming::default(),
        stream_limits: kbf_server::StreamLimits::DEFAULT,
    }
}

/// A frontend with a state store over mutual TLS: the worker listener's address, the
/// store behind it with its clock, and another frontend over the same store (the
/// operator's side: it writes the deny list and refreshes the map).
fn frontend(pki: &Pki) -> (SocketAddr, Arc<Hooked>, Members) {
    frontend_behind(pki, None)
}

/// [`frontend`], behind a proxy when `proxy` is set.
fn frontend_behind(pki: &Pki, proxy: Option<Behind>) -> (SocketAddr, Arc<Hooked>, Members) {
    let store = Hooked::new();
    let state = StateStore {
        store: Arc::clone(&store) as Arc<dyn Store>,
        epoch: EpochPolicy::Keep,
        members: config(),
        history: None,
    };
    let bound = bind_server_with_options(
        Arc::new(Cache::memory()),
        listeners(pki.worker_tls()),
        None,
        kbf_auth::Policy::allow_all(),
        Some(state),
        None,
        None,
        proxy,
        std::future::pending::<()>(),
    )
    .expect("binds");
    tokio::spawn(bound.serving);
    let ops = Members::new(Arc::clone(&store.fake) as Arc<dyn Store>, config());
    (bound.worker, store, ops)
}

/// Catches a registration that an entry of the deny list does not stop, or stops only after
/// the map has caught up, or stops after it has taken the member over. Four nodes register.
/// The operator then denies the first by serial, the second by the hash of its public key
/// and the third by node id (the DNS name its certificate carries), with no refresh: each
/// of the three is refused `Register` PERMISSION_DENIED saying the entry, in the
/// canonical spelling an operator can paste back, and the fourth registers again. The refusal
/// of the first wrote nothing: once it is allowed again (and the map refreshed) its beat
/// with the boot it first got is still the live one, which a refusal that came after the
/// boot was assigned would have raised.
///
/// Catches: the `Register` not reading the rows; the rows read after the member
/// is registered; `node` entries matched against nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_denied_certificate_cannot_register_and_a_refusal_writes_nothing() {
    let pki = Pki::new();
    let (worker, _store, ops) = frontend(&pki);
    let mut nodes = Vec::new();
    for (name, serial) in [
        ("node-a", 0x0a1b),
        ("node-b", 2),
        ("node-c", 3),
        ("node-d", 4),
    ] {
        let mut node = pki.node(worker, name, serial).await;
        let request = node.registration();
        let joined = node
            .client
            .register(request)
            .await
            .expect("registers while nothing is denied")
            .into_inner();
        nodes.push((node, joined));
    }
    let entries = [
        DenyEntry::Serial("a1b".to_owned()),
        DenyEntry::SpkiSha256(nodes[1].0.spki_sha256.clone()),
        DenyEntry::Node("node-c".to_owned()),
    ];
    for entry in &entries {
        ops.deny(entry, "test").expect("deny");
    }

    for ((node, _), entry) in nodes.iter_mut().zip(&entries) {
        let request = node.registration();
        let refused = node
            .client
            .register(request)
            .await
            .expect_err("a denied certificate registers");
        assert_eq!(refused.code(), Code::PermissionDenied, "{entry}");
        assert_eq!(refused.message(), format!("denied: {entry}"));
    }
    let (d, joined_d) = &mut nodes[3];
    let request = d.registration();
    let again = d
        .client
        .register(request)
        .await
        .expect("an undenied certificate registers again")
        .into_inner();
    assert_eq!(again.member_id, joined_d.member_id);
    assert_eq!(again.boot, joined_d.boot + 1);

    assert!(ops.allow(&entries[0]).expect("allow"));
    ops.refresh().expect("refresh");
    let (a, joined_a) = &mut nodes[0];
    a.client
        .heartbeat(beat(joined_a))
        .await
        .expect("the refused registrations did not raise A's boot");
}

/// Catches a heartbeat that an entry in the map does not stop, or that renews the row it
/// refuses: A and B register and beat. The operator denies A and refreshes, which turns the
/// map's generation: A's next beat is PERMISSION_DENIED saying the entry, and B's is not.
/// Forty seconds on (the time to live is sixty) B beats and A's beat is refused again; thirty
/// more and the operator's refresh finds A's row lapsed since the first refusal did not
/// renew it, so A is `Down` and B is `Active`, and A's beat is still PERMISSION_DENIED
/// (the entry is judged before the row).
///
/// Catches: `Heartbeat` not reading the map's list; the list read after the row is
/// touched (the refused beat renews it, so A is still `Active` at seventy seconds).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_denied_member_is_refused_its_beats_and_goes_down_when_its_row_lapses() {
    let pki = Pki::new();
    let (worker, store, ops) = frontend(&pki);
    let mut a = pki.node(worker, "node-a", 0x0a1b).await;
    let mut b = pki.node(worker, "node-b", 2).await;
    let (request_a, request_b) = (a.registration(), b.registration());
    let joined_a = a
        .client
        .register(request_a)
        .await
        .expect("A registers")
        .into_inner();
    let joined_b = b
        .client
        .register(request_b)
        .await
        .expect("B registers")
        .into_inner();
    a.client.heartbeat(beat(&joined_a)).await.expect("A beats");
    b.client.heartbeat(beat(&joined_b)).await.expect("B beats");

    let entry = DenyEntry::Serial("a1b".to_owned());
    ops.deny(&entry, "retired").expect("deny");
    let before = ops.current_map().expect("read").expect("a map");
    let map = ops.refresh().expect("refresh").expect("a map");
    assert_eq!(map.generation(), before.generation() + 1);
    assert_eq!(map.denied(), std::slice::from_ref(&entry));

    let refused = a
        .client
        .heartbeat(beat(&joined_a))
        .await
        .expect_err("A is denied");
    assert_eq!(refused.code(), Code::PermissionDenied);
    assert_eq!(refused.message(), "denied: serial a1b");
    b.client
        .heartbeat(beat(&joined_b))
        .await
        .expect("B is not denied");

    store.clock.advance(40 * SECOND);
    b.client.heartbeat(beat(&joined_b)).await.expect("B beats");
    a.client
        .heartbeat(beat(&joined_a))
        .await
        .expect_err("A is still denied");
    store.clock.advance(30 * SECOND);
    let map = ops.refresh().expect("refresh").expect("a map");
    let state = |id: u64| map.member(MemberId(id)).map(|m| m.state);
    assert_eq!(state(joined_a.member_id), Some(MemberState::Down));
    assert_eq!(state(joined_b.member_id), Some(MemberState::Active));
    let refused = a
        .client
        .heartbeat(beat(&joined_a))
        .await
        .expect_err("A is still denied");
    assert_eq!(refused.code(), Code::PermissionDenied);
}

/// Catches a worker listener with mutual TLS that ignores `--proxy-auth-file`: its proxy
/// layer must read the secret even though a certificate is also there, so a call that
/// presents the secret with a role that is not a role is refused `PERMISSION_DENIED` before
/// `Register` reads it, and one the proxy vouched for as the certificate's own node and
/// role registers.
///
/// Catches: the mutual-TLS arm of `worker_peers` built with `ProxyAuthLayer::new(None)`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_flagged_mutual_tls_worker_listener_still_reads_the_proxys_word() {
    const SECRET: [u8; 32] = [0x77; 32];
    let dir =
        kbf_testkit::dir::TestDir::under(env!("CARGO_TARGET_TMPDIR"), "members-denied-secret");
    let path = dir.join("secret");
    std::fs::write(&path, SECRET).expect("write");
    std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o600))
        .expect("chmod");
    let secret = Arc::new(ProxySecret::from_file(&path).expect("a usable secret"));
    let pki = Pki::new();
    let (worker, _store, _ops) = frontend_behind(
        &pki,
        Some(Behind {
            secret,
            deny_list: None,
        }),
    );
    let mut node = pki.node(worker, "node-a", 1).await;
    let registration = node.registration();
    let call = |role: &str| {
        let mut request = tonic::Request::new(registration.clone());
        let meta = request.metadata_mut();
        meta.insert(
            "x-kbf-proxy-auth",
            hex::encode(SECRET).parse().expect("a value"),
        );
        meta.insert("x-kbf-node-id", "node-a".parse().expect("a value"));
        meta.insert("x-kbf-role", role.parse().expect("a value"));
        request
    };
    let refused = node.client.register(call("admin")).await;
    assert_eq!(
        refused.expect_err("a malformed role").code(),
        Code::PermissionDenied
    );
    let request = call("storage");
    node.client
        .register(request)
        .await
        .expect("the proxy vouched for node-a as storage");
}
