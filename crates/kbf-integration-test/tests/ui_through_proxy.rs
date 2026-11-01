//! The open web UI, black box: the real `kbf-proxy` in front of the real `kbf-server`, over
//! real sockets and real TLS, and nothing but the proxy's one public port to talk to
//! (`docs/design/kbf-ui-v1.md`: open, read-only, on by default, and opening nothing else).
//!
//! The server is the production one (`bind_server_with_ui`, its operator API, REAPI and
//! worker listeners on the ports of the proxy's member, and `Behind` the proxy's secret file, so a
//! request's client address is believed only when the proxy forwarded it). The proxy runs
//! `buck2_auth = "bearer"` so that REAPI has a credential to refuse. The browser is a bare
//! TLS client with no certificate; the operator and the Buck2 client use their bearer
//! tokens.
//!
//! The tests, and the defect each one catches:
//! - `the_ui_answers_without_credentials_with_the_security_headers`: `/ui` behind a
//!   credential, a page without the CSP (or with it twice, or with `no-store` lost).
//! - `a_write_to_the_ui_is_a_405`: a method other than `GET` and `HEAD` reaching a page.
//! - `no_other_route_opens_without_credentials`: the open class leaking to a `/v1` read or
//!   write, `/metrics`, the update API, or REAPI and the worker listener.
//! - `an_uploaded_html_blob_comes_back_as_an_attachment`: an uploaded page served as a page.
//! - `two_browsers_each_have_their_own_read_bound`: every browser behind the proxy sharing
//!   the proxy's address as one caller.

#![cfg(target_os = "linux")]

#[allow(
    dead_code,
    reason = "the proxy soaks share this module; each uses some of it"
)]
mod proxy_support;

use std::net::{Ipv4Addr, SocketAddr};
use std::os::fd::OwnedFd;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use h2::client::SendRequest;
use kbf_auth::Policy;
use kbf_front::{Cache, MemoryMetaLog, VerifiedBlob};
use kbf_objstore::MemoryStore;
use kbf_proto::reapi::BatchUpdateBlobsRequest;
use kbf_proto::reapi::batch_update_blobs_request::Request as BlobRequest;
use kbf_proto::reapi::content_addressable_storage_client::ContentAddressableStorageClient;
use kbf_proxy::control::NoUpdates;
use kbf_proxy::{Proxy, ProxyHandle};
use kbf_server::proxy_auth::ProxySecret;
use kbf_server::{Api, Behind, Listeners, bind_server_with_ui};
use kbf_testkit::dir::TestDir;
use proxy_support::{OPERATOR_TOKEN, connect, digest_of, free_base, grpc_request};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpSocket, TcpStream};
use tonic::transport::{Certificate, ClientTlsConfig, Endpoint};

/// The Buck2 client's bearer token (`buck2_token_file`).
const BUCK2_TOKEN: &str = "buck2-token-0123456789-abcdefghijklmnopqrst";

/// The policy of an open farm: every caller accepted, builds and blobs readable.
const OPEN: &str = r#"{"authenticationPolicy": {"allow": {}},
    "invocations": {"getAuthorizer": {"allow": {}}}, "blobs": {"getAuthorizer": {"allow": {}}}}"#;

/// The ruling's policy, as the browser must be sent it.
const CSP: &str = "default-src 'none'; style-src 'self'; img-src 'self'; frame-ancestors 'none'; \
                   base-uri 'none'; form-action 'self'";

/// A real proxy in front of a real server, and what a test needs to talk to them.
struct Rig {
    handle: ProxyHandle,
    proxy: SocketAddr,
    ca: std::path::PathBuf,
    cache: Arc<Cache<MemoryMetaLog, MemoryStore>>,
    stop: tokio::sync::oneshot::Sender<()>,
    serving: tokio::task::JoinHandle<()>,
    /// Held so the directory outlives the proxy.
    _dir: TestDir,
}

fn private(path: &Path, content: &[u8]) {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::write(path, content).expect("write");
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).expect("chmod");
}

fn loopback(port: u16) -> SocketAddr {
    SocketAddr::from((Ipv4Addr::LOCALHOST, port))
}

impl Rig {
    async fn start(name: &str) -> Self {
        let dir = TestDir::under(
            env!("CARGO_TARGET_TMPDIR"),
            &format!("ui-through-proxy-{name}"),
        );
        let base = free_base();
        let pki = kbf_integration_test::pki::write(&dir.path().join("pki"), "ui-test")
            .expect("certificates");
        private(
            &dir.path().join("operator.token"),
            OPERATOR_TOKEN.as_bytes(),
        );
        private(&dir.path().join("buck2.token"), BUCK2_TOKEN.as_bytes());
        let secret_file = dir.path().join("proxy-auth");
        let config = kbf_proxy::config::ProxyConfig::from_toml_str(&format!(
            "operator_token_file = {:?}\nserver_cert_file = {:?}\nserver_key_file = {:?}\n\
             client_ca_file = {:?}\nproxy_auth_file = {secret_file:?}\n\
             buck2_auth = \"bearer\"\nbuck2_token_file = {:?}\n\
             [[frontend]]\nname = \"a\"\nbase_port = {base}\n",
            dir.path().join("operator.token"),
            pki.server_cert,
            pki.server_key,
            pki.ca_cert,
            dir.path().join("buck2.token"),
        ))
        .expect("a valid table");
        let socket = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind");
        let proxy = socket.local_addr().expect("address");
        // The proxy builds its own runtime, which must not be built or dropped inside this one.
        let handle = tokio::task::spawn_blocking(move || {
            let (handle, runner) =
                Proxy::bind(config, OwnedFd::from(socket)).expect("bind the proxy");
            std::thread::spawn(move || runner.run(NoUpdates));
            handle
        })
        .await
        .expect("the proxy starts");
        let secret = ProxySecret::from_file(&secret_file).expect("the file the proxy wrote");

        let cache = Arc::new(Cache::memory());
        let listeners = Listeners {
            reapi: loopback(base),
            worker: loopback(base + 1),
            reapi_tls: None,
            worker_tls: None,
            heartbeat_interval: Duration::from_millis(200),
            hello_wait: Duration::from_secs(5),
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
            streams: kbf_server::StreamTiming::default(),
            store_probe_timeout: kbf_server::health::STORE_PROBE_TIMEOUT,
            invocation_memory: kbf_front::DEFAULT_INVOCATION_MEMORY,
            segments: kbf_front::SegmentsConfig::DEFAULT,
            stream_limits: kbf_server::StreamLimits::DEFAULT,
        };
        let api = Api {
            listen: loopback(base + 2),
            token: None,
            local_cache: None,
        };
        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        let bound = bind_server_with_ui(
            Arc::clone(&cache),
            listeners,
            Some(api),
            Policy::from_json(OPEN).expect("a policy"),
            None,
            None,
            None,
            Some(Behind {
                secret: Arc::new(secret),
                deny_list: None,
            }),
            true,
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
            cache,
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

    /// A TLS connection to the proxy from `from` (a loopback address), for HTTP/1.1 or h2.
    async fn tls(
        &self,
        from: Ipv4Addr,
        alpn: &[&[u8]],
    ) -> tokio_rustls::client::TlsStream<TcpStream> {
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
        config.alpn_protocols = alpn.iter().map(|p| p.to_vec()).collect();
        let socket = TcpSocket::new_v4().expect("a socket");
        socket
            .bind(SocketAddr::from((from, 0)))
            .expect("bind the client's own loopback address");
        let tcp = socket.connect(self.proxy).await.expect("connect");
        tokio_rustls::TlsConnector::from(Arc::new(config))
            .connect(
                rustls::pki_types::ServerName::try_from("localhost").expect("name"),
                tcp,
            )
            .await
            .expect("TLS handshake")
    }

    /// One HTTP/1.1 request from `from` and its whole answer.
    async fn send(&self, from: Ipv4Addr, method: &str, path: &str, headers: &str) -> Reply {
        let mut tls = self.tls(from, &[b"http/1.1"]).await;
        let request = request(method, path, headers);
        tls.write_all(request.as_bytes()).await.expect("write");
        let mut out = Vec::new();
        // A read error after the data (no close_notify) still leaves what arrived in `out`.
        let _ = tls.read_to_end(&mut out).await;
        Reply::parse(&out)
    }

    /// [`Rig::send`] from an address of no importance, with no credential.
    async fn anon(&self, method: &str, path: &str) -> Reply {
        self.send(Ipv4Addr::LOCALHOST, method, path, "").await
    }

    /// [`Rig::send`] as the operator.
    async fn operator(&self, method: &str, path: &str) -> Reply {
        let headers = format!("Authorization: Bearer {OPERATOR_TOKEN}\r\n");
        self.send(Ipv4Addr::LOCALHOST, method, path, &headers).await
    }

    /// An h2 client with no certificate: what a gRPC caller sends to the proxy.
    async fn h2(&self) -> SendRequest<Bytes> {
        let (client, _driver) = connect(&self.proxy.to_string(), &self.ca).await;
        client
    }
}

fn request(method: &str, path: &str, headers: &str) -> String {
    format!(
        "{method} {path} HTTP/1.1\r\nHost: localhost\r\n{headers}Content-Length: 0\r\n\
         Connection: close\r\n\r\n"
    )
}

/// An HTTP/1.1 answer.
struct Reply {
    status: u16,
    head: String,
    body: Vec<u8>,
}

impl Reply {
    fn parse(raw: &[u8]) -> Self {
        let end = raw
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .unwrap_or_else(|| panic!("no head in {:?}", String::from_utf8_lossy(raw)));
        let head = String::from_utf8_lossy(&raw[..end]).into_owned();
        let status = head
            .split_whitespace()
            .nth(1)
            .and_then(|c| c.parse().ok())
            .unwrap_or_else(|| panic!("no status in {head:?}"));
        Self {
            status,
            head,
            body: raw[end + 4..].to_vec(),
        }
    }

    /// Every value of header `name`, in order.
    fn headers(&self, name: &str) -> Vec<&str> {
        self.head
            .lines()
            .skip(1)
            .filter_map(|line| {
                let (key, value) = line.split_once(':')?;
                key.trim()
                    .eq_ignore_ascii_case(name)
                    .then_some(value.trim())
            })
            .collect()
    }

    /// The one value of header `name`: absent or sent twice fails the test.
    fn header(&self, name: &str) -> &str {
        let all = self.headers(name);
        assert_eq!(all.len(), 1, "{name}: {all:?} in {}", self.head);
        all[0]
    }
}

/// The `grpc-status` of an unauthenticated call of `path` over `client`.
async fn grpc_status(client: &SendRequest<Bytes>, path: &str) -> String {
    let (response, _) = client
        .clone()
        .ready()
        .await
        .expect("capacity")
        .send_request(grpc_request(path), true)
        .expect("send_request");
    let response = response.await.expect("an answer");
    if let Some(status) = response.headers().get("grpc-status") {
        return status.to_str().expect("text").to_owned();
    }
    let mut body = response.into_body();
    while let Some(chunk) = body.data().await {
        let chunk = chunk.expect("data");
        let _ = body.flow_control().release_capacity(chunk.len());
    }
    let trailers = body.trailers().await.expect("trailers");
    trailers
        .and_then(|t| {
            t.get("grpc-status")
                .map(|v| v.to_str().unwrap_or("").to_owned())
        })
        .unwrap_or_default()
}

/// A caller with no credential reads the UI: the index, the builds list, an error page, the
/// stylesheet and HEAD, each `200` (or the page's own `404`) with the ruling's headers, each
/// of them once.
///
/// Kills: `/ui` behind the operator token or a certificate; a page, an error or the
/// stylesheet answered without the CSP, with it twice (the proxy adding its own), or with
/// `no-store`, `nosniff` or `no-referrer` lost on the way.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_ui_answers_without_credentials_with_the_security_headers() {
    let rig = Rig::start("headers").await;
    for (method, path, status) in [
        ("GET", "/ui", 200),
        ("GET", "/ui/", 200),
        // The list reads `/v1/invocations`, which this server has no history for (it answers
        // as an unknown route, `404`): the page says history is off, `200`, with the headers.
        ("GET", "/ui/builds", 200),
        ("GET", "/ui/static/kbf.css", 200),
        ("HEAD", "/ui", 200),
        ("GET", "/ui/builds/no-such-build", 404),
        ("GET", "/ui/no-such-page", 404),
    ] {
        let reply = rig.anon(method, path).await;
        assert_eq!(reply.status, status, "{method} {path}: {}", reply.head);
        assert_eq!(reply.header("content-security-policy"), CSP, "{path}");
        assert_eq!(reply.header("x-content-type-options"), "nosniff", "{path}");
        assert_eq!(reply.header("referrer-policy"), "no-referrer", "{path}");
        assert_eq!(reply.header("cache-control"), "no-store", "{path}");
        let media = reply.header("content-type");
        if path.ends_with(".css") {
            assert!(media.starts_with("text/css"), "{path}: {media}");
        } else {
            assert!(media.starts_with("text/html"), "{path}: {media}");
        }
        assert!(
            reply.headers("www-authenticate").is_empty(),
            "{path}: no credential is asked for"
        );
    }
    let index = rig.anon("GET", "/ui").await;
    let page = String::from_utf8_lossy(&index.body);
    assert!(page.contains("<h1>Overview</h1>"), "{page}");
    assert!(!page.contains("<script"), "no script on a page");
    // No history store on this farm: the list says so and names the flag, where a missing
    // build (a record the farm does not hold) stays a `404`.
    let builds = rig.anon("GET", "/ui/builds").await;
    let page = String::from_utf8_lossy(&builds.body);
    assert!(page.contains("History is off on this farm"), "{page}");
    assert!(page.contains("--history-objects"), "{page}");
    rig.stop().await;
}

/// Every method but `GET` and `HEAD` is a `405` on the index and under it, naming the two
/// allowed.
///
/// Kills: a write forwarded to a page, on `/ui` or on a page under it; a `405` without
/// `Allow`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_write_to_the_ui_is_a_405() {
    let rig = Rig::start("methods").await;
    for method in ["POST", "PUT", "PATCH", "DELETE", "OPTIONS"] {
        for path in [
            "/ui",
            "/ui/builds",
            "/ui/builds/b1",
            "/ui/builds/b1/actions/d",
            "/ui/static/kbf.css",
        ] {
            let reply = rig.anon(method, path).await;
            assert_eq!(reply.status, 405, "{method} {path}: {}", reply.head);
            let allow = reply.header("allow");
            assert!(
                allow.contains("GET") && allow.contains("HEAD"),
                "{method} {path}: {allow}"
            );
        }
    }
    rig.stop().await;
}

/// The UI being open opens nothing else: with no certificate and no token, every `/v1` read
/// and write, `/metrics`, the update API and look-alikes of `/ui` under `/v1` are `401`, and
/// REAPI and the worker listener answer `UNAUTHENTICATED`. The same operator routes with the
/// token are served (so the refusals are the credential, not a missing route), and a wrong
/// token is refused like none.
///
/// Kills: the open class leaking to a `/v1` route, `/metrics` or the update API (a prefix
/// match that takes `/v1/ui` or `/uix` for the UI); REAPI or a worker path in the open class.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn no_other_route_opens_without_credentials() {
    let rig = Rig::start("closed").await;
    let routes = [
        "/v1/nodes",
        "/v1/nodes/n1:drain",
        "/v1/status",
        "/v1/blobs/abc-1",
        "/v1/invocations/i1",
        "/v1/invocations/i1/actions",
        "/v1/leases",
        "/v1/queue",
        "/v1/storage",
        "/v1/health",
        "/v1/update",
        "/v1/update/desired",
        "/v1/update/frontend",
        "/v1/update/frontend/abort",
        "/v1/frontend/drain",
        "/v1/ui",
        "/v1/ui/builds",
        "/v1/uix",
        "/metrics",
    ];
    for path in routes {
        for method in ["GET", "HEAD", "POST", "PUT", "PATCH", "DELETE"] {
            let reply = rig.anon(method, path).await;
            assert_eq!(reply.status, 401, "{method} {path}: {}", reply.head);
        }
    }
    // A wrong token is a failure the proxy rate-limits per address (a burst of ten), so a
    // few only.
    for path in ["/v1/nodes", "/v1/blobs/abc-1", "/metrics"] {
        let wrong = rig
            .send(
                Ipv4Addr::LOCALHOST,
                "GET",
                path,
                "Authorization: Bearer not-the-operator-token-0123456789-abcdef\r\n",
            )
            .await;
        assert_eq!(wrong.status, 401, "{path} wrong token");
    }
    let served = rig.operator("GET", "/v1/nodes").await;
    assert_eq!(served.status, 200, "{}", served.head);
    assert_eq!(rig.operator("GET", "/v1/ui").await.status, 404);

    let client = rig.h2().await;
    for path in [
        "/build.bazel.remote.execution.v2.Execution/Execute",
        "/build.bazel.remote.execution.v2.ContentAddressableStorage/BatchUpdateBlobs",
        "/build.bazel.remote.execution.v2.ActionCache/GetActionResult",
        "/build.bazel.remote.execution.v2.Capabilities/GetCapabilities",
        "/google.bytestream.ByteStream/Write",
        "/kbf.worker.v1.Worker/Session",
        "/kbf.map.v1.Membership/Register",
    ] {
        assert_eq!(grpc_status(&client, path).await, "16", "{path}");
    }
    rig.stop().await;
}

/// An uploaded HTML page comes back from the blob route as bytes to save, never as a page:
/// `application/octet-stream`, `Content-Disposition: attachment`, `nosniff`, and a sandbox
/// CSP; `?inline=text` serves it as `text/plain`, not `text/html`; and without the operator
/// token the route is a `401`.
///
/// Kills: the attachment disposition dropped (the browser would render the upload at the
/// farm's own origin); the blob served as `text/html`; `nosniff` or the sandbox lost on the
/// way; the blob route readable with no credential.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_uploaded_html_blob_comes_back_as_an_attachment() {
    let rig = Rig::start("attachment").await;
    let html = b"<!doctype html><html><body><script>alert(document.cookie)</script></body></html>";
    let digest = digest_of(html);
    let tls = ClientTlsConfig::new()
        .domain_name("localhost")
        .ca_certificate(Certificate::from_pem(
            std::fs::read_to_string(&rig.ca).expect("CA"),
        ));
    let channel = Endpoint::from_shared(format!("https://localhost:{}", rig.proxy.port()))
        .expect("an endpoint")
        .tls_config(tls)
        .expect("tls")
        .connect()
        .await
        .expect("connect through the proxy");
    let mut cas = ContentAddressableStorageClient::with_interceptor(
        channel,
        |mut request: tonic::Request<()>| {
            request.metadata_mut().insert(
                "authorization",
                format!("Bearer {BUCK2_TOKEN}").parse().expect("metadata"),
            );
            Ok(request)
        },
    );
    let stored = cas
        .batch_update_blobs(BatchUpdateBlobsRequest {
            requests: vec![BlobRequest {
                digest: Some(digest.clone()),
                data: html.to_vec(),
                ..BlobRequest::default()
            }],
            ..BatchUpdateBlobsRequest::default()
        })
        .await
        .expect("the upload through the proxy")
        .into_inner();
    assert_eq!(stored.responses.len(), 1);
    assert!(
        stored.responses[0]
            .status
            .as_ref()
            .is_none_or(|s| s.code == 0)
    );

    let path = format!("/v1/blobs/{}-{}", digest.hash, digest.size_bytes);
    let reply = rig.operator("GET", &path).await;
    assert_eq!(reply.status, 200, "{}", reply.head);
    assert!(
        reply
            .header("content-disposition")
            .starts_with("attachment")
    );
    assert_eq!(reply.header("content-type"), "application/octet-stream");
    assert_eq!(reply.header("x-content-type-options"), "nosniff");
    assert_eq!(reply.header("content-security-policy"), "sandbox");
    assert!(
        reply.body.windows(html.len()).any(|w| w == html),
        "the bytes come back whole"
    );

    let text = rig.operator("GET", &format!("{path}?inline=text")).await;
    assert_eq!(text.status, 200, "{}", text.head);
    assert!(text.header("content-type").starts_with("text/plain"));
    assert_eq!(text.header("x-content-type-options"), "nosniff");
    assert_eq!(text.header("content-security-policy"), "sandbox");

    assert_eq!(rig.anon("GET", &path).await.status, 401);
    rig.stop().await;
}

/// A `GET` of `path` from `from` that has its head and is then left unread, so the server
/// still holds the read open (the answer is larger than the connections between hold). The
/// status is returned with the connection.
async fn hold(
    rig: &Rig,
    from: Ipv4Addr,
    path: &str,
) -> (u16, tokio_rustls::client::TlsStream<TcpStream>) {
    let mut tls = rig.tls(from, &[b"http/1.1"]).await;
    let headers = format!("Authorization: Bearer {OPERATOR_TOKEN}\r\n");
    tls.write_all(request("GET", path, &headers).as_bytes())
        .await
        .expect("write");
    let mut head = Vec::new();
    let mut byte = [0_u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        let n = tls.read(&mut byte).await.expect("a head");
        assert_eq!(
            n,
            1,
            "closed before the head: {:?}",
            String::from_utf8_lossy(&head)
        );
        head.push(byte[0]);
    }
    let status = String::from_utf8_lossy(&head)
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .expect("a status");
    (status, tls)
}

/// Two client addresses behind the one proxy each get their own read bound: one that holds
/// as many reads open as the bound allows is refused its next with `429` and `Retry-After`,
/// while another address's first reads are served, and then it too is held to the bound.
/// The reads are of a large blob over `/v1/blobs` (the one answer that stays open long
/// enough to hold), which shares the caller key and the bound's code with `/ui`.
///
/// Kills: the proxy's `x-kbf-client-addr` not forwarded, or not read by the server, so the
/// browsers all count as the proxy's address (the second address's first read is a `429`);
/// a bound that is not enforced (the fifth read is served).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_browsers_each_have_their_own_read_bound() {
    let rig = Rig::start("bound").await;
    let big = VerifiedBlob::hashed(Bytes::from(vec![7_u8; 48 << 20]));
    let digest = big.digest();
    rig.cache.store_blobs(vec![big]).await.expect("store");
    let path = format!("/v1/blobs/{}-{}", digest.hash_hex(), digest.size_bytes);

    let mut held = Vec::new();
    for from in [Ipv4Addr::new(127, 0, 0, 2), Ipv4Addr::new(127, 0, 0, 3)] {
        for n in 0..kbf_server::read_limit::PER_CALLER {
            let (status, tls) = hold(&rig, from, &path).await;
            assert_eq!(status, 200, "{from} read {n}");
            held.push(tls);
        }
        let (status, _) = hold(&rig, from, &path).await;
        assert_eq!(status, 429, "{from}: one past the bound");
    }
    // The bound is released with the reads: the first address is served again.
    held.drain(..kbf_server::read_limit::PER_CALLER);
    let mut again = 200;
    for _ in 0..200 {
        again = hold(&rig, Ipv4Addr::new(127, 0, 0, 2), &path).await.0;
        if again == 200 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(again, 200, "the held reads were dropped");
    drop(held);
    rig.stop().await;
}
