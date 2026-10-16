//! `kbf storage --frontend`: a storage node registers with a real frontend in this
//! process (`bind_server_with_state` over the fake state store and its manual clock),
//! heartbeats, and is expired by the frontend's own timer when it stops.

#[path = "support/hooked.rs"]
mod hooked;

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use hooked::Hooked;
use kbf_front::Cache;
use kbf_map::{MemberId, MemberState};
use kbf_server::join::{Identity as NodeIdentity, Joiner};
use kbf_server::members::{Members, MembershipConfig};
use kbf_server::storage_node::{StorageFlags, command, parse, start_storage};
use kbf_server::{EpochPolicy, Listeners, StateStore, WorkerTls, bind_server_with_state};
use kbf_state::{Store, Versioned};
use kbf_storage::service::ROLE_URI_PREFIX;
use rcgen::{
    BasicConstraints, CertificateParams, CertifiedIssuer, DnType, ExtendedKeyUsagePurpose, IsCa,
    KeyPair, KeyUsagePurpose, SanType,
};
use tonic::Code;
use tonic::transport::{Certificate, ClientTlsConfig, Endpoint, Identity, ServerTlsConfig};

const MS: Duration = Duration::from_millis(1);

fn dir(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("storage-register")
        .join(name);
    std::fs::create_dir_all(&dir).expect("directory");
    dir
}

fn flags(dir: &Path, more: &[String]) -> Result<StorageFlags, clap::Error> {
    let mut argv = vec!["storage".to_owned()];
    if !more.iter().any(|m| m.starts_with("--storage-listen")) {
        argv.push("--storage-listen=127.0.0.1:0".to_owned());
    }
    argv.extend([
        format!("--cold-store=file://{}", dir.join("cold").display()),
        format!("--meta-dir={}", dir.join("meta").display()),
    ]);
    argv.extend_from_slice(more);
    parse(&command().try_get_matches_from(argv)?)
}

fn never() -> std::io::Result<std::future::Pending<()>> {
    Ok(std::future::pending())
}

/// A TTL of 60 ms (beats every 20 ms) on a clock that moves only when a test moves it.
fn config() -> MembershipConfig {
    MembershipConfig {
        ttl: 60 * MS,
        dwell: Duration::from_secs(1),
        replication: 2,
    }
}

struct Frontend {
    worker: SocketAddr,
    store: Arc<Hooked>,
    members: Members,
}

fn listeners(worker_tls: Option<WorkerTls>) -> Listeners {
    let loopback: SocketAddr = "127.0.0.1:0".parse().expect("addr");
    Listeners {
        reapi: loopback,
        worker: loopback,
        reapi_tls: None,
        worker_tls,
        heartbeat_interval: Duration::from_secs(5),
        hello_wait: Duration::from_secs(10),
        tick: Duration::from_millis(50),
        unservable_wait: kbf_sched::UNSERVABLE_WAIT,
        memory_ladder: kbf_server::MemoryLadder::default(),
        finished_retention: kbf_sched::FINISHED_RETENTION,
        shutdown_timeout: Duration::from_secs(10),
        store_probe_timeout: kbf_server::health::STORE_PROBE_TIMEOUT,
        invocation_memory: kbf_front::DEFAULT_INVOCATION_MEMORY,
        segments: kbf_front::SegmentsConfig::DEFAULT,
        streams: kbf_server::StreamTiming::default(),
    }
}

/// A frontend with a state store: the membership service is on its worker listener and
/// its timer expires members.
fn frontend(worker_tls: Option<WorkerTls>) -> Frontend {
    let store = Hooked::new();
    let state = StateStore {
        store: Arc::clone(&store) as Arc<dyn Store>,
        epoch: EpochPolicy::Keep,
        members: config(),
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
    let worker = bound.worker;
    tokio::spawn(bound.serving);
    Frontend {
        worker,
        members: Members::new(Arc::clone(&store.fake) as Arc<dyn Store>, config()),
        store,
    }
}

async fn until<T>(what: &str, mut probe: impl FnMut() -> Option<T>) -> T {
    for _ in 0..1000 {
        if let Some(found) = probe() {
            return found;
        }
        tokio::time::sleep(5 * MS).await;
    }
    panic!("timed out waiting for {what}");
}

fn row(f: &Frontend, id: u64) -> Option<Versioned> {
    f.store.fake.get(&format!("member/{id}")).expect("read")
}

fn state(f: &Frontend, id: u64) -> Option<MemberState> {
    let map = f.members.current_map().expect("read")?;
    map.member(MemberId(id)).map(|m| m.state)
}

/// Catches: a node that registers but does not keep its row alive, a frontend that never
/// expires a node that stops, and a registration that tells the frontend something other
/// than what the node is. The node registers on start (its bound address, Joining); the
/// store's clock then moves 45 ms (TTL 60 ms) after each beat it sees, six times, which is
/// four TTLs and which the row survives only because the node beats; when the node stops,
/// 61 ms are enough for the frontend's own timer to mark it Down.
#[tokio::test]
async fn a_storage_node_registers_and_heartbeats_across_several_ttls() {
    let f = frontend(None);
    let url = format!("--frontend=http://{}", f.worker);
    let node = start_storage(&flags(&dir("beats"), &[url]).expect("flags"), never)
        .await
        .expect("starts");
    let addr = node.addr;
    let serving = tokio::spawn(node.serve());

    let mut seen = until("the node to register", || row(&f, 1)).await.version;
    let map = f.members.current_map().expect("read").expect("a map");
    let me = map.member(MemberId(1)).expect("the node is in the map");
    assert_eq!(
        (me.addr.clone(), me.state),
        (addr.to_string(), MemberState::Joining)
    );
    for round in 1..=6 {
        f.store.clock.advance(45 * MS);
        seen = until("a heartbeat", || {
            row(&f, 1).filter(|r| r.version != seen).map(|r| r.version)
        })
        .await;
        assert_eq!(state(&f, 1), Some(MemberState::Joining), "round {round}");
    }

    serving.abort();
    f.store.clock.advance(61 * MS);
    until("the frontend to mark the silent node Down", || {
        (state(&f, 1) == Some(MemberState::Down)).then_some(())
    })
    .await;
}

/// Catches: a node that treats a lapsed row as fatal or never notices. After its row
/// expires (the manual clock moves past the TTL right after a beat, and the frontend
/// marks it Down) its next heartbeat is NOT_FOUND; it registers again, keeps its member
/// id, gets boot 2 and is Joining.
#[tokio::test]
async fn a_node_whose_row_lapsed_registers_again() {
    let f = frontend(None);
    let url = format!("--frontend=http://{}", f.worker);
    let node = start_storage(&flags(&dir("lapse"), &[url]).expect("flags"), never)
        .await
        .expect("starts");
    let _serving = tokio::spawn(node.serve());
    let first = until("the node to register", || row(&f, 1)).await.version;
    until("a beat", || row(&f, 1).filter(|r| r.version != first)).await;
    f.store.clock.advance(61 * MS);
    // Boot 2 is the registration after the lapse: a beat of boot 2 is accepted, and the
    // member has the id it had.
    until("the node to register again", || {
        f.members.heartbeat(MemberId(1), 2).ok()
    })
    .await;
    assert_eq!(state(&f, 1), Some(MemberState::Joining));
    assert_eq!(f.store.fake.scan_prefix("member/").expect("scan").len(), 1);
}

fn node_addr_text(f: &Frontend) -> String {
    let map = f.members.current_map().expect("read").expect("map");
    map.member(MemberId(1)).expect("member 1").addr.clone()
}

/// Catches: a node that gives up, or loses its place, when the frontend fails: the first
/// three registrations fail (the store errors; UNAVAILABLE), the node waits and tries
/// again until it is in; two heartbeats then fail, and the node keeps its registration
/// (boot 1, no second registration) and beats again afterwards.
#[tokio::test]
async fn a_node_retries_through_failures_and_keeps_its_boot() {
    let f = frontend(None);
    let channel = Endpoint::from_shared(format!("http://{}", f.worker))
        .expect("endpoint")
        .connect_lazy();
    let who = NodeIdentity {
        instance_id: "node-x".to_owned(),
        addr: "192.0.2.5:8982".to_owned(),
        zone: String::new(),
        weight: 1,
    };
    f.store
        .fail_gets
        .store(3, std::sync::atomic::Ordering::SeqCst);
    let joiner = tokio::spawn(Joiner::new(channel, who, 5 * MS).run());
    let first = until("the node to register", || row(&f, 1)).await.version;
    f.store
        .fail_gets
        .store(2, std::sync::atomic::Ordering::SeqCst);
    f.store.clock.advance(10 * MS);
    until("a heartbeat after the failures", || {
        row(&f, 1).filter(|r| r.version != first)
    })
    .await;
    assert_eq!(state(&f, 1), Some(MemberState::Joining));
    assert_eq!(
        f.store.fake.scan_prefix("member/").expect("scan").len(),
        1,
        "one member, registered once"
    );
    joiner.abort();
}

/// Catches: flags that are ignored or accepted when they cannot work: the advertised
/// address, zone and weight are what the frontend records; the node refuses a flag that
/// needs `--frontend` without it, a non-http URL, `https` without the certificate flags,
/// and a wildcard listen address with no `--advertise-addr` (it would register an address
/// nobody can dial); and a node without `--frontend` registers nowhere.
#[tokio::test]
async fn the_registration_flags_are_carried_or_refused() {
    let f = frontend(None);
    let url = format!("--frontend=http://{}", f.worker);
    let more = [
        url.clone(),
        "--advertise-addr=192.0.2.7:8982".to_owned(),
        "--storage-zone=eu-1".to_owned(),
        "--storage-weight=3".to_owned(),
    ];
    let node = start_storage(&flags(&dir("flags"), &more).expect("flags"), never)
        .await
        .expect("starts");
    let _serving = tokio::spawn(node.serve());
    until("the node to register", || row(&f, 1)).await;
    let map = f.members.current_map().expect("read").expect("map");
    let me = map.member(MemberId(1)).expect("member");
    assert_eq!(
        (me.addr.as_str(), me.zone.as_str(), me.weight),
        ("192.0.2.7:8982", "eu-1", 3)
    );

    let d = dir("flag-refusals");
    for (argv, why) in [
        (
            vec!["--advertise-addr=192.0.2.4:5".to_owned()],
            "needs --frontend",
        ),
        (vec!["--storage-zone=z".to_owned()], "needs --frontend"),
        (vec!["--frontend=ftp://x".to_owned()], "http:// or https://"),
        (
            vec!["--frontend=https://x".to_owned()],
            "--storage-tls-cert",
        ),
        (
            vec![
                "--frontend=http://x".to_owned(),
                "--storage-listen=0.0.0.0:0".to_owned(),
            ],
            "--advertise-addr is required",
        ),
    ] {
        let refused = flags(&d, &argv).expect_err(why).to_string();
        assert!(
            refused.contains(why) || refused.contains("required arguments"),
            "{argv:?}: {refused}"
        );
    }
    let alone = flags(&d, &[]).expect("flags");
    let none = alone.joiner("127.0.0.1:1".parse().expect("addr"));
    assert!(
        none.expect("no error").is_none(),
        "no --frontend, no registration"
    );
    let help = command().render_help().to_string();
    for flag in [
        "--frontend",
        "--advertise-addr",
        "--storage-zone",
        "--storage-weight",
    ] {
        assert!(help.contains(flag), "{flag}");
    }
}

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

    /// A certificate for `localhost` naming `role`, good for serving and for calling.
    fn cert(&self, role: &str) -> (String, String) {
        let mut params = CertificateParams::new(vec!["localhost".to_owned()]).expect("params");
        params.subject_alt_names.push(SanType::URI(
            format!("{ROLE_URI_PREFIX}{role}").try_into().expect("IA5"),
        ));
        params.extended_key_usages = vec![
            ExtendedKeyUsagePurpose::ServerAuth,
            ExtendedKeyUsagePurpose::ClientAuth,
        ];
        let key = KeyPair::generate().expect("key");
        let cert = params.signed_by(&key, &self.ca).expect("sign");
        (cert.pem(), key.serialize_pem())
    }
}

/// Catches: registration that works only in plain text, or that admits any certificate.
/// Over mutual TLS a node whose certificate names the role `storage` registers (it
/// presents its own certificate to the frontend and trusts the CA it was given); a
/// certificate that names the role `daemon` is refused PERMISSION_DENIED.
#[tokio::test]
async fn over_mutual_tls_only_the_storage_role_registers() {
    let pki = Pki::new();
    let (frontend_cert, frontend_key) = pki.cert("frontend");
    let worker_tls = WorkerTls {
        server: ServerTlsConfig::new()
            .identity(Identity::from_pem(frontend_cert, frontend_key))
            .client_ca_root(Certificate::from_pem(pki.ca.pem())),
        deny_list: None,
    };
    let f = frontend(Some(worker_tls));
    let d = dir("tls");
    let write = |name: &str, text: String| {
        let path = d.join(name);
        std::fs::write(&path, text).expect("write");
        path.display().to_string()
    };
    let (cert, key) = pki.cert("storage");
    let more = [
        format!("--frontend=https://localhost:{}", f.worker.port()),
        format!("--storage-tls-cert={}", write("node.pem", cert)),
        format!("--storage-tls-key={}", write("node.key", key)),
        format!("--storage-client-ca={}", write("ca.pem", pki.ca.pem())),
    ];
    let node = start_storage(&flags(&d, &more).expect("flags"), never)
        .await
        .expect("starts");
    let addr = node.addr;
    let _serving = tokio::spawn(node.serve());
    until("the node to register over TLS", || row(&f, 1)).await;
    assert_eq!(node_addr_text(&f), addr.to_string());

    let (cert, key) = pki.cert("daemon");
    let tls = ClientTlsConfig::new()
        .ca_certificate(Certificate::from_pem(pki.ca.pem()))
        .identity(Identity::from_pem(cert, key));
    let channel = Endpoint::from_shared(format!("https://localhost:{}", f.worker.port()))
        .expect("endpoint")
        .tls_config(tls)
        .expect("TLS")
        .connect_lazy();
    let who = NodeIdentity {
        instance_id: "daemon-box".to_owned(),
        addr: "192.0.2.9:1".to_owned(),
        zone: String::new(),
        weight: 1,
    };
    let denied = Joiner::new(channel, who, 5 * MS)
        .register()
        .await
        .expect_err("denied");
    assert_eq!(denied.code(), Code::PermissionDenied);
}

/// Catches: a retry that does not back off (a down frontend would be hit at the base
/// rate), one that grows without bound, and one that overflows after many failures: the
/// wait doubles from the base to 30 s and stays there, however many failures.
#[test]
fn the_wait_after_failures_doubles_to_a_cap() {
    use kbf_server::join::{RETRY_BASE, RETRY_MAX, backoff};
    let secs = |failures| backoff(RETRY_BASE, failures).as_secs();
    assert_eq!([secs(0), secs(1), secs(2), secs(4)], [1, 2, 4, 16]);
    assert_eq!(backoff(RETRY_BASE, 5), RETRY_MAX);
    assert_eq!(backoff(RETRY_BASE, 40), RETRY_MAX);
    assert_eq!(backoff(RETRY_BASE, u32::MAX), RETRY_MAX);
    assert_eq!(backoff(5 * MS, 1), 10 * MS);
}
