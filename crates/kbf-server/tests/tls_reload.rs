//! The REAPI and worker listeners' certificates replaced on disk while the server runs:
//! the flags (`--tls-reload-interval-secs`) and the metrics, over the real listeners and
//! tonic clients.
//!
//! No test here awaits without a deadline. A wait with no end hangs the test, and the CI
//! job that runs it, instead of failing it: every connection, call, scrape, bind and poll
//! a test awaits is bounded by [`within`], and fails naming what it waited for. (The
//! accept loops of the fixture servers are tasks no test awaits.) A client attempt dials
//! once ([`DialOnce`]), so one that is refused is one connection that ended.

use std::future::{Future, pending};
use std::io;
use std::net::SocketAddr;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll};
use std::time::Duration;

use clap::Parser;
use futures::StreamExt;
use futures::channel::mpsc::unbounded;
use hyper_util::rt::TokioIo;
use kbf_front::Cache;
use kbf_proto::reapi::GetCapabilitiesRequest;
use kbf_proto::reapi::capabilities_client::CapabilitiesClient;
use kbf_proto::worker::{
    Capability, DaemonMessage, Hello, daemon_message, server_message, worker_client::WorkerClient,
};
use kbf_server::{Api, Args, bind_server_with_api};
use kbf_tls::{Incoming, TlsSource};
use rcgen::{
    BasicConstraints, CertificateParams, CertifiedIssuer, DnType, ExtendedKeyUsagePurpose, IsCa,
    KeyPair, KeyUsagePurpose, SanType,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tonic::codegen::Service;
use tonic::codegen::http::Uri;
use tonic::transport::server::TcpIncoming;
use tonic::transport::{Certificate, Channel, ClientTlsConfig, Endpoint, Identity};

/// How long one step of a client attempt (the connection and its handshake, a call, the
/// first answer to it) or a scrape may take on loopback before the test fails naming it.
const STEP: Duration = Duration::from_secs(10);

/// How long the server may take to look at replaced files again (its interval is one
/// second here) before the test fails naming what it waited for.
const RELOAD: Duration = Duration::from_secs(30);

struct Ca {
    issuer: CertifiedIssuer<'static, KeyPair>,
}

struct Pair {
    cert: String,
    key: String,
}

fn ca(name: &str) -> Ca {
    let mut params = CertificateParams::new(Vec::<String>::new()).expect("CA params");
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params.distinguished_name.push(DnType::CommonName, name);
    params.key_usages = vec![
        KeyUsagePurpose::KeyCertSign,
        KeyUsagePurpose::DigitalSignature,
    ];
    Ca {
        issuer: CertifiedIssuer::self_signed(params, KeyPair::generate().expect("key"))
            .expect("CA"),
    }
}

impl Ca {
    fn pem(&self) -> String {
        self.issuer.pem()
    }

    fn leaf(&self, name: &str, usage: ExtendedKeyUsagePurpose, role: Option<&str>) -> Pair {
        let mut params = CertificateParams::new(vec![name.to_owned()]).expect("params");
        // The worker listener reads the role from one `urn:kbf:role:<name>` URI.
        params.subject_alt_names.extend(role.map(|role| {
            SanType::URI(
                format!("urn:kbf:role:{role}")
                    .try_into()
                    .expect("an IA5 name"),
            )
        }));
        params.distinguished_name.push(DnType::CommonName, name);
        params.extended_key_usages = vec![usage];
        let key = KeyPair::generate().expect("key");
        let cert = params.signed_by(&key, &self.issuer).expect("sign");
        Pair {
            cert: cert.pem(),
            key: key.serialize_pem(),
        }
    }

    fn server(&self) -> Pair {
        self.leaf("localhost", ExtendedKeyUsagePurpose::ServerAuth, None)
    }
}

/// A directory of this test's own, removed when it drops: two runs of the binary at once
/// do not replace each other's files.
fn dir(name: &str) -> kbf_testkit::dir::TestDir {
    kbf_testkit::dir::TestDir::under(
        env!("CARGO_TARGET_TMPDIR"),
        &format!("kbf-server-reload-{name}"),
    )
}

/// Replaces `dir/file` whole, as a renewal tool does.
fn put(dir: &Path, file: &str, contents: &str) {
    let aside = dir.join(format!("{file}.new"));
    std::fs::write(&aside, contents).expect("write");
    std::fs::rename(&aside, dir.join(file)).expect("rename");
}

fn path(dir: &Path, file: &str) -> String {
    dir.join(file).to_str().expect("UTF-8 path").to_owned()
}

/// `future`'s output, or a panic naming `what` if it is not ready within `limit`.
async fn within<T>(limit: Duration, what: &str, future: impl Future<Output = T>) -> T {
    match tokio::time::timeout(limit, future).await {
        Ok(output) => output,
        Err(_) => panic!("timed out after {limit:?} waiting for {what}"),
    }
}

/// A loopback listener on a port of its choosing.
async fn bound() -> TcpListener {
    within(
        STEP,
        "binding a loopback port",
        TcpListener::bind("127.0.0.1:0"),
    )
    .await
    .expect("bind")
}

/// A connector that dials once, for a client attempt.
///
/// A tonic `Channel` dials again, at once, whenever its connection has closed before a
/// request was sent on it, inside `connect()` and the call waiting behind it. A worker
/// listener refusing a client certificate closes the connection that way: TLS 1.3
/// finishes the client's handshake before the server judges the certificate, so the
/// connection looks open until the server's alert ends it, and whether the client's
/// HTTP/2 task or that alert is first is a race, so a refused attempt can open a second
/// connection. Here the second dial fails, and an attempt is one connection: one that is
/// refused is one that ended.
#[derive(Default)]
struct DialOnce {
    dialed: bool,
}

impl Service<Uri> for DialOnce {
    type Response = TokioIo<TcpStream>;
    type Error = io::Error;
    type Future = Pin<Box<dyn Future<Output = io::Result<Self::Response>> + Send>>;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, uri: Uri) -> Self::Future {
        let first = !std::mem::replace(&mut self.dialed, true);
        Box::pin(async move {
            if !first {
                return Err(io::Error::other(
                    "the connection closed, and this client dials once",
                ));
            }
            let authority = uri
                .authority()
                .ok_or_else(|| io::Error::other("the address has no host and port"))?;
            let stream = TcpStream::connect(authority.as_str()).await?;
            // As tonic's own connector does: without it a small call waits out the delayed
            // acknowledgement of the one before.
            stream.set_nodelay(true)?;
            Ok(TokioIo::new(stream))
        })
    }
}

fn client(addr: SocketAddr, trust: &str, identity: Option<Identity>) -> Endpoint {
    let mut tls = ClientTlsConfig::new()
        .ca_certificate(Certificate::from_pem(trust))
        .domain_name("localhost");
    if let Some(identity) = identity {
        tls = tls.identity(identity);
    }
    Endpoint::from_shared(format!("https://{addr}"))
        .expect("endpoint")
        .tls_config(tls)
        .expect("tls")
}

/// The channel over one connection to `endpoint`, once its TLS handshake is done within
/// `limit`; `None` if the connection or the handshake fails (a refused server
/// certificate, say).
async fn connect(limit: Duration, endpoint: Endpoint) -> Option<Channel> {
    let connecting = endpoint.connect_with_connector(DialOnce::default());
    within(limit, "the connection and TLS handshake", connecting)
        .await
        .ok()
}

async fn capabilities(channel: Channel) -> bool {
    let mut capabilities = CapabilitiesClient::new(channel);
    let call = capabilities.get_capabilities(GetCapabilitiesRequest::default());
    within(STEP, "the answer to GetCapabilities", call)
        .await
        .is_ok()
}

async fn capabilities_answered(endpoint: Endpoint) -> bool {
    match connect(STEP, endpoint).await {
        Some(channel) => capabilities(channel).await,
        None => false,
    }
}

/// Whether a daemon session to `endpoint` is welcomed, each step within `limit`: a
/// refusal at any step (the server certificate, the client certificate, the call) is
/// `false`, a step that does not answer in time fails the test.
async fn session_welcomed_within(limit: Duration, endpoint: Endpoint) -> bool {
    let Some(channel) = connect(limit, endpoint).await else {
        return false;
    };
    let (tx, rx) = unbounded();
    let capability = |key: &str, value: &str| Capability {
        key: key.to_owned(),
        value: value.to_owned(),
    };
    let hello = Hello {
        protocol_version: 1,
        node_id: "node-1".to_owned(),
        capabilities: vec![
            capability("cpus", "1"),
            capability("mem_gib", "1"),
            capability("arch", "x86_64"),
        ],
        ..Hello::default()
    };
    tx.unbounded_send(DaemonMessage {
        message: Some(daemon_message::Message::Hello(hello)),
    })
    .expect("queue Hello");
    let mut worker = WorkerClient::new(channel);
    let call = worker.session(rx);
    let Ok(response) = within(limit, "the response to the session call", call).await else {
        return false;
    };
    let first = within(
        limit,
        "the first message of the session",
        response.into_inner().message(),
    )
    .await;
    matches!(
        first,
        Ok(Some(m)) if matches!(m.message, Some(server_message::Message::Welcome(_)))
    )
}

async fn session_welcomed(endpoint: Endpoint) -> bool {
    session_welcomed_within(STEP, endpoint).await
}

/// Polls `check` every 100 ms until it holds, and fails naming `what` if it has not by
/// then.
async fn until<F: Future<Output = bool>>(what: &str, mut check: impl FnMut() -> F) {
    let polling = async {
        while !check().await {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    };
    within(RELOAD, what, polling).await;
}

async fn scrape(api: SocketAddr) -> String {
    let exchange = async {
        let mut tcp = TcpStream::connect(api).await.expect("connect");
        let request = "GET /metrics HTTP/1.1\r\nHost: kbf\r\nConnection: close\r\n\r\n";
        tcp.write_all(request.as_bytes()).await.expect("send");
        let mut response = String::new();
        tcp.read_to_string(&mut response).await.expect("read");
        response
    };
    within(STEP, "the answer to the /metrics scrape", exchange).await
}

/// How many looks at `listener`'s files ended as `outcome` says, from `/metrics`.
async fn looks(api: SocketAddr, listener: &str, outcome: &str) -> u64 {
    let prefix = format!("kbf_tls_reloads_total{{listener=\"{listener}\",outcome=\"{outcome}\"}} ");
    scrape(api)
        .await
        .lines()
        .find_map(|line| line.strip_prefix(prefix.as_str()).map(str::to_owned))
        .map_or(0, |count| count.parse().expect("a count"))
}

struct Running {
    reapi: SocketAddr,
    worker: SocketAddr,
    api: SocketAddr,
}

fn start(args: &[String]) -> Running {
    let mut all = vec![
        "kbf-server".to_owned(),
        "--listen".to_owned(),
        "127.0.0.1:0".to_owned(),
        "--worker-listen".to_owned(),
        "127.0.0.1:0".to_owned(),
        "--tls-reload-interval-secs".to_owned(),
        "1".to_owned(),
    ];
    all.extend(args.iter().cloned());
    let listeners = Args::parse_from(all).listeners().expect("listeners");
    let api = Api {
        listen: SocketAddr::from(([127, 0, 0, 1], 0)),
        token: None,
        local_cache: None,
        releases: None,
    };
    let bound = bind_server_with_api(Arc::new(Cache::memory()), listeners, Some(api), pending())
        .expect("bind");
    let running = Running {
        reapi: bound.reapi,
        worker: bound.worker,
        api: bound.api.expect("an API address"),
    };
    tokio::spawn(async move { bound.serving.await.expect("serve") });
    running
}

/// A REAPI certificate renewed by replacing the files: a connection open at the time
/// keeps being answered, a client that trusts only the new certificate's issuer is
/// answered once the files are looked at again, the old issuer's client is not, and a
/// bad file in between is counted in `/metrics` while the good pair keeps serving.
///
/// Kills: a listener that reads its certificate once; a renewal that ends the open
/// connections; garbage replacing the working certificate.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_renewed_reapi_certificate_is_served_without_a_restart() {
    let (old_ca, new_ca) = (ca("old issuer"), ca("new issuer"));
    let d = dir("reapi");
    let old = old_ca.server();
    put(&d, "server.pem", &old.cert);
    put(&d, "server.key", &old.key);
    let running = start(&[
        "--reapi-tls-cert".to_owned(),
        path(&d, "server.pem"),
        "--reapi-tls-key".to_owned(),
        path(&d, "server.key"),
    ]);

    let open = connect(STEP, client(running.reapi, &old_ca.pem(), None))
        .await
        .expect("TLS handshake");
    assert!(capabilities(open.clone()).await);

    put(&d, "server.pem", "garbage in the middle of a renewal");
    until("the garbage to be refused", || async {
        scrape(running.api)
            .await
            .contains("kbf_tls_reloads_total{listener=\"reapi\",outcome=\"rejected\"} 1")
    })
    .await;
    assert!(
        capabilities_answered(client(running.reapi, &old_ca.pem(), None)).await,
        "garbage replaced the working certificate"
    );

    let new = new_ca.server();
    put(&d, "server.pem", &new.cert);
    put(&d, "server.key", &new.key);
    until("the renewed certificate to be served", || async {
        capabilities_answered(client(running.reapi, &new_ca.pem(), None)).await
    })
    .await;
    assert!(
        !capabilities_answered(client(running.reapi, &old_ca.pem(), None)).await,
        "the old certificate is still served to new connections"
    );
    assert!(capabilities(open).await, "the open connection was ended");
    let metrics = scrape(running.api).await;
    assert!(
        metrics.contains("kbf_tls_reloads_total{listener=\"reapi\",outcome=\"applied\"} 1"),
        "{metrics}"
    );
    assert!(metrics.contains("kbf_tls_leaf_not_after_seconds{listener=\"reapi\"} "));
}

/// The worker listener's three files: after its client CA is replaced, a daemon with a
/// certificate of the new CA is welcomed and one of the old CA is not; after its server
/// certificate is replaced, a daemon that trusts only the new issuer is welcomed and
/// one that trusts the old issuer is not.
///
/// Each replacement is waited for in `/metrics` (the server counted the look that
/// applied it), and then tried once. The listener swaps its configuration before it
/// counts the look, so the one attempt after the count is decided by the new files.
/// Polling with connection attempts instead, as this test once did, made every attempt
/// before the reload a refusal to wait out.
///
/// Kills: a worker listener whose trust roots or identity are read once.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_worker_listener_reloads_its_client_ca_and_identity() {
    let (server_ca, new_server_ca) = (ca("server issuer"), ca("new server issuer"));
    let (old_clients, new_clients) = (ca("old clients"), ca("new clients"));
    let d = dir("worker");
    let server = server_ca.server();
    put(&d, "server.pem", &server.cert);
    put(&d, "server.key", &server.key);
    put(&d, "ca.pem", &old_clients.pem());
    let running = start(&[
        "--worker-tls-cert".to_owned(),
        path(&d, "server.pem"),
        "--worker-tls-key".to_owned(),
        path(&d, "server.key"),
        "--worker-client-ca".to_owned(),
        path(&d, "ca.pem"),
    ]);
    let node = |issuer: &Ca| {
        let pair = issuer.leaf(
            "node-1",
            ExtendedKeyUsagePurpose::ClientAuth,
            Some("daemon"),
        );
        Identity::from_pem(pair.cert, pair.key)
    };
    let (old_node, new_node) = (node(&old_clients), node(&new_clients));
    let welcomed = |trust: &Ca, node: &Identity| {
        session_welcomed(client(running.worker, &trust.pem(), Some(node.clone())))
    };
    let applied = |n: u64| async move { looks(running.api, "worker", "applied").await == n };

    assert!(
        welcomed(&server_ca, &old_node).await,
        "a certificate of the CA the files held at start is refused"
    );
    put(&d, "ca.pem", &new_clients.pem());
    until("the new client CA to be applied", || applied(1)).await;
    assert!(
        welcomed(&server_ca, &new_node).await,
        "a certificate of the new CA is refused after the reload"
    );
    assert!(
        !welcomed(&server_ca, &old_node).await,
        "a certificate of the replaced CA is still welcomed"
    );

    let renewed = new_server_ca.server();
    put(&d, "server.pem", &renewed.cert);
    put(&d, "server.key", &renewed.key);
    // A look between the two files sees a certificate and a key that do not match and
    // refuses them; the look after it applies the pair.
    until("the new server certificate to be applied", || applied(2)).await;
    assert!(
        welcomed(&new_server_ca, &new_node).await,
        "the renewed server certificate is not served after the reload"
    );
    assert!(
        !welcomed(&server_ca, &new_node).await,
        "the replaced server certificate is still served"
    );
}

/// An attempt on a server that accepts the connection and never answers ends, and says
/// what it waited for: with the client this file once had (`Endpoint::connect()`, no
/// deadline) it waited for ever, and the test with it.
///
/// Kills: a wait in the client helpers that has no deadline.
#[tokio::test]
async fn an_attempt_on_a_server_that_never_answers_names_what_it_waited_for() {
    let silent = bound().await;
    let addr = silent.local_addr().expect("address");
    tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((stream, _)) = silent.accept().await {
            held.push(stream);
        }
    });
    let issuer = ca("issuer");
    let attempt = session_welcomed_within(
        Duration::from_millis(300),
        client(addr, &issuer.pem(), None),
    );
    let failed = tokio::spawn(attempt)
        .await
        .expect_err("an attempt on a silent server cannot be welcomed");
    let message = failed
        .into_panic()
        .downcast::<String>()
        .expect("a formatted message");
    assert!(
        message.contains("waiting for the connection and TLS handshake"),
        "{message}"
    );
}

/// The connector of a client attempt dials its address once and refuses a second dial,
/// which is what a `Channel` would do when its connection closes before it carries a
/// request.
///
/// Kills: a connector that dials again.
#[tokio::test]
async fn the_connector_of_an_attempt_dials_once() {
    let listener = bound().await;
    let uri: Uri = format!("https://{}", listener.local_addr().expect("address"))
        .parse()
        .expect("a URI");
    let mut dial = DialOnce::default();
    let first = dial.call(uri.clone());
    within(STEP, "the first dial", first)
        .await
        .expect("the first dial");
    let second = dial.call(uri);
    let again = within(STEP, "the second dial", second)
        .await
        .expect_err("a second dial");
    assert!(again.to_string().contains("dials once"), "{again}");
}

/// Attempts through [`connect`] on a server that completes the TLS handshake and then
/// ends the session are one connection each, counted on the server: a `Channel` from
/// `Endpoint::connect()` dials again when its connection has closed before a request
/// went out on it, and this makes that the case every time.
///
/// The server is kbf-tls's own `Incoming`, so a connection it yields is one whose
/// handshake it finished. It counts the connection, ends the TLS session, and counts
/// again when the client has let go of the socket. The client lets go once it has seen
/// the end (its connection task finishes, which drops the socket and, in the same poll,
/// the channel to it), so the call that follows finds a closed connection, on this
/// current-thread runtime, without a sleep. The connection is counted before it is
/// closed, so the count is exact when the attempt ends.
///
/// Kills: `connect` going back to `Endpoint::connect()`, or any client helper that dials
/// without [`DialOnce`].
#[tokio::test]
async fn attempts_through_connect_are_one_connection_each() {
    const ATTEMPTS: usize = 5;
    let issuer = ca("issuer");
    let server = issuer.server();
    let source = TlsSource::fixed(
        "closing",
        server.cert.as_bytes(),
        server.key.as_bytes(),
        None,
    )
    .expect("a TLS configuration");
    let tcp = TcpIncoming::bind(SocketAddr::from(([127, 0, 0, 1], 0))).expect("bind");
    let addr = tcp.local_addr().expect("address");
    let accepted = Arc::new(AtomicUsize::new(0));
    let let_go = Arc::new(AtomicUsize::new(0));
    let (counted, counted_let_go) = (Arc::clone(&accepted), Arc::clone(&let_go));
    tokio::spawn(async move {
        let mut incoming = Incoming::new(tcp, Some(source));
        while let Some(Ok(mut conn)) = incoming.next().await {
            counted.fetch_add(1, Ordering::SeqCst);
            let counted_let_go = Arc::clone(&counted_let_go);
            tokio::spawn(async move {
                let _ = conn.shutdown().await;
                // Ends when the client closes its end, cleanly or not.
                let _ = conn.read_to_end(&mut Vec::new()).await;
                counted_let_go.fetch_add(1, Ordering::SeqCst);
            });
        }
    });

    for attempt in 1..=ATTEMPTS {
        let channel = connect(STEP, client(addr, &issuer.pem(), None)).await;
        let letting_go = async {
            while let_go.load(Ordering::SeqCst) < attempt {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        };
        within(
            STEP,
            "the client to let go of a closed connection",
            letting_go,
        )
        .await;
        // What the call ends with does not matter, only how many connections it made.
        if let Some(channel) = channel {
            let _ = capabilities(channel).await;
        }
    }
    assert_eq!(
        accepted.load(Ordering::SeqCst),
        ATTEMPTS,
        "an attempt dialed more than once"
    );
}

/// The flag's range: zero and over an hour are refused, and the default is half a minute.
#[test]
fn the_reload_interval_flag_is_bounded() {
    let parse = |extra: &[&str]| {
        let mut all = vec!["kbf-server"];
        all.extend(extra);
        Args::try_parse_from(all)
    };
    assert_eq!(parse(&[]).expect("default").tls_reload_interval_secs, 30);
    assert!(parse(&["--tls-reload-interval-secs", "0"]).is_err());
    assert!(parse(&["--tls-reload-interval-secs", "3601"]).is_err());
    let long = parse(&["--tls-reload-interval-secs", "3600"]).expect("an hour");
    assert_eq!(long.tls_reload_interval_secs, 3600);
}
