//! A host's updater against the frontend's real enroll listener: `kbf-updater enroll` with a
//! one-time token, the files it leaves, what the CA refuses, and the agent's renewal at half
//! the certificate's life. The updater side is the real code (`kbf_updater::agent::enroll`
//! and `kbf_updater::agent::renew`), the listener is the one a frontend starts (`prepare`),
//! and the registry is the in-memory state store, whose clock the test moves.

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use kbf_enroll::store::Store as NodeStore;
use kbf_enroll_registry::{TokenSpec, create_token};
use kbf_peer::{PeerRole, names_of, role_of, uri_values_of};
use kbf_server::enroll::ca::SignerChoice;
use kbf_server::enroll::flags::EnrollFlags;
use kbf_server::enroll::limits::Limits;
use kbf_server::enroll::service::NodeSettings;
use kbf_server::enroll::start::prepare;
use kbf_state::fake::{Fake, Flaw};
use kbf_state::{Clock as _, ManualClock, Store};
use kbf_testkit::dir::TestDir;
use kbf_types::{CaPin, HostRole, RefusalReason, Role, RoleSet, Token, TokenId, TokenSecret};
use kbf_updater::agent::enroll::{EnrollFlags as UpdaterFlags, run_enroll};
use kbf_updater::agent::renew::{FrontendRenewer, Renewer as _};
use kbf_updater::secret::euid;

fn unix_now() -> i64 {
    i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("time")
            .as_secs(),
    )
    .expect("time")
}

fn scratch(name: &str) -> TestDir {
    TestDir::under(
        env!("CARGO_TARGET_TMPDIR"),
        &format!("kbf-updater-e2e-{name}"),
    )
}

/// A running enroll listener over a fresh store and CA.
struct Farm {
    clock: Arc<ManualClock>,
    store: Arc<dyn Store>,
    port: u16,
    pin: CaPin,
    ca_dir: PathBuf,
    /// The CA the listener issues from: the farm's, which also issues the coordinator's
    /// certificate.
    ca: Arc<kbf_server::enroll::ca::Ca>,
    _latch: kbf_server::enroll::start::Serving,
    /// Declared last: the directory outlives the listener that reads the CA from it.
    _dir: TestDir,
}

async fn start(name: &str) -> Farm {
    let clock = Arc::new(ManualClock::default());
    clock.advance(Duration::from_secs(
        u64::try_from(unix_now()).expect("time"),
    ));
    let store: Arc<dyn Store> = Arc::new(Fake::with_flaw(Arc::clone(&clock), Flaw::None));
    let dir = scratch(name);
    let ca_dir = dir.join("ca");
    let flags = EnrollFlags {
        listen: "127.0.0.1:0".parse().expect("addr"),
        ca_dir: ca_dir.clone(),
        signer: SignerChoice::DevFile,
        frontend_names: vec!["localhost".to_owned()],
        cert_lifetime_secs: 86_400,
        limits: Limits::DEFAULT,
        allow_unenforced_deny: true,
        settings: NodeSettings {
            worker_endpoint: "https://localhost:7443".to_owned(),
            cas_endpoint: String::new(),
            storage_port: 8983,
        },
        renew_every: None,
    };
    let prepared = prepare(&flags).expect("prepared");
    let (port, pin, ca) = (prepared.local().port(), prepared.ca().pin(), prepared.ca());
    let mut serving = prepared.serve(Arc::clone(&store), None).expect("serving");
    let listener = std::mem::replace(&mut serving.listener, Box::pin(std::future::pending()));
    tokio::spawn(listener);
    Farm {
        clock,
        store,
        port,
        pin,
        ca_dir,
        ca,
        _latch: serving,
        _dir: dir,
    }
}

impl Farm {
    fn now(&self) -> i64 {
        i64::try_from(self.store.now().as_duration().as_secs()).expect("time")
    }

    fn advance(&self, secs: u64) {
        self.clock.advance(Duration::from_secs(secs));
    }

    /// A token as an operator makes one: for the host `node` (if given) and `host_role` for
    /// an updater, else for `roles` as a node's.
    fn token(
        &self,
        bits: u64,
        roles: &[Role],
        node: Option<&str>,
        host_role: Option<HostRole>,
        ttl: Option<Duration>,
    ) -> Token {
        let id = TokenId::from_bits(bits);
        #[allow(clippy::cast_possible_truncation)]
        let secret = TokenSecret::from_bytes([bits as u8; 32]);
        create_token(
            &*self.store,
            &id,
            &secret,
            &TokenSpec {
                roles: RoleSet::new(roles.iter().copied()).expect("roles"),
                host_role,
                max_uses: 1,
                ttl,
                node_id: node.map(|n| n.parse().expect("node id")),
                node_prefix: None,
                names: Vec::new(),
                replace: false,
                label: "e2e".to_owned(),
                created_by: "e2e".to_owned(),
            },
        )
        .expect("token");
        Token::new(id, secret, self.pin)
    }

    fn updater_token(&self, bits: u64, node: &str, host_role: HostRole) -> Token {
        self.token(bits, &[Role::Updater], Some(node), Some(host_role), None)
    }
}

/// A host: where its token and its files are.
struct Host {
    /// The command's arguments, and what they parse to.
    args: Vec<String>,
    flags: UpdaterFlags,
    root: TestDir,
}

fn host(name: &str, farm: &Farm, node: &str, token: &Token) -> Host {
    let root = scratch(name);
    let token_file = root.join("enroll.token");
    std::fs::write(&token_file, token.to_line()).expect("token file");
    std::fs::set_permissions(&token_file, std::fs::Permissions::from_mode(0o600)).expect("mode");
    let args: Vec<String> = [
        "--frontend",
        &format!("localhost:{}", farm.port),
        "--node-id",
        node,
        "--token-file",
        token_file.to_str().expect("utf8"),
        "--state-dir",
        root.join("state").to_str().expect("utf8"),
    ]
    .map(str::to_owned)
    .to_vec();
    let flags = kbf_updater::agent::enroll::parse_flags(&args).expect("flags");
    Host { args, flags, root }
}

impl Host {
    fn pki(&self, name: &str) -> PathBuf {
        self.flags.state_dir.join("pki").join(name)
    }

    fn mode(&self, name: &str) -> u32 {
        std::fs::metadata(self.pki(name))
            .expect("a file")
            .permissions()
            .mode()
            & 0o777
    }

    fn cert_der(&self) -> Vec<u8> {
        let pem = std::fs::read(self.pki("updater.pem")).expect("cert");
        x509_parser::pem::parse_x509_pem(&pem)
            .expect("a PEM certificate")
            .1
            .contents
    }
}

/// Runs `kbf-updater enroll` on a blocking thread (the command makes a runtime of its own): its
/// text when it exits 0, else the text of its failure.
async fn enroll(host: &Host) -> Result<String, String> {
    let args = host.args.clone();
    let (code, text) = tokio::task::spawn_blocking(move || run_enroll(&args, euid(), unix_now()))
        .await
        .expect("joined");
    if code == 0 { Ok(text) } else { Err(text) }
}

fn names_in(dir: &Path) -> Vec<String> {
    let mut found = Vec::new();
    for entry in std::fs::read_dir(dir).expect("dir") {
        let entry = entry.expect("entry");
        let name = entry.file_name().to_string_lossy().into_owned();
        if entry.file_type().expect("type").is_dir() {
            for inner in names_in(&entry.path()) {
                found.push(format!("{name}/{inner}"));
            }
        } else {
            found.push(name);
        }
    }
    found.sort();
    found
}

/// A host enrolls with its one-time token: the certificate is the updater's for exactly that node
/// and host role, made for the key the host made, with a private key of mode 0600 and nothing of
/// the CA's but its public bundle on the host; the token file is gone; the node listing of the
/// registry does not show it.
///
/// Catches: a certificate with the wrong node id or host role, a key written wider than 0600, a
/// CA key or any other secret left on the host, a token left behind to be used twice.
#[tokio::test(flavor = "multi_thread")]
async fn a_host_enrolls_with_its_token_and_gets_its_own_updater_certificate() {
    let farm = start("enroll").await;
    let token = farm.updater_token(0x77, "host-1", HostRole::MAC_DAEMON);
    let host = host("enroll-host", &farm, "host-1", &token);
    let said = enroll(&host).await.expect("enrolled");
    assert!(
        said.contains("enrolled the updater of node host-1"),
        "{said}"
    );

    let der = host.cert_der();
    assert_eq!(role_of(&der), Ok(PeerRole::Updater));
    assert_eq!(names_of(&der).expect("names").dns(), ["host-1"]);
    assert_eq!(
        uri_values_of(&der, HostRole::URI_PREFIX),
        Ok(vec!["mac-daemon".to_owned()])
    );
    assert_eq!(host.mode("updater.key"), 0o600);
    assert_eq!(host.mode("node.json"), 0o600);
    assert_eq!(
        names_in(&host.flags.state_dir),
        [
            "pki/ca.pem",
            "pki/node.json",
            "pki/updater.key",
            "pki/updater.pem"
        ]
    );
    let bundle = std::fs::read_to_string(host.pki("ca.pem")).expect("bundle");
    assert_eq!(
        bundle,
        std::fs::read_to_string(farm.ca_dir.join("ca.pem")).expect("ca")
    );
    assert!(
        !host.root.join("enroll.token").exists(),
        "the token is spent"
    );
    assert!(
        kbf_enroll_registry::list_nodes(&*farm.store)
            .expect("nodes")
            .is_empty()
    );
}

/// What the CA refuses: a token already used, one that expired, one for another node id, and one
/// that grants a node's roles and not an updater's; the host is left with no files and the token
/// file where it was.
#[tokio::test(flavor = "multi_thread")]
async fn a_used_expired_wrong_node_or_wrong_role_token_makes_no_certificate() {
    let farm = start("refused").await;
    let used = farm.updater_token(0x71, "host-1", HostRole::DAEMON);
    let first = host("refused-first", &farm, "host-1", &used);
    enroll(&first).await.expect("the first use");
    let second = host("refused-second", &farm, "host-1", &used);
    let expired = farm.token(
        0x72,
        &[Role::Updater],
        Some("host-1"),
        Some(HostRole::DAEMON),
        Some(Duration::from_secs(600)),
    );
    let late = host("refused-late", &farm, "host-1", &expired);
    farm.advance(601);
    let other_node = farm.updater_token(0x73, "host-9", HostRole::DAEMON);
    let wrong_node = host("refused-node", &farm, "host-1", &other_node);
    let for_nodes = farm.token(0x74, &[Role::Storage, Role::Daemon], None, None, None);
    let wrong_role = host("refused-role", &farm, "host-1", &for_nodes);
    for (host, why) in [
        (&second, RefusalReason::TokenUsedUp),
        (&late, RefusalReason::TokenExpired),
        (&wrong_node, RefusalReason::NodeIdNotAllowed),
        (&wrong_role, RefusalReason::RoleNotGranted),
    ] {
        let error = enroll(host).await.expect_err("refused");
        assert!(error.contains(why.message()), "{why:?}: {error}");
        assert!(
            host.root.join("enroll.token").exists(),
            "{why:?}: kept for a retry"
        );
        assert!(!host.pki("updater.pem").exists(), "{why:?}");
    }
}

/// The agent's renewer waits until half the certificate's life, then renews with the key the host
/// made and no token: the key file is the same, the certificate is new, for the same node and host
/// role, and the next renewal is due half a life after that.
///
/// Catches: a renewer that renews early or never, one that makes a new key (the registry holds
/// the first), one that changes the node or the host role, and one that does not wait again.
#[tokio::test(flavor = "multi_thread")]
async fn the_agent_renews_the_certificate_at_half_its_life_with_the_same_key() {
    let farm = start("renew").await;
    let token = farm.updater_token(0x78, "host-1", HostRole::DAEMON);
    let host = host("renew-host", &farm, "host-1", &token);
    enroll(&host).await.expect("enrolled");
    let node = NodeStore::open(&host.flags.state_dir)
        .expect("store")
        .load()
        .expect("load")
        .expect("enrolled");
    let (key_before, cert_before) = (
        std::fs::read(host.pki("updater.key")).expect("key"),
        host.cert_der(),
    );

    let logs = Arc::new(Mutex::new(Vec::<String>::new()));
    let sink = Arc::clone(&logs);
    let clock = Arc::clone(&farm.clock);
    let store = NodeStore::open(&host.flags.state_dir).expect("store");
    let url = kbf_updater::agent::enroll::frontend_url(&host.flags.frontend);
    let renewer = Arc::new(
        FrontendRenewer::new(
            url,
            store,
            Arc::new(move || i64::try_from(clock.now().as_duration().as_secs()).expect("time")),
            Arc::new(move |line| sink.lock().expect("log").push(line.to_owned())),
            0,
        )
        .expect("a renewer"),
    );
    let round = |renewer: Arc<FrontendRenewer>| async move {
        tokio::task::spawn_blocking(move || renewer.round())
            .await
            .expect("joined")
    };

    // One second before half a life: nothing is called, nothing changes.
    let until = u64::try_from(node.renew_after - farm.now()).expect("in the future");
    farm.advance(until - 1);
    round(Arc::clone(&renewer)).await;
    assert_eq!(host.cert_der(), cert_before);
    assert!(logs.lock().expect("log").is_empty());

    // At half a life the certificate is renewed.
    farm.advance(1);
    round(Arc::clone(&renewer)).await;
    assert_eq!(
        logs.lock().expect("log").len(),
        1,
        "{:?}",
        logs.lock().expect("log")
    );
    assert!(logs.lock().expect("log")[0].contains("renewed the certificate"));
    let cert_after = host.cert_der();
    assert_ne!(cert_after, cert_before);
    assert_eq!(
        std::fs::read(host.pki("updater.key")).expect("key"),
        key_before
    );
    assert_eq!(role_of(&cert_after), Ok(PeerRole::Updater));
    assert_eq!(names_of(&cert_after).expect("names").dns(), ["host-1"]);
    assert_eq!(
        uri_values_of(&cert_after, HostRole::URI_PREFIX),
        Ok(vec!["daemon".to_owned()])
    );
    let renewed = NodeStore::open(&host.flags.state_dir)
        .expect("store")
        .load()
        .expect("load")
        .expect("enrolled");
    assert!(renewed.renew_after > node.renew_after);
    assert!(renewed.earliest_not_after() > node.earliest_not_after());

    // Not again until the next half life.
    round(Arc::clone(&renewer)).await;
    assert_eq!(logs.lock().expect("log").len(), 1);
    // The renewer owns a runtime, which may not be dropped where a task runs.
    tokio::task::spawn_blocking(move || drop(renewer))
        .await
        .expect("joined");
}

/// The coordinator's side of a handshake, with a certificate the farm's CA issued it for
/// `coordinator.farm.example`, and the farm's root as the only one a caller's certificate may
/// chain to.
fn coordinator_tls(farm: &Farm) -> Arc<rustls::ServerConfig> {
    use rcgen::PublicKeyData as _;
    use rustls::pki_types::pem::PemObject as _;
    use rustls::pki_types::{CertificateDer, PrivateKeyDer};

    let pair = rcgen::KeyPair::generate().expect("a key");
    let public =
        kbf_ca::PublicKey::from_spki(&pair.subject_public_key_info()).expect("a P-256 key");
    let issued = farm
        .ca
        .node()
        .issue_coordinator(
            &[COORDINATOR],
            &public,
            unix_now(),
            &mut kbf_ca::SystemEntropy,
        )
        .expect("the coordinator's certificate");
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut roots = rustls::RootCertStore::empty();
    roots
        .add(CertificateDer::from(farm.ca.node().root_der().to_vec()))
        .expect("the root");
    let verifier = rustls::server::WebPkiClientVerifier::builder_with_provider(
        Arc::new(roots),
        provider.clone(),
    )
    .build()
    .expect("a verifier");
    let config = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .expect("versions")
        .with_client_cert_verifier(verifier)
        .with_single_cert(
            vec![CertificateDer::from(issued.der)],
            PrivateKeyDer::from_pem_slice(pair.serialize_pem().as_bytes()).expect("the key"),
        )
        .expect("a server configuration");
    Arc::new(config)
}

/// The coordinator's name, as the agent is given it and its certificate names it.
const COORDINATOR: &str = "coordinator.farm.example";

/// A whole handshake between the agent's own TLS client configuration (`client`, made from the
/// files `cfg` names) and the coordinator's side (`server`), exchanged in memory: no socket and no
/// name lookup. The certificate the coordinator was shown, or why the handshake failed.
fn handshake_in_memory(
    client: Arc<rustls::ClientConfig>,
    server: Arc<rustls::ServerConfig>,
) -> Result<Vec<u8>, String> {
    let name = rustls::pki_types::ServerName::try_from(COORDINATOR).map_err(|e| e.to_string())?;
    let mut agent = rustls::ClientConnection::new(client, name).map_err(|e| e.to_string())?;
    let mut coordinator = rustls::ServerConnection::new(server).map_err(|e| e.to_string())?;
    // Each round moves what one side has to say to the other; a handshake is a handful of them.
    for _ in 0..16 {
        if !agent.is_handshaking() && !coordinator.is_handshaking() {
            break;
        }
        let mut flight = Vec::new();
        while agent.wants_write() {
            agent.write_tls(&mut flight).map_err(|e| e.to_string())?;
        }
        coordinator
            .read_tls(&mut flight.as_slice())
            .map_err(|e| e.to_string())?;
        coordinator
            .process_new_packets()
            .map_err(|e| e.to_string())?;
        let mut flight = Vec::new();
        while coordinator.wants_write() {
            coordinator
                .write_tls(&mut flight)
                .map_err(|e| e.to_string())?;
        }
        agent
            .read_tls(&mut flight.as_slice())
            .map_err(|e| e.to_string())?;
        agent.process_new_packets().map_err(|e| e.to_string())?;
    }
    if agent.is_handshaking() || coordinator.is_handshaking() {
        return Err("the handshake did not end".to_owned());
    }
    coordinator
        .peer_certificates()
        .and_then(|c| c.first())
        .map(|c| c.as_ref().to_vec())
        .ok_or_else(|| "the agent showed no certificate".to_owned())
}

/// What the test's maker of the agent says when it ends the run, having made the agent's TLS
/// from the enrollment.
const MADE: &str = "the agent was made from the enrollment; the test ends it here";

/// The agent as the packaged unit starts it, at boot, on a host not enrolled yet: it says once that
/// it is not enrolled and waits, making nothing; the operator runs `kbf-updater enroll` against
/// the frontend's real enroll listener with a token for another node, which is refused, and the
/// agent looks again and still waits, saying nothing more and making nothing; the operator runs it
/// again with the right token; the agent, at its next look, goes on to make itself from the files
/// the enrollment wrote, with no restart. Its own TLS client configuration, made from those files,
/// completes a mutual-TLS handshake, in memory, with a coordinator whose certificate the farm's CA
/// issued, and the coordinator is shown the enrolled certificate. (That the agent made then
/// heartbeats is `kbf-updater`'s `once_the_host_is_enrolled_the_waiting_agent_heartbeats`.)
///
/// Catches: an agent that fails and is restarted in a loop until the host is enrolled, one that
/// takes what a refused attempt leaves (a key, no certificate) for an enrollment, one that must be
/// restarted by hand after `enroll`, and an enrollment whose files the coordinator would refuse.
#[tokio::test(flavor = "multi_thread")]
async fn an_agent_started_before_kbf_updater_enroll_starts_from_it_once_it_has_run() {
    use std::sync::mpsc::channel;

    let farm = start("agent-waits").await;
    let other_node = farm.updater_token(0x7a, "host-9", HostRole::DAEMON);
    let token = farm.updater_token(0x79, "host-1", HostRole::DAEMON);
    let host = host("agent-waits-host", &farm, "host-1", &other_node);
    let server = coordinator_tls(&farm);
    let state = host.flags.state_dir.clone();
    let args: Vec<String> = [
        "--coordinator".to_owned(),
        format!("{COORDINATOR}:8443"),
        "--enroll-frontend".to_owned(),
        "frontend.farm.example:8444".to_owned(),
        "--state-dir".to_owned(),
        state.display().to_string(),
        "--channel".to_owned(),
        "stable".to_owned(),
    ]
    .to_vec();
    let config = kbf_updater::agent::flags::parse_flags(&args).expect("the agent's flags");

    // The agent's nap says it has looked and waits for the test's word to look again.
    let (napped, looked) = channel::<()>();
    let (again, go) = channel::<()>();
    let lines = Arc::new(Mutex::new(Vec::<String>::new()));
    let said = Arc::clone(&lines);
    let shown = Arc::new(Mutex::new(Vec::<Result<Vec<u8>, String>>::new()));
    let seen = Arc::clone(&shown);
    let agent = tokio::task::spawn_blocking(move || {
        let log = |line: &str| said.lock().expect("log").push(line.to_owned());
        let nap = |_: &kbf_updater::agent::Stop| {
            napped.send(()).expect("the test listens");
            go.recv().expect("the test says when");
        };
        let make = move |cfg: &kbf_updater::agent::flags::Config| {
            let client = kbf_updater::agent::coordinator::client_config(&cfg.files)
                .map_err(|e| e.to_string())?;
            seen.lock()
                .expect("shown")
                .push(handshake_in_memory(client, server));
            Err(MADE.to_owned())
        };
        kbf_updater::agent::run_config_waiting(
            &config,
            |_| {},
            &kbf_updater::agent::enrolled::Waiting {
                nap: &nap,
                log: &log,
            },
            make,
        )
    });
    // Waits until the agent has looked once more, and hands the receiver back.
    let next_look = |looked: std::sync::mpsc::Receiver<()>| async move {
        tokio::task::spawn_blocking(move || {
            looked.recv().expect("the agent looked");
            looked
        })
        .await
        .expect("joined")
    };

    let looked = next_look(looked).await;
    assert_eq!(
        *lines.lock().expect("log"),
        [format!(
            "not enrolled: {0} holds no enrollment yet; waiting for `kbf-updater enroll \
             --state-dir {0}`, and starting once it has run",
            state.display()
        )]
    );
    assert!(shown.lock().expect("shown").is_empty(), "nothing is made");

    // A refused attempt: the token is for another node.
    let error = enroll(&host).await.expect_err("refused");
    assert!(
        error.contains(RefusalReason::NodeIdNotAllowed.message()),
        "{error}"
    );
    again.send(()).expect("the agent waits");
    let _looked = next_look(looked).await;
    assert_eq!(lines.lock().expect("log").len(), 1, "said once");
    assert!(shown.lock().expect("shown").is_empty(), "nothing is made");
    assert!(!host.pki("node.json").exists());

    // The operator runs it again with the token for this node.
    std::fs::write(host.root.join("enroll.token"), token.to_line()).expect("token file");
    enroll(&host).await.expect("enrolled");
    again.send(()).expect("the agent waits");
    let (code, text) = agent.await.expect("joined");
    assert_eq!((code, text), (1, format!("kbf-updater: {MADE}\n")));
    let shown = shown.lock().expect("shown").clone();
    assert_eq!(shown, [Ok(host.cert_der())], "the enrollment's certificate");
    assert_eq!(lines.lock().expect("log").len(), 1, "said once");
}
