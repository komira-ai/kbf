//! The registered cold store's address reaches a storage node and no log. Alone in its test
//! binary, with a process-wide subscriber on a current-thread runtime: a callsite that a
//! thread reached before any subscriber existed is cached as uninteresting, so a
//! per-thread subscriber installed late would miss it (the library's unit tests failed one
//! run in ten that way).

use std::os::unix::fs::PermissionsExt as _;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use kbf_enroll::client::{Enrollment, connect, enroll};
use kbf_enroll::config::{Config, Host};
use kbf_enroll::pin::Anchor;
use kbf_enroll::renew::renew;
use kbf_enroll::store::Store as NodeStore;
use kbf_enroll_registry::{TokenSpec, create_token};
use kbf_proto::kbf::enroll::v1::NodeFacts;
use kbf_server::enroll::OsRandom;
use kbf_server::enroll::ca::SignerChoice;
use kbf_server::enroll::flags::EnrollFlags;
use kbf_server::enroll::limits::Limits;
use kbf_server::enroll::service::NodeSettings;
use kbf_server::enroll::start::prepare;
use kbf_server::object_store::Registry;
use kbf_state::fake::{Fake, Flaw};
use kbf_state::{ManualClock, Store};
use kbf_types::object_store_api::{BindingPut, BindingRole, ConnectionPut, Secret};
use kbf_types::{Role, RoleSet, Token, TokenId, TokenSecret};

/// What the operator registered; each is distinctive, so a hit in a log is this store's.
const HOST: &str = "objects-zq7.test:9000";
const BUCKET: &str = "cold-bucket-zq7";
const KEY_ID: &str = "AKIDZQ7KEYID";
const SECRET: &str = "zq7-secret-access-key-value";

/// Log lines written by the test's subscriber.
#[derive(Clone, Default)]
struct Lines(Arc<Mutex<Vec<u8>>>);

impl Lines {
    fn text(&self) -> String {
        String::from_utf8(self.0.lock().expect("lock").clone()).expect("UTF-8")
    }
}

impl std::io::Write for Lines {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().expect("lock").extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

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
    let dir = std::env::temp_dir().join(format!("kbf-enroll-log-{name}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("scratch");
    std::fs::canonicalize(&dir).expect("canonical")
}

fn register_cold(store: &Arc<dyn Store>) {
    let registry = Registry::open(
        Arc::clone(store),
        &scratch("registry-secrets"),
        Arc::new(OsRandom),
    )
    .expect("registry");
    let name = "cold".parse().expect("name");
    registry
        .put_connection(
            &name,
            &ConnectionPut {
                endpoint: format!("http://{HOST}"),
                region: None,
                access_key_id: KEY_ID.to_owned(),
                secret_access_key: Secret::new(SECRET.to_owned()),
                conditional_put: None,
                min_put_rate: None,
                expect_generation: None,
                verify: None,
            },
        )
        .expect("connection");
    registry
        .put_binding(
            BindingRole::Cold,
            &BindingPut {
                connection: name,
                bucket: BUCKET.to_owned(),
                prefix: "cold/".to_owned(),
                expect_generation: None,
                verify: None,
            },
        )
        .expect("binding");
}

/// A storage node enrolls and renews while a store is registered: it is told the store, and
/// no log line at any level holds the endpoint, the bucket or the key pair.
///
/// Catches: a log call on the configuration path that quotes the URI (the endpoint is an
/// internal address, and an operator may have put more in it than the URI check sees).
#[tokio::test(flavor = "current_thread")]
async fn the_registered_cold_store_is_in_no_log_line() {
    let lines = Lines::default();
    let writer = lines.clone();
    tracing::subscriber::set_global_default(
        tracing_subscriber::fmt()
            .with_max_level(tracing::Level::TRACE)
            .with_writer(move || writer.clone())
            .with_ansi(false)
            .finish(),
    )
    .expect("the only subscriber of this process");

    let clock = Arc::new(ManualClock::default());
    clock.advance(Duration::from_secs(
        u64::try_from(unix_now()).expect("time"),
    ));
    let store: Arc<dyn Store> = Arc::new(Fake::with_flaw(Arc::clone(&clock), Flaw::None));
    let flags = EnrollFlags {
        listen: "127.0.0.1:0".parse().expect("addr"),
        ca_dir: scratch("farm").join("ca"),
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
    let (port, pin) = (prepared.local().port(), prepared.ca().pin());
    let mut serving = prepared.serve(Arc::clone(&store)).expect("serving");
    let listener = std::mem::replace(&mut serving.listener, Box::pin(std::future::pending()));
    tokio::spawn(listener);

    // Enrolled first, registered after: the same node then renews into the new store, so
    // the renew path builds a configuration with the store registered; a second node
    // enrolling after the registration runs the enroll path.
    let (first, frontend, first_store) = join(&store, port, pin, "node-02", 0x7a7).await;
    assert_eq!(first.config.cold_store, "");
    register_cold(&store);
    clock.advance(Duration::from_secs(120));
    let renewed = renew(&frontend, &first_store, unix_now() + 120)
        .await
        .expect("renewed");
    assert!(
        renewed.config.cold_store.contains("objects-zq7"),
        "the node was told"
    );
    let (second, _, _) = join(&store, port, pin, "node-03", 0x7a8).await;
    assert!(
        second.config.cold_store.contains("objects-zq7"),
        "the node was told"
    );
    drop(serving);

    let log = lines.text();
    for needle in [HOST, "objects-zq7", BUCKET, KEY_ID, SECRET] {
        assert!(!log.contains(needle), "{needle} is in the log:\n{log}");
    }
    assert!(!log.is_empty(), "the subscriber saw the run");
}

/// Enrolls storage node `node` with a fresh token and returns what its client installed.
async fn join(
    store: &Arc<dyn Store>,
    port: u16,
    pin: kbf_types::CaPin,
    node: &str,
    bits: u64,
) -> (
    kbf_enroll::store::Node,
    kbf_enroll::client::Frontend,
    NodeStore,
) {
    let id = TokenId::from_bits(bits);
    let secret = TokenSecret::from_bytes([7; 32]);
    create_token(
        &**store,
        &id,
        &secret,
        &TokenSpec {
            roles: RoleSet::new([Role::Storage]).expect("roles"),
            max_uses: 1,
            ttl: None,
            node_id: None,
            node_prefix: None,
            names: Vec::new(),
            replace: false,
            label: "log".to_owned(),
            created_by: "log".to_owned(),
        },
    )
    .expect("token");
    let token = Token::new(id, secret, pin);
    let root = scratch(node);
    let text = format!(
        "[frontend]\nurl = \"https://localhost:{port}\"\n[node]\nid = \"{node}\"\nadvertise = \"{node}.nodes.test:8983\"\n[enroll]\ntoken_file = \"{}\"\n[dirs]\nstate = \"{}\"\n",
        root.join("enroll.token").display(),
        root.join("state").display(),
    );
    let config = Config::parse(
        &text,
        &Host {
            hostname: node.to_owned(),
            cpus: 8,
        },
    )
    .expect("config");
    std::fs::write(&config.token_file, token.to_line()).expect("token file");
    std::fs::set_permissions(&config.token_file, std::fs::Permissions::from_mode(0o600))
        .expect("mode");
    let node_store = NodeStore::open(&config.state_dir).expect("store");
    let frontend = connect(&config.frontend, Anchor::Pin(*token.pin()))
        .await
        .expect("connect");
    let installed = enroll(
        &frontend,
        &node_store,
        &Enrollment {
            config: &config,
            token: &token,
            roles: RoleSet::new([Role::Storage]).expect("roles"),
            facts: NodeFacts {
                os: "linux".to_owned(),
                arch: "x86_64".to_owned(),
                commit: "abc".to_owned(),
                protocol: 1,
                package_version: "0.0.0".to_owned(),
            },
            now: unix_now(),
        },
    )
    .await
    .expect("enrolled");
    (installed, frontend, node_store)
}
