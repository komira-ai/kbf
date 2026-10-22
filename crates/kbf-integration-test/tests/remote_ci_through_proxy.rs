//! A CI job's GitHub OIDC token against the real `kbf-proxy` in front of the real
//! `kbf-server`, over real sockets and real TLS: the token starts the desired release the way
//! the operator's token does, through the server's own handler and its own rows, and nothing
//! else.
//!
//! The server is the production one (`bind_server_with_ui`: its operator API with a token, a
//! release store, a state store of the fake, `Behind` the proxy's secret). The proxy has the
//! allow-list of a verifier over `kbf_oidc::testkit` (keys made in the test, an issuer that
//! opens no socket), or none. The server is told only the operator token: a call that the
//! OIDC token gets through is one the proxy put the operator token on.
//!
//! The tests, and the defect each one catches:
//! - `a_good_token_offers_a_release_as_the_operator_does`: the route not reaching the real
//!   handler (a stub, a 501), the OIDC token passed on to a gate that does not take it, a
//!   generation that moves differently from the operator's (repeat keeps it, a rollback
//!   outranks), the server's own refusal (a commit it does not hold, 409) lost on the way.
//! - `a_token_opens_nothing_but_the_two_calls`: the pause, the reads and the other writes
//!   of the update API open to a CI token, and a refused token still changing the row.
//! - `without_an_allow_list_the_token_is_nothing`: the credential on when nothing asked for it.

#![cfg(target_os = "linux")]

#[allow(
    dead_code,
    reason = "the proxy soaks share this module; each uses some of it"
)]
mod proxy_support;

use std::net::{Ipv4Addr, SocketAddr};
use std::os::fd::OwnedFd;
use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use kbf_auth::Policy;
use kbf_front::Cache;
use kbf_oidc::testkit::{STATIC_ISSUER, good_claims, keys, mint, static_verifier};
use kbf_proxy::control::NoUpdates;
use kbf_proxy::remote::RemoteCi;
use kbf_proxy::slot::Slot;
use kbf_proxy::{Proxy, ProxyHandle};
use kbf_server::members::MembershipConfig;
use kbf_server::proxy_auth::ProxySecret;
use kbf_server::release::{Limits, ReleaseStore};
use kbf_server::token::ApiToken;
use kbf_server::{Api, Behind, EpochPolicy, Listeners, StateStore, bind_server_with_ui};
use kbf_state::ManualClock;
use kbf_state::fake::{Fake, Flaw};
use kbf_testkit::dir::TestDir;
use proxy_support::{OPERATOR_TOKEN, free_base};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpSocket;

const OPEN: &str = r#"{"authenticationPolicy": {"allow": {}},
    "invocations": {"getAuthorizer": {"allow": {}}}, "blobs": {"getAuthorizer": {"allow": {}}}}"#;
const A: &str = "0123456789abcdef0123456789abcdef01234567";
const B: &str = "fedcba9876543210fedcba9876543210fedcba98";

struct Rig {
    handle: ProxyHandle,
    proxy: SocketAddr,
    ca: std::path::PathBuf,
    stop: tokio::sync::oneshot::Sender<()>,
    serving: tokio::task::JoinHandle<()>,
    _dir: TestDir,
}

fn private(path: &Path, content: &[u8]) {
    std::fs::write(path, content).expect("write");
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).expect("chmod");
}

fn loopback(port: u16) -> SocketAddr {
    SocketAddr::from((Ipv4Addr::LOCALHOST, port))
}

impl Rig {
    /// The pair behind one public port; `remote` says whether the proxy has an allow-list.
    async fn start(name: &str, remote: bool) -> Self {
        let dir = TestDir::under(
            env!("CARGO_TARGET_TMPDIR"),
            &format!("remote-ci-through-proxy-{name}"),
        );
        let base = free_base();
        let pki = kbf_integration_test::pki::write(&dir.path().join("pki"), "remote-ci")
            .expect("certificates");
        let operator = dir.path().join("operator.token");
        private(&operator, OPERATOR_TOKEN.as_bytes());
        let secret_file = dir.path().join("proxy-auth");
        let config = kbf_proxy::config::ProxyConfig::from_toml_str(&format!(
            "operator_token_file = {operator:?}\nserver_cert_file = {:?}\nserver_key_file = {:?}\n\
             client_ca_file = {:?}\nproxy_auth_file = {secret_file:?}\n\
             [upstream]\nbase_port = {base}\n",
            pki.server_cert, pki.server_key, pki.ca_cert,
        ))
        .expect("a valid table");
        let socket = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind");
        let proxy = socket.local_addr().expect("address");
        let gate = remote.then(|| RemoteCi::new(static_verifier().0));
        // The proxy builds its own runtime, which must not be built or dropped inside this one.
        let handle = tokio::task::spawn_blocking(move || {
            let (handle, runner) =
                Proxy::bind_with_remote(config, OwnedFd::from(socket), gate).expect("bind");
            std::thread::spawn(move || runner.run(NoUpdates));
            handle.slots().adopt(Slot::A, None);
            handle.slots().release_into(Slot::A).expect("A is open");
            handle
        })
        .await
        .expect("the proxy starts");
        let secret = ProxySecret::from_file(&secret_file).expect("the file the proxy wrote");

        let listeners = Listeners {
            reapi: loopback(base),
            worker: loopback(base + 1),
            reapi_tls: None,
            worker_tls: None,
            heartbeat_interval: Duration::from_millis(200),
            hello_wait: Duration::from_secs(5),
            tick: Duration::from_secs(3600),
            unservable_wait: kbf_sched::UNSERVABLE_WAIT,
            default_platform: kbf_front::DefaultPlatform::NONE,
            memory_ladder: kbf_server::MemoryLadder::default(),
            learned_sizes: kbf_server::LearnedSizes::default(),
            finished_retention: kbf_sched::FINISHED_RETENTION,
            shutdown_timeout: Duration::from_secs(10),
            streams: kbf_server::StreamTiming::default(),
            store_probe_timeout: kbf_server::health::STORE_PROBE_TIMEOUT,
            invocation_memory: kbf_front::DEFAULT_INVOCATION_MEMORY,
            segments: kbf_front::SegmentsConfig::DEFAULT,
            stream_limits: kbf_server::StreamLimits::DEFAULT,
        };
        let releases = Arc::new(
            ReleaseStore::open(&dir.path().join("releases"), Limits::default()).expect("open"),
        );
        let api = Api {
            listen: loopback(base + 2),
            token: Some(ApiToken::from_file(&operator).expect("a usable token")),
            local_cache: None,
            releases: Some(releases),
        };
        let state = StateStore {
            store: Arc::new(Fake::with_flaw(
                Arc::new(ManualClock::default()),
                Flaw::None,
            )),
            epoch: EpochPolicy::Keep,
            members: MembershipConfig {
                ttl: Duration::from_secs(30),
                dwell: Duration::from_secs(120),
                replication: 1,
            },
            history: None,
        };
        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        let bound = bind_server_with_ui(
            Arc::new(Cache::memory()),
            listeners,
            Some(api),
            Policy::from_json(OPEN).expect("a policy"),
            Some(state),
            None,
            None,
            Some(Behind {
                secret: Arc::new(secret),
                deny_list: None,
            }),
            false,
            async move {
                let _ = stopped.await;
            },
        )
        .expect("bind the server");
        let serving = tokio::spawn(async move { bound.serving.await.expect("serve") });
        Self {
            handle,
            proxy,
            ca: pki.ca_cert,
            stop,
            serving,
            _dir: dir,
        }
    }

    async fn stop(self) {
        self.handle.shutdown();
        self.stop.send(()).expect("the server is running");
        self.serving.await.expect("the server stops cleanly");
    }

    /// One HTTP/1.1 request through the proxy with `bearer`: the status and the JSON body.
    async fn call(
        &self,
        method: &str,
        path: &str,
        bearer: Option<&str>,
        body: &[u8],
    ) -> (u16, Value) {
        use rustls::pki_types::pem::PemObject as _;
        let mut roots = rustls::RootCertStore::empty();
        for der in rustls::pki_types::CertificateDer::pem_file_iter(&self.ca).expect("CA file") {
            roots.add(der.expect("CA PEM")).expect("root");
        }
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let mut config = rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .expect("versions")
            .with_root_certificates(roots)
            .with_no_client_auth();
        config.alpn_protocols = vec![b"http/1.1".to_vec()];
        let socket = TcpSocket::new_v4().expect("a socket");
        let tcp = socket.connect(self.proxy).await.expect("connect");
        let mut tls = tokio_rustls::TlsConnector::from(Arc::new(config))
            .connect(
                rustls::pki_types::ServerName::try_from("localhost").expect("name"),
                tcp,
            )
            .await
            .expect("TLS handshake");
        let auth = bearer
            .map(|t| format!("Authorization: Bearer {t}\r\n"))
            .unwrap_or_default();
        let head = format!(
            "{method} {path} HTTP/1.1\r\nHost: localhost\r\n{auth}Content-Type: application/json\r\n\
             Content-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        tls.write_all(head.as_bytes()).await.expect("write");
        tls.write_all(body).await.expect("write");
        let mut raw = Vec::new();
        // A read error after the data (no close_notify) still leaves what arrived in `raw`.
        let _ = tls.read_to_end(&mut raw).await;
        let text = String::from_utf8_lossy(&raw).into_owned();
        let status = text
            .split_whitespace()
            .nth(1)
            .and_then(|c| c.parse().ok())
            .unwrap_or_else(|| panic!("no status in {text:?}"));
        let body = text.split_once("\r\n\r\n").map_or("", |(_, b)| b);
        (status, serde_json::from_str(body).unwrap_or(Value::Null))
    }

    /// The operator stores `commit`'s tarball and attestation.
    async fn release(&self, commit: &str) {
        let path = format!("/v1/releases/{commit}/linux-x86_64/kbf");
        let put = self
            .call("PUT", &path, Some(OPERATOR_TOKEN), b"a tarball")
            .await;
        assert_eq!(put.0, 201, "{put:?}");
        let attestation = format!("{path}.attestation");
        let put = self
            .call("PUT", &attestation, Some(OPERATOR_TOKEN), b"{\"bundle\":1}")
            .await;
        assert_eq!(put.0, 201, "{put:?}");
    }

    /// `POST /v1/update/desired {commit}` with `bearer`.
    async fn offer(&self, commit: &str, bearer: &str) -> (u16, Value) {
        let body = json!({ "commit": commit }).to_string();
        self.call("POST", "/v1/update/desired", Some(bearer), body.as_bytes())
            .await
    }

    /// The desired row and the pause, as the operator reads them.
    async fn desired(&self) -> (Value, Value) {
        let (code, view) = self
            .call("GET", "/v1/update", Some(OPERATOR_TOKEN), b"")
            .await;
        assert_eq!(code, 200, "{view}");
        (view["desired"].clone(), view["paused"].clone())
    }
}

/// A token CI would send: fresh `jti` each time.
fn token() -> String {
    mint(&keys()[0], &good_claims(STATIC_ISSUER))
}

/// The token of a run that is not the allowed one.
fn wrong_audience() -> String {
    let mut claims = good_claims(STATIC_ISSUER);
    claims["aud"] = json!("someone-else");
    mint(&keys()[0], &claims)
}

/// A good token offers a release as the operator does: the generation is 1, a repeat keeps
/// it, and an older commit offered after a newer one outranks both; a commit the store does
/// not hold is the server's own 409. Operator and token calls are interleaved on one row.
///
/// Mutants planted: the audience check skipped in `kbf_oidc` (the wrong-audience call in the
/// next test changed the row); the OIDC token forwarded to the server instead of the
/// operator's (401 from the server's gate, here a red 401 on the first offer).
#[tokio::test]
async fn a_good_token_offers_a_release_as_the_operator_does() {
    let rig = Rig::start("offer", true).await;
    rig.release(A).await;
    rig.release(B).await;
    let missing = "ffffffffffffffffffffffffffffffffffffffff";
    let (code, body) = rig.offer(missing, &token()).await;
    assert_eq!(
        code, 409,
        "the server's own refusal reaches the caller: {body}"
    );
    assert_eq!(rig.desired().await.0, Value::Null);

    let (code, body) = rig.offer(A, &token()).await;
    assert_eq!(
        (code, &body["generation"], &body["commit"]),
        (200, &json!(1), &json!(A))
    );
    let (code, body) = rig.offer(A, &token()).await;
    assert_eq!(
        (code, &body["generation"]),
        (200, &json!(1)),
        "a repeat keeps it"
    );
    let (code, body) = rig.offer(B, OPERATOR_TOKEN).await;
    assert_eq!((code, &body["generation"]), (200, &json!(2)));
    let (code, body) = rig.offer(A, &token()).await;
    assert_eq!(
        (code, &body["generation"]),
        (200, &json!(3)),
        "a rollback outranks"
    );
    let (desired, paused) = rig.desired().await;
    assert_eq!(
        (&desired["commit"], &desired["generation"]),
        (&json!(A), &json!(3))
    );
    assert_eq!(paused, json!(false));
    rig.stop().await;
}

/// A token opens the two calls and nothing else: the pause, the reads, the other writes and
/// the other `/v1/update` paths are 401, and a refused token (wrong audience, replayed) does
/// not move the row.
#[tokio::test]
async fn a_token_opens_nothing_but_the_two_calls() {
    let rig = Rig::start("closed", true).await;
    rig.release(A).await;
    for (method, path) in [
        ("POST", "/v1/update/pause"),
        ("POST", "/v1/update/resume"),
        ("GET", "/v1/update"),
        ("GET", "/v1/nodes"),
        ("POST", "/v1/update/frontend/abort"),
        (
            "PUT",
            "/v1/releases/ffffffffffffffffffffffffffffffffffffffff/linux-x86_64/kbf",
        ),
    ] {
        let (code, body) = rig.call(method, path, Some(&token()), b"{}").await;
        assert_eq!(code, 401, "{method} {path}: {body}");
    }
    assert_eq!(rig.desired().await, (Value::Null, json!(false)));
    assert_eq!(rig.offer(A, &wrong_audience()).await.0, 401);
    assert_eq!(
        rig.desired().await.0,
        Value::Null,
        "a refused token moves nothing"
    );
    let once = token();
    assert_eq!(rig.offer(A, &once).await.0, 200);
    assert_eq!(rig.offer(B, &once).await.0, 401, "a token is good once");
    assert_eq!(rig.desired().await.0["commit"], json!(A));
    rig.stop().await;
}

/// With no allow-list a good token is nothing: the operator token is the only credential.
#[tokio::test]
async fn without_an_allow_list_the_token_is_nothing() {
    let rig = Rig::start("off", false).await;
    rig.release(A).await;
    assert_eq!(rig.offer(A, &token()).await.0, 401);
    assert_eq!(rig.desired().await.0, Value::Null);
    assert_eq!(rig.offer(A, OPERATOR_TOKEN).await.0, 200);
    rig.stop().await;
}
