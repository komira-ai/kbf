//! A node revoked through the enroll API is refused on the worker listener, end to end:
//! a frontend over a real SQLite state store with the enroll listener and worker TLS,
//! daemons holding the certificates the enroll listener issued, and the revoke made
//! through the operator API.

use std::net::SocketAddr;
use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
use std::time::Duration;

use futures::channel::mpsc::{UnboundedSender, unbounded};
use kbf_ca::{Policy, SystemEntropy};
use kbf_enroll::client::{Enrollment, connect, enroll};
use kbf_enroll::config::{Config, Host};
use kbf_enroll::pin::Anchor;
use kbf_enroll::store::Store as NodeStore;
use kbf_proto::kbf::enroll::v1::NodeFacts;
use kbf_proto::worker::{
    Capability, DaemonMessage, Heartbeat, Hello, ServerMessage, daemon_message, server_message,
    worker_client::WorkerClient,
};
use kbf_server::api_client::OperatorClient;
use kbf_server::enroll::api::TOKENS;
use kbf_server::enroll::ca::{Ca, SignerChoice};
use kbf_server::frontend::{self, FrontendFlags, start_frontend};
use kbf_types::enroll_api::{NodeRevoked, TokenCreated};
use kbf_types::{Role, RoleSet, Token};
use rcgen::{
    BasicConstraints, CertificateParams, CertifiedIssuer, DnType, ExtendedKeyUsagePurpose, IsCa,
    KeyPair, KeyUsagePurpose,
};
use tokio::time::timeout;
use tonic::transport::{Certificate, Channel, ClientTlsConfig, Endpoint, Identity};
use tonic::{Code, Status, Streaming};

const API_TOKEN: &str = "kbf-test-token-0123456789abcdef0123456789";
const PROMPT: Duration = Duration::from_secs(10);

fn unix_now() -> i64 {
    i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("time")
            .as_secs(),
    )
    .expect("time")
}

/// A frontend serving, and the way to the things a test reaches it by.
struct Farm {
    dir: kbf_testkit::dir::TestDir,
    api: SocketAddr,
    enroll: SocketAddr,
    worker: SocketAddr,
    server_ca: String,
    _stop: tokio::sync::oneshot::Sender<()>,
}

impl Farm {
    async fn start(name: &str, extra: &[&str]) -> Self {
        let dir = kbf_testkit::dir::TestDir::under(
            env!("CARGO_TARGET_TMPDIR"),
            &format!("kbf-worker-revocation-{name}"),
        );
        // The enroll CA first: the worker listener trusts it, and reads it at start.
        let ca_dir = dir.join("ca");
        let policy = Policy::new(604_800).expect("policy");
        Ca::open(
            &ca_dir,
            &SignerChoice::DevFile,
            policy,
            unix_now(),
            &mut SystemEntropy,
        )
        .expect("the CA");
        // The worker listener's own certificate, from a CA the daemons trust for it.
        let mut ca = CertificateParams::new(Vec::<String>::new()).expect("CA params");
        ca.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        ca.distinguished_name.push(DnType::CommonName, "server CA");
        ca.key_usages = vec![
            KeyUsagePurpose::KeyCertSign,
            KeyUsagePurpose::DigitalSignature,
        ];
        let ca = CertifiedIssuer::self_signed(ca, KeyPair::generate().expect("key")).expect("CA");
        let mut params = CertificateParams::new(vec!["localhost".to_owned()]).expect("params");
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        let key = KeyPair::generate().expect("key");
        let cert = params.signed_by(&key, &ca).expect("sign");
        std::fs::write(dir.join("server.pem"), cert.pem()).expect("cert");
        std::fs::write(dir.join("server.key"), key.serialize_pem()).expect("key");
        let token_file = dir.join("api.token");
        std::fs::write(&token_file, format!("{API_TOKEN}\n")).expect("token file");
        std::fs::set_permissions(&token_file, std::fs::Permissions::from_mode(0o600))
            .expect("mode");

        let mut argv: Vec<String> = [
            "frontend",
            "--storage=http://127.0.0.1:1",
            "--listen=127.0.0.1:0",
            "--worker-listen=127.0.0.1:0",
            "--api-listen=127.0.0.1:0",
            "--enroll-listen=127.0.0.1:0",
            "--ca-dev-file-signer",
            "--frontend-name=localhost",
            "--memory-ladder-gib=1",
            "--drain-lb-wait=0",
        ]
        .map(str::to_owned)
        .to_vec();
        argv.extend([
            format!("--api-token-file={}", token_file.display()),
            format!("--state-store=sqlite://{}", dir.join("state.db").display()),
            format!("--ca-dir={}", ca_dir.display()),
            format!("--worker-tls-cert={}", dir.join("server.pem").display()),
            format!("--worker-tls-key={}", dir.join("server.key").display()),
            format!("--worker-client-ca={}", ca_dir.join("ca.pem").display()),
        ]);
        argv.extend(extra.iter().map(|a| (*a).to_owned()));
        let flags: FrontendFlags = frontend::parse(
            &frontend::command()
                .try_get_matches_from(argv)
                .expect("argv"),
        )
        .expect("flags");
        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        let handle = start_frontend(&flags, || {
            Ok(async move {
                let _ = stopped.await;
            })
        })
        .expect("starts");
        let farm = Self {
            api: handle.api.expect("the operator API"),
            enroll: handle.enroll.expect("the enroll listener"),
            worker: handle.worker,
            server_ca: ca.pem(),
            dir,
            _stop: stop,
        };
        tokio::spawn(handle.serve());
        farm
    }

    /// One operator API call, off the async threads (the client blocks).
    async fn call(&self, method: &'static str, path: String, body: &'static str) -> (u16, String) {
        let client = OperatorClient::new(self.api, Some(API_TOKEN.to_owned()));
        tokio::task::spawn_blocking(move || {
            let reply = match method {
                "POST" => client.post(&path, body),
                _ => client.delete(&path),
            }
            .expect("the API answers");
            (reply.status, reply.body)
        })
        .await
        .expect("join")
    }

    /// Enrolls `node` as a daemon through the enroll listener; returns its identity.
    async fn enrolled(&self, node: &str) -> Identity {
        let (status, body) = self
            .call(
                "POST",
                TOKENS.to_owned(),
                r#"{"roles":["daemon"],"max_uses":1}"#,
            )
            .await;
        assert_eq!(status, 200, "{body}");
        let token: Token = serde_json::from_str::<TokenCreated>(&body)
            .expect("a token")
            .token
            .parse()
            .expect("a token");
        let root = self.dir.join(node);
        let text = format!(
            "[frontend]\nurl = \"https://localhost:{}\"\n[node]\nid = \"{node}\"\n[enroll]\ntoken_file = \"{}\"\n[dirs]\nstate = \"{}\"\n",
            self.enroll.port(),
            root.join("enroll.token").display(),
            root.join("state").display(),
        );
        let config = Config::parse(
            &text,
            &Host {
                hostname: node.to_owned(),
                cpus: 4,
            },
        )
        .unwrap_or_else(|e| panic!("config: {e}"));
        std::fs::create_dir_all(&root).expect("node dir");
        std::fs::write(&config.token_file, token.to_line()).expect("token file");
        std::fs::set_permissions(&config.token_file, std::fs::Permissions::from_mode(0o600))
            .expect("mode");
        let store = NodeStore::open(&config.state_dir).expect("store");
        let frontend = connect(&config.frontend, Anchor::Pin(*token.pin()))
            .await
            .expect("connected");
        enroll(
            &frontend,
            &store,
            &Enrollment {
                config: &config,
                token: &token,
                roles: RoleSet::new([Role::Daemon]).expect("roles"),
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
        identity_of(
            &store.cert_path(Role::Daemon),
            &store.key_path(Role::Daemon),
        )
    }

    async fn channel(&self, identity: &Identity) -> Channel {
        let tls = ClientTlsConfig::new()
            .ca_certificate(Certificate::from_pem(&self.server_ca))
            .domain_name("localhost")
            .identity(identity.clone());
        Endpoint::from_shared(format!("https://{}", self.worker))
            .expect("endpoint")
            .tls_config(tls)
            .expect("tls")
            .connect()
            .await
            .expect("connect")
    }

    async fn session(&self, identity: &Identity, node: &str) -> Result<Session, Status> {
        let (tx, rx) = unbounded();
        send(&tx, hello(node));
        let mut inbound = WorkerClient::new(self.channel(identity).await)
            .session(rx)
            .await?
            .into_inner();
        match timeout(PROMPT, inbound.message())
            .await
            .expect("an answer in time")?
        {
            Some(ServerMessage {
                message: Some(server_message::Message::Welcome(_)),
            }) => Ok(Session { tx, inbound }),
            other => panic!("expected Welcome or an error, got {other:?}"),
        }
    }
}

fn identity_of(cert: &Path, key: &Path) -> Identity {
    Identity::from_pem(
        std::fs::read(cert).expect("the node's certificate"),
        std::fs::read(key).expect("the node's key"),
    )
}

fn hello(node: &str) -> daemon_message::Message {
    let entry = |key: &str, value: &str| Capability {
        key: key.to_owned(),
        value: value.to_owned(),
    };
    daemon_message::Message::Hello(Hello {
        protocol_version: 1,
        node_id: node.to_owned(),
        capabilities: vec![
            entry("os", "linux"),
            entry("arch", "x86_64"),
            entry("cpus", "1"),
            entry("mem_gib", "1"),
            entry("drivers", "fake"),
        ],
        ..Hello::default()
    })
}

fn send(tx: &UnboundedSender<DaemonMessage>, message: daemon_message::Message) {
    tx.unbounded_send(DaemonMessage {
        message: Some(message),
    })
    .expect("the stream is open");
}

/// A daemon's open session.
struct Session {
    tx: UnboundedSender<DaemonMessage>,
    inbound: Streaming<ServerMessage>,
}

impl Session {
    async fn heartbeat(&mut self, seq: u64) -> Result<Option<ServerMessage>, Status> {
        send(
            &self.tx,
            daemon_message::Message::Heartbeat(Heartbeat {
                seq,
                ..Heartbeat::default()
            }),
        );
        timeout(PROMPT, self.inbound.message())
            .await
            .expect("an answer or an end in time")
    }
}

fn refused(opened: Result<Session, Status>) -> Status {
    match opened {
        Ok(_) => panic!("the session was welcomed"),
        Err(status) => status,
    }
}

/// A revoke made through the enroll API ends the node's open stream at its next heartbeat,
/// refuses its next `Hello` PERMISSION_DENIED, and leaves a node that
/// was not revoked admitted. The flag `--enroll-allow-unenforced-deny` is not given: the
/// enroll listener starts without it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_revoked_node_is_refused_on_the_worker_listener() {
    let farm = Farm::start("revoke", &[]).await;
    let revoked = farm.enrolled("node-07").await;
    let other = farm.enrolled("node-08").await;

    let mut open = farm
        .session(&revoked, "node-07")
        .await
        .expect("admitted before the revoke");
    assert!(matches!(open.heartbeat(1).await, Ok(Some(_))));

    let (status, body) = farm
        .call("POST", "/v1/enroll/nodes/node-07/revoke".to_owned(), "")
        .await;
    assert_eq!(status, 200, "{body}");

    let ended = open
        .heartbeat(2)
        .await
        .expect_err("the stream of the revoked node ended");
    assert_eq!(ended.code(), Code::PermissionDenied, "{ended:?}");
    let again = refused(farm.session(&revoked, "node-07").await);
    assert_eq!(again.code(), Code::PermissionDenied, "{again:?}");

    let mut fine = farm.session(&other, "node-08").await.expect("not revoked");
    assert!(matches!(fine.heartbeat(1).await, Ok(Some(_))));
}

/// Taking the rows a revoke wrote off the deny list (`DELETE /v1/denied`) readmits the
/// node: its next `Hello` is welcomed again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn taking_a_revoked_node_off_the_deny_list_readmits_it() {
    let farm = Farm::start("readmit", &["--enroll-allow-unenforced-deny"]).await;
    let node = farm.enrolled("node-07").await;
    let (status, body) = farm
        .call("POST", "/v1/enroll/nodes/node-07/revoke".to_owned(), "")
        .await;
    assert_eq!(status, 200, "{body}");
    let revoked: NodeRevoked = serde_json::from_str(&body).expect("an answer");
    assert!(!revoked.denied.is_empty());
    let denied = refused(farm.session(&node, "node-07").await);
    assert_eq!(denied.code(), Code::PermissionDenied, "{denied:?}");

    for entry in &revoked.denied {
        let query = entry.replace(' ', "%20");
        let (status, body) = farm
            .call("DELETE", format!("/v1/denied?entry={query}"), "")
            .await;
        assert_eq!(status, 200, "{entry}: {body}");
    }
    let session = farm.session(&node, "node-07").await;
    assert!(session.is_ok(), "readmitted: {:?}", session.err());
}
