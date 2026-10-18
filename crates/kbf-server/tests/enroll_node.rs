//! The node's own client (`kbf-enroll`, the one `kbf node` runs) against the frontend's
//! real enroll listener: TLS with the frontend certificate from the CA, pinned through the
//! token, `Enroll`, `Renew`, and the refusals as the node reads them. Each test says what
//! it proves and the defect it would catch.
//!
//! The registry is the in-memory state store (`kbf-state`'s fake), whose clock the test
//! moves; the CA is made by the listener's own start in a scratch directory.

use std::os::unix::fs::PermissionsExt as _;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use kbf_enroll::client::{CallError, Enrollment, Frontend, connect, enroll};
use kbf_enroll::config::{Config, Host};
use kbf_enroll::pin::Anchor;
use kbf_enroll::renew::{RenewError, renew};
use kbf_enroll::store::Store as NodeStore;
use kbf_enroll_registry::{TokenSpec, create_token, list_nodes, list_tokens, revoke_node};
use kbf_proto::kbf::enroll::v1::{NodeFacts, RoleCsr};
use kbf_server::enroll::ca::SignerChoice;
use kbf_server::enroll::flags::EnrollFlags;
use kbf_server::enroll::limits::Limits;
use kbf_server::enroll::service::NodeSettings;
use kbf_server::enroll::start::prepare;
use kbf_state::fake::{Fake, Flaw};
use kbf_state::{ManualClock, Store};
use kbf_types::{CaPin, RefusalReason, Role, RoleSet, Token, TokenId, TokenSecret};

fn unix_now() -> i64 {
    i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("time")
            .as_secs(),
    )
    .expect("time")
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("kbf-enroll-e2e-{name}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("scratch");
    std::fs::canonicalize(&dir).expect("canonical")
}

/// A running enroll listener over a fresh store and CA.
struct Farm {
    clock: Arc<ManualClock>,
    store: Arc<dyn Store>,
    port: u16,
    pin: CaPin,
    ca_dir: PathBuf,
    _latch: kbf_server::enroll::start::Serving,
}

fn flags(ca_dir: PathBuf, limits: Limits, allow_unenforced_deny: bool) -> EnrollFlags {
    EnrollFlags {
        listen: "127.0.0.1:0".parse().expect("addr"),
        ca_dir,
        signer: SignerChoice::DevFile,
        frontend_names: vec!["localhost".to_owned()],
        cert_lifetime_secs: 86_400,
        limits,
        allow_unenforced_deny,
        settings: NodeSettings {
            worker_endpoint: "https://localhost:7443".to_owned(),
            cas_endpoint: String::new(),
            storage_port: 8983,
        },
    }
}

async fn start(name: &str, limits: Limits) -> Farm {
    let clock = Arc::new(ManualClock::default());
    clock.advance(Duration::from_secs(
        u64::try_from(unix_now()).expect("time"),
    ));
    let store: Arc<dyn Store> = Arc::new(Fake::with_flaw(Arc::clone(&clock), Flaw::None));
    let ca_dir = scratch(name).join("ca");
    let flags = flags(ca_dir.clone(), limits, true);
    let prepared = prepare(&flags).expect("prepared");
    let (port, pin) = (prepared.local().port(), prepared.ca().pin());
    let mut serving = prepared.serve(Arc::clone(&store)).expect("serving");
    let listener = std::mem::replace(&mut serving.listener, Box::pin(std::future::pending()));
    tokio::spawn(listener);
    Farm {
        clock,
        store,
        port,
        pin,
        ca_dir,
        _latch: serving,
    }
}

impl Farm {
    fn token(&self, bits: u64, roles: &[Role], uses: u32) -> Token {
        let id = TokenId::from_bits(bits);
        #[allow(clippy::cast_possible_truncation)]
        let secret = TokenSecret::from_bytes([bits as u8; 32]);
        create_token(
            &*self.store,
            &id,
            &secret,
            &TokenSpec {
                roles: RoleSet::new(roles.iter().copied()).expect("roles"),
                max_uses: uses,
                ttl: None,
                node_id: None,
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

    fn advance(&self, secs: u64) {
        self.clock.advance(Duration::from_secs(secs));
    }
}

struct Node {
    config: Config,
    store: NodeStore,
}

/// A node's files. A storage node says where it can be reached; a node with no storage
/// role must not (the registry refuses an advertise address it has no use for).
fn node(name: &str, farm: &Farm, token: &Token, storage: bool) -> Node {
    let advertise = if storage {
        "advertise = \"node-02.nodes.test:8983\"\n"
    } else {
        ""
    };
    let root = scratch(name);
    let text = format!(
        "[frontend]\nurl = \"https://localhost:{}\"\n[node]\nid = \"node-02\"\n{advertise}[enroll]\ntoken_file = \"{}\"\n[dirs]\nstate = \"{}\"\n",
        farm.port,
        root.join("enroll.token").display(),
        root.join("state").display(),
    );
    let config = Config::parse(
        &text,
        &Host {
            hostname: "node-02".to_owned(),
            cpus: 8,
        },
    )
    .unwrap_or_else(|e| panic!("config: {e}"));
    std::fs::write(&config.token_file, token.to_line()).expect("token file");
    std::fs::set_permissions(&config.token_file, std::fs::Permissions::from_mode(0o600))
        .expect("mode");
    let store = NodeStore::open(&config.state_dir).expect("store");
    Node { config, store }
}

fn facts() -> NodeFacts {
    NodeFacts {
        os: "linux".to_owned(),
        arch: "x86_64".to_owned(),
        commit: "abc".to_owned(),
        protocol: 1,
        package_version: "0.0.0".to_owned(),
    }
}

async fn dial(node: &Node, token: &Token) -> Frontend {
    connect(&node.config.frontend, Anchor::Pin(*token.pin()))
        .await
        .expect("connect")
}

async fn enroll_with(
    node: &Node,
    token: &Token,
    roles: &[Role],
) -> Result<kbf_enroll::store::Node, kbf_enroll::client::EnrollError> {
    let frontend = dial(node, token).await;
    enroll(
        &frontend,
        &node.store,
        &Enrollment {
            config: &node.config,
            token,
            roles: RoleSet::new(roles.iter().copied()).expect("roles"),
            facts: facts(),
            now: unix_now(),
        },
    )
    .await
}

fn refused(error: &kbf_enroll::client::EnrollError) -> Option<RefusalReason> {
    match error {
        kbf_enroll::client::EnrollError::Call(CallError::Refused(reason)) => Some(*reason),
        _ => None,
    }
}

/// A node enrolls over TLS with the CA pinned through its token, the answer passes the
/// node's own checks (the chain, the keys, the bundle), and everything is installed.
///
/// Catches: a listener whose certificate the pin does not accept (not the CA's, not for
/// the name, not the frontend role), an answer the node's client refuses (a bundle
/// without the root, a certificate for another key), and a missing service on the socket.
#[tokio::test(flavor = "multi_thread")]
async fn a_node_enrolls_over_tls_and_its_client_accepts_the_answer() {
    let farm = start("enroll", Limits::DEFAULT).await;
    let token = farm.token(0x1234, &[Role::Storage, Role::Daemon], 1);
    let node = node("enroll-node", &farm, &token, true);
    let installed = enroll_with(&node, &token, &[Role::Storage, Role::Daemon])
        .await
        .expect("enrolled");
    assert_eq!(installed.node_id, "node-02");
    assert_eq!(installed.roles(), vec![Role::Storage, Role::Daemon]);
    assert_eq!(installed.pin().expect("pin"), farm.pin);
    assert_eq!(installed.config.storage_port, 8983);
    assert_eq!(installed.config.worker_endpoint, "https://localhost:7443");
    assert!(node.store.load().expect("load").is_some());

    let nodes = list_nodes(&*farm.store).expect("nodes");
    assert_eq!(nodes.len(), 1);
    assert_eq!(nodes[0].advertise, "node-02.nodes.test:8983");
    assert_eq!(list_tokens(&*farm.store).expect("tokens")[0].uses, 1);
    assert!(farm.ca_dir.join("ca.pem").exists() && farm.ca_dir.join("ca.key").exists());
}

/// The same node renews with its registered key, over the same listener, and its client
/// installs the new certificates; a node that was revoked is then refused, on the wire,
/// with exactly `NodeRevoked`.
///
/// Catches: a renewal path that works in the service and not through the listener (the
/// guard, the TLS), a renewed pair the node's client refuses, and revocation that does
/// not reach a node that keeps asking.
#[tokio::test(flavor = "multi_thread")]
async fn a_node_renews_and_a_revoked_node_is_refused_node_revoked() {
    let farm = start("renew", Limits::DEFAULT).await;
    let token = farm.token(0x2222, &[Role::Storage, Role::Daemon], 1);
    let node = node("renew-node", &farm, &token, true);
    let first = enroll_with(&node, &token, &[Role::Storage, Role::Daemon])
        .await
        .expect("enrolled");
    let frontend = dial(&node, &token).await;

    farm.advance(120);
    let renewed = renew(&frontend, &node.store, unix_now() + 120)
        .await
        .expect("renewed");
    assert_eq!(renewed.roles(), first.roles());
    assert_ne!(
        renewed.certs[0].serial, first.certs[0].serial,
        "new certificates"
    );
    assert!(renewed.earliest_not_after() > first.earliest_not_after());

    revoke_node(&*farm.store, &"node-02".parse().expect("id"), "e2e").expect("revoked");
    farm.advance(120);
    let error = renew(&frontend, &node.store, unix_now() + 240)
        .await
        .expect_err("revoked");
    assert!(
        matches!(
            error,
            RenewError::Call(CallError::Refused(RefusalReason::NodeRevoked))
        ),
        "{error:?}"
    );
}

/// Refusals reach the node's client as exactly the reason: the client recognises a refusal
/// only by its code and message together, so `Refused(reason)` proves both. A wrong secret
/// is `TokenRefused`; a role outside the token, `RoleNotGranted`; a used-up token,
/// `TokenUsedUp`.
///
/// Catches: a status with the right code and a free-text message (the client would show
/// `Status`, not the reason, and decide retries wrongly).
#[tokio::test(flavor = "multi_thread")]
async fn refusals_reach_the_client_as_exactly_the_reason() {
    let farm = start("refusals", Limits::DEFAULT).await;
    let token = farm.token(0x3333, &[Role::Daemon], 1);

    let wrong = Token::new(
        token.id().clone(),
        TokenSecret::from_bytes([0xee; 32]),
        farm.pin,
    );
    let node_a = node("refusals-a", &farm, &wrong, false);
    let error = enroll_with(&node_a, &wrong, &[Role::Daemon])
        .await
        .expect_err("refused");
    assert_eq!(
        refused(&error),
        Some(RefusalReason::TokenRefused),
        "{error:?}"
    );

    let node_b = node("refusals-b", &farm, &token, true);
    let error = enroll_with(&node_b, &token, &[Role::Storage])
        .await
        .expect_err("refused");
    assert_eq!(
        refused(&error),
        Some(RefusalReason::RoleNotGranted),
        "{error:?}"
    );

    let node_d = node("refusals-d", &farm, &token, false);
    enroll_with(&node_d, &token, &[Role::Daemon])
        .await
        .expect("enrolled");
    let node_c = node("refusals-c", &farm, &token, false);
    let error = enroll_with(&node_c, &token, &[Role::Daemon])
        .await
        .expect_err("refused");
    assert_eq!(
        refused(&error),
        Some(RefusalReason::TokenUsedUp),
        "{error:?}"
    );
}

/// A node that pins another CA does not talk to this frontend.
///
/// Catches: a listener certificate not tied to the CA (any certificate for `localhost`
/// would then do for a man in the middle, and the token secret would be sent to it).
#[tokio::test(flavor = "multi_thread")]
async fn a_node_that_pins_another_ca_refuses_the_listener() {
    let farm = start("pin", Limits::DEFAULT).await;
    let token = farm.token(0x4444, &[Role::Daemon], 1);
    let node = node("pin-node", &farm, &token, false);
    let other = CaPin::from_bytes([7; 32]);
    let error = connect(&node.config.frontend, Anchor::Pin(other))
        .await
        .expect_err("untrusted");
    assert!(
        matches!(error, kbf_enroll::client::ConnectError::Untrusted(_)),
        "{error:?}"
    );
}

/// A request over 16 KiB is `MalformedRequest`, whatever is in it, while the same request
/// at a size under the cap is judged on its content (`BadCsr` for a 3 KiB CSR).
///
/// Catches: a cap that is not on the listener (the oversized body would be read and
/// judged `BadCsr`), and a cap so low it refuses requests the CA would read.
#[tokio::test(flavor = "multi_thread")]
async fn a_request_over_sixteen_kib_is_malformed_and_one_under_it_is_read() {
    let farm = start("cap", Limits::DEFAULT).await;
    let token = farm.token(0x5555, &[Role::Daemon], 5);
    let node = node("cap-node", &farm, &token, false);
    let frontend = dial(&node, &token).await;
    let request = |csr_len: usize| kbf_proto::kbf::enroll::v1::EnrollRequest {
        token_id: token.id().to_string(),
        token_secret: token.secret().as_bytes().to_vec(),
        node_id: "node-02".to_owned(),
        csrs: vec![RoleCsr {
            role: "daemon".to_owned(),
            csr_der: vec![0x30; csr_len],
        }],
        ..Default::default()
    };
    let under = frontend
        .enroll(request(3 * 1024))
        .await
        .expect_err("refused");
    assert!(
        matches!(under, CallError::Refused(RefusalReason::BadCsr)),
        "{under:?}"
    );
    let over = frontend
        .enroll(request(17 * 1024))
        .await
        .expect_err("refused");
    assert!(
        matches!(over, CallError::Refused(RefusalReason::MalformedRequest)),
        "{over:?}"
    );
}

/// A source over its burst is `RateLimited` on the wire, and the limit does not spend the
/// token (the refused calls never reach the registry).
///
/// Catches: a guard that is built but not on the server, and a refusal in any form but
/// the closed set.
#[tokio::test(flavor = "multi_thread")]
async fn a_source_over_its_burst_is_rate_limited() {
    let farm = start(
        "rate",
        Limits {
            per_minute: 1,
            burst: 3,
            in_flight: 8,
        },
    )
    .await;
    let token = farm.token(0x6666, &[Role::Daemon], 1);
    let node = node("rate-node", &farm, &token, false);
    let frontend = dial(&node, &token).await;
    let guess = kbf_proto::kbf::enroll::v1::EnrollRequest {
        token_id: token.id().to_string(),
        token_secret: vec![0; 32],
        node_id: "node-02".to_owned(),
        csrs: vec![RoleCsr {
            role: "daemon".to_owned(),
            csr_der: vec![0x30; 100],
        }],
        ..Default::default()
    };
    let mut seen = Vec::new();
    for _ in 0..5 {
        seen.push(frontend.enroll(guess.clone()).await.expect_err("refused"));
    }
    let reasons: Vec<RefusalReason> = seen
        .iter()
        .map(|e| match e {
            CallError::Refused(reason) => *reason,
            CallError::Status(status) => panic!("{status:?}"),
        })
        .collect();
    assert_eq!(
        reasons,
        [
            RefusalReason::TokenRefused,
            RefusalReason::TokenRefused,
            RefusalReason::TokenRefused,
            RefusalReason::RateLimited,
            RefusalReason::RateLimited
        ]
    );
    assert_eq!(list_tokens(&*farm.store).expect("tokens")[0].uses, 0);
}

/// Without `--enroll-allow-unenforced-deny` the start is refused while this build does not
/// enforce deny rows, and nothing is made: no CA directory, no key.
///
/// Catches: a listener switched on by mistake that promises a revocation storage does not
/// enforce, and a gate checked after the CA was already written.
#[tokio::test(flavor = "multi_thread")]
async fn enroll_listen_without_the_deny_acceptance_is_refused_and_writes_nothing() {
    let ca_dir = scratch("gate").join("ca");
    let error = prepare(&flags(ca_dir.clone(), Limits::DEFAULT, false))
        .err()
        .expect("refused");
    let text = error.to_string();
    assert!(text.contains("--enroll-allow-unenforced-deny"), "{text}");
    assert!(!ca_dir.exists(), "the CA directory was not made");
}

/// A second start on the same `--ca-dir` loads the CA the first made: the same pin, the
/// same files byte for byte.
///
/// Catches: a restart that makes a new CA over the old one, which would strand every node
/// (their pins and certificates are the old CA's).
#[tokio::test(flavor = "multi_thread")]
async fn a_restart_loads_the_ca_it_made_and_does_not_overwrite_it() {
    let ca_dir = scratch("restart").join("ca");
    let first = prepare(&flags(ca_dir.clone(), Limits::DEFAULT, true)).expect("first");
    let pin = first.ca().pin();
    let files = |name: &str| std::fs::read(ca_dir.join(name)).expect("file");
    let (key, root) = (files("ca.key"), files("ca.pem"));
    drop(first);
    let second = prepare(&flags(ca_dir.clone(), Limits::DEFAULT, true)).expect("second");
    assert_eq!(second.ca().pin(), pin);
    assert_eq!(files("ca.key"), key);
    assert_eq!(files("ca.pem"), root);
}

/// With `--ca-signer` the frontend starts from the signer's public root alone: it
/// serves under the signer's pin and holds no key.
///
/// Catches: a production start that makes a CA key of its own when told the key lives
/// elsewhere (the pin tokens carry would then be a different CA's), and one that cannot
/// start against a signer at all.
#[tokio::test(flavor = "multi_thread")]
async fn an_external_signer_start_uses_the_signers_root_and_makes_no_key() {
    use kbf_ca::{Policy, SystemEntropy};
    use kbf_signer::{SignerServer, bind, ensure};

    let signer_dir = scratch("external-signer");
    std::fs::set_permissions(&signer_dir, std::fs::Permissions::from_mode(0o700)).expect("mode");
    let policy = Policy::new(86_400).expect("policy");
    let (authority, _) = ensure(&signer_dir, policy, unix_now(), &mut SystemEntropy).expect("CA");
    let server = SignerServer::from_dir(&signer_dir, rustix::process::geteuid().as_raw(), policy)
        .expect("server");
    let socket_dir = scratch("external-signer-sock");
    std::fs::set_permissions(&socket_dir, std::fs::Permissions::from_mode(0o700)).expect("mode");
    let socket = socket_dir.join("signer.sock");
    let listener = bind(&socket).expect("bind");
    std::thread::spawn(move || {
        let _ = server.serve(&listener);
    });

    let ca_dir = scratch("external-frontend").join("ca");
    std::fs::create_dir_all(&ca_dir).expect("dir");
    std::fs::copy(signer_dir.join("ca.pem"), ca_dir.join("ca.pem")).expect("root");
    let mut f = flags(ca_dir.clone(), Limits::DEFAULT, true);
    f.signer = SignerChoice::External(socket);
    let prepared = tokio::task::spawn_blocking(move || prepare(&f).map_err(|e| e.to_string()))
        .await
        .expect("join")
        .expect("prepared against the signer");
    assert_eq!(prepared.ca().pin(), authority.pin());
    assert!(!ca_dir.join("ca.key").exists(), "the frontend holds no key");
}
