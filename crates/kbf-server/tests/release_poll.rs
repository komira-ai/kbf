//! The release poller's request, against a fake GitHub API on loopback (no live network): the
//! headers it sends and the answers it reads, over plain HTTP and over TLS; then the poller
//! end to end, from the fake's feed to the proposals the operator API lists.
//!
//! The feed bodies are shaped like the API's (`GET /repos/{owner}/{repo}/releases`): the
//! fields the poller reads among the ones it does not.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use kbf_server::release::poll::github::{GithubSource, Roots};
use kbf_server::release::poll::{Config, Cycle, FetchError, Fetched, Poller, Source, Stats};
use kbf_server::release::{Limits, ReleaseStore};
use kbf_server::update::Rows;
use kbf_server::{Api, EpochPolicy, Listeners, StateStore, bind_server_with_state};
use kbf_state::fake::{Fake, Flaw};
use kbf_state::{ManualClock, Store};
use kbf_types::FarmTime;
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

const C: &str = "0123456789abcdef0123456789abcdef01234567";

/// A request the fake received: its lines, lower-cased names.
#[derive(Clone, Debug)]
struct Seen {
    line: String,
    headers: Vec<(String, String)>,
}

impl Seen {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }
}

type Handler = Arc<dyn Fn(&Seen) -> Vec<u8> + Send + Sync>;

/// A whole response with `body` and extra header lines.
fn reply(status: &str, extra: &[&str], body: &[u8]) -> Vec<u8> {
    let mut out = format!(
        "HTTP/1.1 {status}\r\ncontent-length: {}\r\nconnection: close\r\n",
        body.len()
    );
    for line in extra {
        out.push_str(line);
        out.push_str("\r\n");
    }
    out.push_str("\r\n");
    let mut bytes = out.into_bytes();
    bytes.extend_from_slice(body);
    bytes
}

async fn answer<S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin>(
    mut stream: S,
    handler: &Handler,
    seen: &Mutex<Vec<Seen>>,
) {
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        if stream.read(&mut byte).await.unwrap_or(0) == 0 {
            return;
        }
        head.push(byte[0]);
    }
    let text = String::from_utf8_lossy(&head).into_owned();
    let mut lines = text.split("\r\n");
    let line = lines.next().unwrap_or_default().to_owned();
    let headers = lines
        .filter_map(|l| l.split_once(": "))
        .map(|(n, v)| (n.to_ascii_lowercase(), v.to_owned()))
        .collect();
    let request = Seen { line, headers };
    let response = handler(&request);
    seen.lock().unwrap().push(request);
    let _ = stream.write_all(&response).await;
    let _ = stream.flush().await;
    let _ = stream.shutdown().await;
}

/// A plain-HTTP fake: where it listens and the requests it has seen.
async fn fake(handler: Handler) -> (SocketAddr, Arc<Mutex<Vec<Seen>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&seen);
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let (handler, log) = (Arc::clone(&handler), Arc::clone(&log));
            tokio::spawn(async move { answer(stream, &handler, &log).await });
        }
    });
    (addr, seen)
}

fn source(addr: SocketAddr) -> GithubSource {
    GithubSource::new(
        &format!("http://{addr}"),
        "komira-ai/kbf",
        Duration::from_secs(5),
        Roots::System,
    )
    .expect("a source")
}

fn handler(f: impl Fn(&Seen) -> Vec<u8> + Send + Sync + 'static) -> Handler {
    Arc::new(f)
}

fn feed_body() -> Vec<u8> {
    let tar = format!("kbf-{C}-linux-x86_64.tar.gz");
    serde_json::to_vec(&json!([
        {
            "url": "https://api.github.com/repos/komira-ai/kbf/releases/1",
            "id": 1,
            "author": {"login": "someone", "type": "User"},
            "tag_name": "v0.9.1",
            "target_commitish": "main",
            "name": "v0.9.1",
            "draft": false,
            "prerelease": false,
            "created_at": "2026-10-19T23:59:00Z",
            "published_at": "2026-10-20T00:00:00Z",
            "body": format!("The files run 5 of artifacts.yml built and attested for {C} on main (https://github.com/komira-ai/kbf/actions/runs/5), unchanged."),
            "assets": [
                {"id": 7, "name": "SHA256SUMS", "size": 300, "state": "uploaded"},
                {"id": 8, "name": tar, "size": 9000, "state": "uploaded"},
                {"id": 9, "name": format!("{tar}.sigstore.json"), "size": 5000, "state": "uploaded"}
            ]
        }
    ]))
    .expect("json")
}

// Catches: the request carrying a credential, the wrong path or no User-Agent (GitHub refuses
// one), no JSON Accept, a missing `If-None-Match` (every poll would cost a full answer), or
// an `ETag` that is dropped from the answer.
#[tokio::test]
async fn the_request_is_anonymous_conditional_and_reads_the_etag() {
    let (addr, seen) = fake(handler(|r| match r.header("if-none-match") {
        Some("\"e1\"") => reply("304 Not Modified", &[], b""),
        _ => reply(
            "200 OK",
            &["etag: \"e1\"", "content-type: application/json"],
            &feed_body(),
        ),
    }))
    .await;
    let source = source(addr);
    let Fetched::Feed { etag, body } = source.fetch(None).await.expect("a feed") else {
        panic!("not a feed");
    };
    assert_eq!(etag.as_deref(), Some("\"e1\""));
    assert_eq!(body, feed_body());
    assert_eq!(
        source.fetch(Some("\"e1\"")).await.expect("answer"),
        Fetched::NotModified
    );
    let seen = seen.lock().unwrap().clone();
    assert_eq!(
        seen[0].line,
        "GET /repos/komira-ai/kbf/releases?per_page=30 HTTP/1.1"
    );
    assert_eq!(
        seen[0].header("accept"),
        Some("application/vnd.github+json")
    );
    assert!(
        seen[0]
            .header("user-agent")
            .is_some_and(|u| u.starts_with("kbf"))
    );
    assert_eq!(seen[0].header("host"), Some(addr.to_string().as_str()));
    assert!(
        seen[0].header("if-none-match").is_none(),
        "a first request is not conditional"
    );
    assert_eq!(seen[1].header("if-none-match"), Some("\"e1\""));
    for request in &seen {
        for (name, _) in &request.headers {
            assert!(
                !["authorization", "cookie", "proxy-authorization"].contains(&name.as_str()),
                "a credential header {name}"
            );
        }
    }
}

/// A fake that answers each request with the next of `replies` (the last repeats).
async fn scripted(replies: Vec<Vec<u8>>) -> (SocketAddr, Arc<Mutex<Vec<Seen>>>) {
    let queue = Mutex::new(std::collections::VecDeque::from(replies));
    fake(handler(move |_| {
        let mut queue = queue.lock().unwrap();
        if queue.len() > 1 {
            queue.pop_front().unwrap()
        } else {
            queue.front().cloned().unwrap_or_default()
        }
    }))
    .await
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

// Catches: each status read as another kind of failure: a rate limit (429, or 403 with none
// left) not asking for a wait, a plain 403 or a 5xx taken for one, a redirect followed (the
// API is not to be sent elsewhere), and a reset time already past taken as a wait.
#[tokio::test]
async fn statuses_become_the_failure_that_backs_off_correctly() {
    let soon = format!("x-ratelimit-reset: {}", now_secs() + 600);
    let past = format!("x-ratelimit-reset: {}", now_secs() - 600);
    let (addr, _) = scripted(vec![
        reply("429 Too Many Requests", &["retry-after: 7"], b""),
        reply("403 Forbidden", &["x-ratelimit-remaining: 0", &soon], b""),
        reply("403 Forbidden", &["x-ratelimit-remaining: 0", &past], b""),
        reply("403 Forbidden", &["x-ratelimit-remaining: 0"], b""),
        reply("403 Forbidden", &["x-ratelimit-remaining: 12"], b""),
        reply("403 Forbidden", &[], b""),
        reply("500 Internal Server Error", &[], b""),
        reply(
            "301 Moved Permanently",
            &["location: http://127.0.0.1:1/elsewhere"],
            b"",
        ),
        reply("404 Not Found", &[], b"{}"),
    ])
    .await;
    let source = source(addr);
    let mut got = Vec::new();
    for _ in 0..9 {
        got.push(source.fetch(None).await.expect_err("a failure"));
    }
    let FetchError::RateLimited(Some(reset)) = got[1] else {
        panic!("{:?}", got[1]);
    };
    assert!((590..=600).contains(&reset.as_secs()), "{reset:?}");
    assert_eq!(
        got[0],
        FetchError::RateLimited(Some(Duration::from_secs(7)))
    );
    assert_eq!(got[2], FetchError::RateLimited(None));
    assert_eq!(got[3], FetchError::RateLimited(None));
    assert_eq!(
        &got[4..],
        [
            FetchError::Status(403),
            FetchError::Status(403),
            FetchError::Status(500),
            FetchError::Status(301),
            FetchError::Status(404),
        ]
    );
}

// Catches: a feed larger than the bound being read whole, a connection that fails or answers
// nothing taking a poll forever (the timeout), and a body cut short being taken for a feed.
#[tokio::test]
async fn a_huge_a_silent_a_cut_and_a_refused_answer_each_fail() {
    let (addr, _) = scripted(vec![reply("200 OK", &[], &vec![b' '; 4 * 1024 * 1024 + 1])]).await;
    assert_eq!(source(addr).fetch(None).await, Err(FetchError::TooLarge));

    // Headers promise ten bytes and the connection ends after three.
    let (addr, _) = scripted(vec![
        b"HTTP/1.1 200 OK\r\ncontent-length: 10\r\n\r\nabc".to_vec(),
    ])
    .await;
    let got = source(addr).fetch(None).await;
    assert!(
        matches!(got, Err(FetchError::Transport(ref why)) if why.starts_with("body:")),
        "{got:?}"
    );

    // Accepts and says nothing.
    let silent = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = silent.local_addr().unwrap();
    tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((stream, _)) = silent.accept().await {
            held.push(stream);
        }
    });
    let quick = GithubSource::new(
        &format!("http://{addr}"),
        "a/b",
        Duration::from_millis(100),
        Roots::System,
    )
    .unwrap();
    assert_eq!(quick.fetch(None).await, Err(FetchError::Timeout));

    // Nothing listens.
    let closed = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = closed.local_addr().unwrap();
    drop(closed);
    let got = source(addr).fetch(None).await;
    assert!(
        matches!(got, Err(FetchError::Transport(ref why)) if why.starts_with("connect:")),
        "{got:?}"
    );

    // Not HTTP.
    let (addr, _) = scripted(vec![b"garbage\r\n\r\n".to_vec()]).await;
    assert!(matches!(
        source(addr).fetch(None).await,
        Err(FetchError::Transport(_))
    ));
}

// Catches: a base URL that is not http(s) with a host being accepted (a file or ftp URL, a
// bare path) so the poller starts and fails at every poll.
#[test]
fn only_an_http_or_https_url_with_a_host_is_a_source() {
    let make = |u: &str| GithubSource::new(u, "a/b", Duration::from_secs(1), Roots::System);
    assert!(make("https://api.github.com").is_ok());
    assert!(make("http://127.0.0.1:1").is_ok());
    for bad in [
        "ftp://x",
        "file:///etc",
        "/path",
        "api.github.com",
        "http://",
        "not a url",
    ] {
        assert!(make(bad).is_err(), "{bad:?}");
    }
}

/// A TLS fake for `localhost` with its own certificate, and that certificate.
async fn fake_tls(
    replies: Vec<Vec<u8>>,
) -> (
    SocketAddr,
    rustls::pki_types::CertificateDer<'static>,
    Arc<Mutex<Vec<Seen>>>,
) {
    let key = rcgen::generate_simple_self_signed(vec!["localhost".to_owned()]).expect("cert");
    let cert = key.cert.der().clone();
    let private = rustls::pki_types::PrivateKeyDer::try_from(key.signing_key.serialize_der())
        .expect("a PKCS#8 key");
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .expect("versions")
        .with_no_client_auth()
        .with_single_cert(vec![cert.clone()], private)
        .expect("a server config");
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&seen);
    let queue = Arc::new(Mutex::new(std::collections::VecDeque::from(replies)));
    tokio::spawn(async move {
        while let Ok((tcp, _)) = listener.accept().await {
            let (acceptor, log, queue) = (acceptor.clone(), Arc::clone(&log), Arc::clone(&queue));
            tokio::spawn(async move {
                let Ok(stream) = acceptor.accept(tcp).await else {
                    return;
                };
                let next = move |_: &Seen| queue.lock().unwrap().pop_front().unwrap_or_default();
                answer(stream, &handler(next), &log).await;
            });
        }
    });
    (addr, cert, seen)
}

// Catches: TLS not being used for https (the request in the clear), a server whose
// certificate the roots do not name being trusted, no roots at all being taken for "trust
// everything", and the handshake's hostname not being the URL's.
#[tokio::test]
async fn https_trusts_only_the_given_roots() {
    let (addr, cert, seen) = fake_tls(vec![
        reply("200 OK", &["etag: \"t\""], b"[]"),
        reply("200 OK", &[], b"[]"),
    ])
    .await;
    let url = format!("https://localhost:{}", addr.port());
    let trusting = GithubSource::new(
        &url,
        "komira-ai/kbf",
        Duration::from_secs(5),
        Roots::Given(vec![cert]),
    )
    .unwrap();
    let Fetched::Feed { etag, body } = trusting.fetch(None).await.expect("over TLS") else {
        panic!("not a feed");
    };
    assert_eq!(
        (etag.as_deref(), body.as_slice()),
        (Some("\"t\""), b"[]".as_slice())
    );
    assert_eq!(seen.lock().unwrap().len(), 1);

    let other =
        rcgen::generate_simple_self_signed(vec!["localhost".to_owned()]).expect("another cert");
    let wrong = GithubSource::new(
        &url,
        "a/b",
        Duration::from_secs(5),
        Roots::Given(vec![other.cert.der().clone()]),
    )
    .unwrap();
    let got = wrong.fetch(None).await;
    assert!(
        matches!(got, Err(FetchError::Transport(ref why)) if why.starts_with("TLS:")),
        "{got:?}"
    );

    let none = GithubSource::new(
        &url,
        "a/b",
        Duration::from_secs(5),
        Roots::Given(Vec::new()),
    )
    .unwrap();
    let got = none.fetch(None).await;
    assert!(
        matches!(got, Err(FetchError::Transport(ref why)) if why.contains("no trusted root")),
        "{got:?}"
    );
}

fn config(addr: SocketAddr) -> Config {
    Config {
        every: Duration::from_secs(300),
        repo: "komira-ai/kbf".to_owned(),
        refs: vec!["main".to_owned()],
        api_url: format!("http://{addr}"),
        timeout: Duration::from_secs(5),
        running: "cccccccccccc".to_owned(),
    }
}

// Catches: the pieces not fitting: the real client's ETag round trip feeding the poller (a
// second poll on an unchanged feed is a 304 and writes nothing), and the proposal the poller
// writes being the one `Rows` lists.
#[tokio::test]
async fn the_poller_reads_the_fake_api_and_a_second_poll_is_a_304() {
    let (addr, seen) = fake(handler(|r| match r.header("if-none-match") {
        Some("\"e1\"") => reply("304 Not Modified", &[], b""),
        _ => reply("200 OK", &["etag: \"e1\""], &feed_body()),
    }))
    .await;
    let store: Arc<dyn Store> = Arc::new(Fake::with_flaw(
        Arc::new(ManualClock::default()),
        Flaw::None,
    ));
    let rows = Rows::new(Arc::clone(&store));
    let stats = Arc::new(Stats::default());
    let mut poller = Poller::new(source(addr), rows.clone(), config(addr), Arc::clone(&stats));
    let now = FarmTime::from_millis(1_792_886_400_000);
    let first = poller.cycle(now, || true).await;
    assert!(
        matches!(first, Cycle::Proposed(ref p) if p.commit == C && p.tag == "v0.9.1"),
        "{first:?}"
    );
    let txns = store.commit_stats().txns;
    assert_eq!(poller.cycle(now, || true).await, Cycle::NotModified);
    assert_eq!(store.commit_stats().txns, txns);
    assert_eq!(rows.proposals().unwrap().len(), 1);
    assert_eq!(seen.lock().unwrap().len(), 2);
    assert_eq!(
        (
            stats.polls(),
            stats.proposals(),
            stats.not_modified(),
            stats.errors()
        ),
        (2, 1, 1, 0)
    );
}

fn listeners() -> Listeners {
    let loopback = SocketAddr::from(([127, 0, 0, 1], 0));
    Listeners {
        reapi: loopback,
        worker: loopback,
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
    }
}

/// Starts a server over a state store and a release store with `poll`; its API address and
/// the stop switch (dropping it stops the server).
fn serve(poll: Option<Config>, tag: &str) -> (SocketAddr, tokio::sync::oneshot::Sender<()>) {
    let dir = std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("kbf-release-poll-{}-{tag}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("a directory");
    let releases =
        Arc::new(ReleaseStore::open(&dir.join("releases"), Limits::default()).expect("open"));
    let store: Arc<dyn Store> = Arc::new(Fake::with_flaw(
        Arc::new(ManualClock::default()),
        Flaw::None,
    ));
    let state = StateStore {
        store,
        epoch: EpochPolicy::Keep,
        members: kbf_server::members::MembershipConfig {
            ttl: Duration::from_secs(30),
            dwell: Duration::from_secs(120),
            replication: 1,
        },
        history: None,
    };
    let api = Api {
        listen: SocketAddr::from(([127, 0, 0, 1], 0)),
        token: None,
        local_cache: None,
        releases: Some(releases),
        poll,
    };
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let bound = bind_server_with_state(
        Arc::new(kbf_front::Cache::<
            kbf_front::MemoryMetaLog,
            kbf_objstore::MemoryStore,
        >::memory()),
        listeners(),
        Some(api),
        kbf_auth::Policy::allow_all(),
        Some(state),
        async move {
            let _ = stopped.await;
        },
    )
    .expect("bind");
    let api = bound.api.expect("an API address");
    tokio::spawn(async move { bound.serving.await.expect("serve") });
    (api, stop)
}

async fn get(addr: SocketAddr, path: &str) -> String {
    let mut stream = tokio::net::TcpStream::connect(addr).await.expect("connect");
    let head = format!("GET {path} HTTP/1.1\r\nHost: kbf\r\nConnection: close\r\n\r\n");
    stream.write_all(head.as_bytes()).await.expect("write");
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).await.expect("read");
    let text = String::from_utf8_lossy(&raw).into_owned();
    text.split_once("\r\n\r\n")
        .map_or(String::new(), |(_, b)| b.to_owned())
}

// Catches: the poller not started by the server (the config read and dropped), it writing
// where the operator cannot see it, a proposal starting a run (`desired` set), the counters
// missing from `/metrics`, and a server with the poller off still listing the families.
#[tokio::test]
async fn a_server_polls_proposes_in_the_operator_api_and_counts_in_metrics() {
    let (fake_api, seen) = fake(handler(|r| match r.header("if-none-match") {
        Some("\"e1\"") => reply("304 Not Modified", &[], b""),
        _ => reply("200 OK", &["etag: \"e1\""], &feed_body()),
    }))
    .await;
    let mut poll = config(fake_api);
    poll.every = Duration::from_millis(50);
    let (api, _stop) = serve(Some(poll), "on");
    let mut view = serde_json::Value::Null;
    for _ in 0..200 {
        view = serde_json::from_str(&get(api, "/v1/update").await).unwrap_or_default();
        if view["proposals"].as_array().is_some_and(|p| !p.is_empty()) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(view["proposals"][0]["commit"], C, "{view}");
    assert_eq!(view["proposals"][0]["tag"], "v0.9.1");
    assert_eq!(
        view["desired"],
        serde_json::Value::Null,
        "a proposal started a run"
    );
    // Wait for a 304 to be counted, then read the metrics.
    let mut metrics = String::new();
    for _ in 0..200 {
        metrics = get(api, "/metrics").await;
        if !metrics.contains("kbf_release_poll_not_modified_total 0")
            && metrics.contains("kbf_release_poll_not_modified_total")
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        metrics.contains("kbf_release_poll_proposals_total 1"),
        "{metrics}"
    );
    assert!(
        !metrics.contains("kbf_release_poll_not_modified_total 0"),
        "{metrics}"
    );
    assert!(seen.lock().unwrap().len() >= 2);

    let (off, _stop_off) = serve(None, "off");
    assert!(!get(off, "/metrics").await.contains("kbf_release_poll"));
    let view: serde_json::Value = serde_json::from_str(&get(off, "/v1/update").await).unwrap();
    assert_eq!(view["proposals"], json!([]));
}
