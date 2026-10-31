//! The fault proxy over real loopback sockets: what reaches the store, what each mode does to a
//! request, what the control file and the counters file do. The stores are a hand-written
//! upstream (it answers with bytes the test chose and keeps what it received) and `FakeS3`
//! (which checks the signature). Waits are for events (a counter, a file, a mode), and the
//! delays and caps are read off tokio's paused clock, so no test reads the wall clock.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use clap::Parser;
use kbf_testkit::dir::TestDir;
use objstore_s3::s3::fake::FakeS3;
use tierstore_bench::cli::{self, Cli};
use tierstore_bench::config::{Secret, StoreSpec};
use tierstore_bench::error::BenchError;
use tierstore_bench::proxy::{COUNTERS_FILE, Counters, FaultMode, ProxyConfig, ProxyHandle, spawn};
use tierstore_bench::requestlog::RequestLog;
use tierstore_bench::role::Role;
use tierstore_bench::s3::{BenchS3, Timeouts};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const DATE: &str = "Sun, 06 Nov 1994 08:49:37 GMT";
const LATER: &str = "Sun, 06 Nov 1994 09:49:37 GMT";

/// What the upstream received, one entry per request: the head as text and the body.
type Request = (String, Vec<u8>);

#[derive(Clone, Default)]
struct Seen(Arc<Mutex<Vec<Request>>>);

impl Seen {
    fn all(&self) -> Vec<Request> {
        self.0.lock().unwrap().clone()
    }
}

fn position(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn content_length(head: &str) -> usize {
    head.to_lowercase()
        .lines()
        .find_map(|line| line.strip_prefix("content-length: ")?.trim().parse().ok())
        .unwrap_or(0)
}

/// An upstream that reads one request per connection, keeps it, writes `response` and closes.
async fn upstream(response: Vec<u8>) -> (SocketAddr, Seen) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let seen = Seen::default();
    let keep = seen.clone();
    tokio::spawn(async move {
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            let (response, seen) = (response.clone(), keep.clone());
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                let head_end = loop {
                    if let Some(at) = position(&buf, b"\r\n\r\n") {
                        break at + 4;
                    }
                    let n = socket.read(&mut chunk).await.unwrap();
                    if n == 0 {
                        return;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                };
                let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
                let wanted = head_end + content_length(&head);
                while buf.len() < wanted {
                    let n = socket.read(&mut chunk).await.unwrap();
                    if n == 0 {
                        break;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                }
                seen.0
                    .lock()
                    .unwrap()
                    .push((head, buf[head_end..wanted].to_vec()));
                socket.write_all(&response).await.ok();
                socket.shutdown().await.ok();
            });
        }
    });
    (addr, seen)
}

fn ok(body: &str, extra: &str) -> Vec<u8> {
    format!(
        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n{extra}\r\n{body}",
        body.len()
    )
    .into_bytes()
}

async fn proxy_for(response: Vec<u8>, tweak: impl FnOnce(&mut ProxyConfig)) -> (ProxyHandle, Seen) {
    let (addr, seen) = upstream(response).await;
    (proxy_at(&format!("http://{addr}"), tweak).await, seen)
}

async fn proxy_at(upstream: &str, tweak: impl FnOnce(&mut ProxyConfig)) -> ProxyHandle {
    let mut config = ProxyConfig::new("127.0.0.1:0".parse().unwrap(), upstream);
    config.tick = Duration::from_millis(10);
    tweak(&mut config);
    spawn(config).await.unwrap()
}

/// One request, `Connection: close`, read to the end; what could not be read is empty.
async fn exchange(addr: SocketAddr, request: &str) -> String {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    stream.write_all(request.as_bytes()).await.ok();
    let mut out = Vec::new();
    stream.read_to_end(&mut out).await.ok();
    String::from_utf8_lossy(&out).into_owned()
}

fn get(path: &str) -> String {
    format!("GET {path} HTTP/1.1\r\nHost: sentinel.example:4242\r\nConnection: close\r\n\r\n")
}

async fn wait_until(mut done: impl FnMut() -> bool) {
    while !done() {
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
}

/// Catches a proxy that changes a request on the way: the line, the signed headers and the
/// body reach the store as sent, a hop-by-hop header does not, and the answer comes back.
#[tokio::test]
async fn a_request_reaches_the_store_as_it_was_sent() {
    let (proxy, seen) = proxy_for(ok("hello", ""), |_| {}).await;
    let request = "PUT /bucket/key?partNumber=1 HTTP/1.1\r\nHost: sentinel.example:4242\r\n\
                   Authorization: AWS4-HMAC-SHA256 Credential=x\r\nX-Amz-Content-Sha256: abc\r\n\
                   Content-Length: 5\r\nConnection: close\r\n\r\nworld";
    let answer = exchange(proxy.local_addr(), request).await;
    assert!(answer.starts_with("HTTP/1.1 200 OK"), "{answer}");
    assert!(answer.ends_with("hello"), "{answer}");

    let received = seen.all();
    assert_eq!(received.len(), 1);
    let (head, body) = &received[0];
    assert!(
        head.starts_with("PUT /bucket/key?partNumber=1 HTTP/1.1\r\n"),
        "{head}"
    );
    let lower = head.to_lowercase();
    for header in [
        "host: sentinel.example:4242",
        "authorization: aws4-hmac-sha256 credential=x",
        "x-amz-content-sha256: abc",
    ] {
        assert!(lower.contains(header), "{header} in {head}");
    }
    assert!(!lower.contains("connection:"), "{head}");
    assert_eq!(body, b"world");

    let counters = proxy.stop().await.unwrap();
    assert_eq!(
        (
            counters.requests,
            counters.request_bytes,
            counters.response_bytes
        ),
        (1, 5, 5)
    );
}

/// Catches a proxy that breaks a SigV4 signature: a store that recomputes the signature over
/// the request as received accepts what went through, and still refuses a wrong secret, so
/// the acceptance is the proxy's and not a store that checks nothing.
#[tokio::test]
async fn a_signed_request_is_accepted_by_a_store_that_checks_the_signature() {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "tierstore-bench-proxy-sigv4");
    let log = Arc::new(RequestLog::create(&dir.path().join("requests.jsonl")).unwrap());
    let fake = FakeS3::start().await;
    fake.require_credentials("fake-key", "fake-secret");
    let proxy = proxy_at(fake.endpoint(), |_| {}).await;
    let client = |secret: &str| {
        let spec = StoreSpec {
            endpoint: format!("http://{}", proxy.local_addr()),
            bucket: "kbf-fake".to_owned(),
            region: "us-east-1".to_owned(),
            access_key_id: "fake-key".to_owned(),
            secret_access_key: Secret::new(secret),
        };
        BenchS3::new(Role::Instance, &spec, Timeouts::default(), log.clone()).unwrap()
    };

    let good = client("fake-secret");
    assert!(
        good.put("bench/a", vec![7u8; 4096].into())
            .await
            .unwrap()
            .is_ok()
    );
    assert_eq!(
        good.head("bench/a").await.unwrap().result.unwrap(),
        Some(4096)
    );
    assert_eq!(
        good.get_range("bench/a", 10, 20)
            .await
            .unwrap()
            .result
            .unwrap()
            .len(),
        20
    );
    let wrong = client("other-secret")
        .put("bench/b", vec![1u8].into())
        .await
        .unwrap();
    assert_eq!(wrong.status, Some(403));

    assert_eq!(fake.requests().len(), 4);
    assert_eq!(proxy.request_count(), 4);
}

/// Catches `refuse` that does not refuse: a new connection is closed with no answer, a request
/// on a connection opened before is cut, and the store sees neither.
#[tokio::test]
async fn refuse_closes_new_connections_and_cuts_open_ones() {
    let (proxy, seen) = proxy_for(ok("ok", ""), |_| {}).await;
    let mut open = TcpStream::connect(proxy.local_addr()).await.unwrap();
    let keep_alive = "GET /a HTTP/1.1\r\nHost: h:1\r\n\r\n";
    open.write_all(keep_alive.as_bytes()).await.unwrap();
    let mut got = Vec::new();
    let mut chunk = [0u8; 512];
    while position(&got, b"\r\n\r\nok").is_none() {
        let n = open.read(&mut chunk).await.unwrap();
        assert!(n > 0, "the first answer never came");
        got.extend_from_slice(&chunk[..n]);
    }

    proxy.set_mode(FaultMode::Refuse).unwrap();
    open.write_all(keep_alive.as_bytes()).await.ok();
    let mut rest = Vec::new();
    open.read_to_end(&mut rest).await.ok();
    assert!(rest.is_empty(), "{}", String::from_utf8_lossy(&rest));
    let fresh = exchange(proxy.local_addr(), &get("/b")).await;
    assert!(fresh.is_empty(), "{fresh}");

    assert_eq!(seen.all().len(), 1);
    let counters = proxy.stop().await.unwrap();
    assert_eq!((counters.requests, counters.refused), (1, 2));
}

/// Catches `blackhole` that answers, that closes the connection (which is `refuse`), or that
/// sends the request on: the request is read, counted, held, and the connection is open until
/// the proxy stops.
#[tokio::test]
async fn blackhole_reads_the_request_and_never_answers() {
    let (proxy, seen) = proxy_for(ok("ok", ""), |_| {}).await;
    proxy.set_mode(FaultMode::Blackhole).unwrap();
    let mut client = TcpStream::connect(proxy.local_addr()).await.unwrap();
    client.write_all(get("/a").as_bytes()).await.unwrap();
    wait_until(|| proxy.counters().blackholed == 1).await;
    assert!(seen.all().is_empty());
    assert_eq!(proxy.request_count(), 0);

    let counters = proxy.stop().await.unwrap();
    assert_eq!((counters.blackholed, counters.refused), (1, 0));
    let mut out = Vec::new();
    client.read_to_end(&mut out).await.ok();
    assert!(out.is_empty(), "{}", String::from_utf8_lossy(&out));
}

/// Catches a latency fault that adds nothing: on a paused clock the request cannot finish
/// before the proxy's wait has run out.
#[tokio::test(start_paused = true)]
async fn latency_waits_before_forwarding() {
    let (proxy, seen) = proxy_for(ok("x", ""), |c| c.tick = Duration::from_secs(3600)).await;
    proxy.set_mode(FaultMode::Latency { ms: 600_000 }).unwrap();
    let start = tokio::time::Instant::now();
    let answer = exchange(proxy.local_addr(), &get("/a")).await;
    assert!(answer.ends_with('x'), "{answer}");
    assert!(
        start.elapsed() >= Duration::from_secs(600),
        "{:?}",
        start.elapsed()
    );
    assert_eq!(seen.all().len(), 1);
}

/// Catches a bandwidth cap on one direction only, or none: a body of 3000 bytes up and one of
/// 4000 bytes down at 10 bytes per second cannot finish in less than 700 seconds of the paused
/// clock, and both bodies arrive whole.
#[tokio::test(start_paused = true)]
async fn bandwidth_holds_both_bodies_to_the_rate() {
    let down = "d".repeat(4000);
    let (proxy, seen) = proxy_for(ok(&down, ""), |c| c.tick = Duration::from_secs(3600)).await;
    proxy
        .set_mode(FaultMode::Bandwidth { bytes_per_sec: 10 })
        .unwrap();
    let up = "u".repeat(3000);
    let request = format!(
        "PUT /a HTTP/1.1\r\nHost: h:1\r\nContent-Length: 3000\r\nConnection: close\r\n\r\n{up}"
    );
    let start = tokio::time::Instant::now();
    let answer = exchange(proxy.local_addr(), &request).await;
    assert!(answer.ends_with(&down), "{}", answer.len());
    assert!(
        start.elapsed() >= Duration::from_secs(700),
        "{:?}",
        start.elapsed()
    );
    assert_eq!(seen.all()[0].1, up.as_bytes());
    let counters = proxy.stop().await.unwrap();
    assert_eq!(
        (counters.request_bytes, counters.response_bytes),
        (3000, 4000)
    );
}

/// Catches a date rewrite that is on without being asked, that moves the wrong way, or that
/// is not refused when it was not allowed.
#[tokio::test]
async fn skew_moves_the_dates_of_the_answer_only_when_allowed_and_asked() {
    let dates = format!("Date: {DATE}\r\nLast-Modified: {DATE}\r\n");
    let (proxy, _) = proxy_for(ok("x", &dates), |c| c.allow_date_rewrite = true).await;
    let plain = exchange(proxy.local_addr(), &get("/a"))
        .await
        .to_lowercase();
    assert!(
        plain.contains(&format!("date: {}", DATE.to_lowercase())),
        "{plain}"
    );

    proxy.set_mode(FaultMode::DateSkew { secs: 3600 }).unwrap();
    let skewed = exchange(proxy.local_addr(), &get("/a"))
        .await
        .to_lowercase();
    for header in ["date", "last-modified"] {
        let line = format!("{header}: {}", LATER.to_lowercase());
        assert!(skewed.contains(&line), "{line} in {skewed}");
    }

    let mut config = ProxyConfig::new("127.0.0.1:0".parse().unwrap(), "http://127.0.0.1:9");
    config.mode = FaultMode::DateSkew { secs: 1 };
    assert!(matches!(spawn(config).await, Err(BenchError::Proxy(_))));
}

/// Catches a store that ends an answer early taking the proxy down, or the bytes that did
/// arrive not being counted: the client gets the part and a closed connection, and the next
/// request is served.
#[tokio::test]
async fn an_answer_cut_short_costs_one_connection() {
    let cut = b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\nabc".to_vec();
    let (proxy, _) = proxy_for(cut, |_| {}).await;
    for _ in 0..2 {
        let answer = exchange(proxy.local_addr(), &get("/a")).await;
        assert!(!answer.ends_with("abcdefghij"), "{answer}");
    }
    let counters = proxy.stop().await.unwrap();
    assert_eq!((counters.requests, counters.response_bytes), (2, 6));
}

/// Catches a store that is down being answered for: the client sees a closed connection, and
/// the attempt is counted.
#[tokio::test]
async fn a_store_that_is_down_closes_the_client_connection() {
    let closed = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", closed.local_addr().unwrap());
    drop(closed);
    let proxy = proxy_at(&url, |_| {}).await;
    let answer = exchange(proxy.local_addr(), &get("/a")).await;
    assert!(answer.is_empty(), "{answer}");
    assert_eq!(proxy.request_count(), 1);
}

/// Catches a fault that outlives its time to live: one with none left is gone before the next
/// request, with no tick and no wait.
#[tokio::test]
async fn a_fault_with_no_time_left_is_pass() {
    let (proxy, _) = proxy_for(ok("x", ""), |c| c.tick = Duration::from_secs(3600)).await;
    proxy
        .set_mode_for(FaultMode::Refuse, Some(Duration::ZERO))
        .unwrap();
    assert_eq!(proxy.mode(), FaultMode::Pass);
    let answer = exchange(proxy.local_addr(), &get("/a")).await;
    assert!(answer.ends_with('x'), "{answer}");
    assert_eq!(proxy.counters().refused, 0);
}

/// Catches a control file that is not read while the proxy runs, and a counters file that is
/// not written: the file switches the mode both ways, and the file at the end is what stop
/// returned.
#[tokio::test]
async fn the_control_file_switches_the_mode_and_the_counters_file_is_written() {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "tierstore-bench-proxy-live");
    let out = dir.path().join("out");
    std::fs::create_dir(&out).unwrap();
    let control = dir.path().join("mode");
    let (proxy, _) = proxy_for(ok("x", ""), |c| {
        c.control = Some(control.clone());
        c.result_dir = Some(out.clone());
    })
    .await;
    let switch = |text: &str| {
        let staged = dir.path().join("mode.staged");
        std::fs::write(&staged, text).unwrap();
        std::fs::rename(&staged, &control).unwrap();
    };

    switch("refuse ttl=3600");
    wait_until(|| proxy.mode() == FaultMode::Refuse).await;
    assert!(exchange(proxy.local_addr(), &get("/a")).await.is_empty());
    switch("pass");
    wait_until(|| proxy.mode() == FaultMode::Pass).await;
    assert!(
        exchange(proxy.local_addr(), &get("/a"))
            .await
            .ends_with('x')
    );

    let counters = proxy.stop().await.unwrap();
    assert_eq!(
        (
            counters.mode_changes,
            counters.refused,
            counters.requests,
            counters.control_rejected
        ),
        (3, 1, 1, 0)
    );
    let written: Counters =
        serde_json::from_slice(&std::fs::read(out.join(COUNTERS_FILE)).unwrap()).unwrap();
    assert_eq!(written, counters);
}

/// Catches a listen address that is taken being ignored.
#[tokio::test]
async fn a_listen_address_in_use_is_an_error() {
    let taken = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let config = ProxyConfig::new(taken.local_addr().unwrap(), "http://127.0.0.1:9");
    assert!(matches!(spawn(config).await, Err(BenchError::Io { .. })));
}

/// Catches a verb that does not wait, does not stop on SIGTERM, or does not write its counters
/// on the way out; also the flags reaching the proxy.
#[tokio::test]
async fn the_verb_serves_until_sigterm_and_writes_its_counters() {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "tierstore-bench-proxy-verb");
    let out = dir.path().join("out");
    std::fs::create_dir(&out).unwrap();
    let control = dir.path().join("mode");
    let argv = [
        "tierstore-bench",
        "fault-proxy",
        "--listen",
        "127.0.0.1:0",
        "--upstream",
        "http://127.0.0.1:9",
        "--mode",
        "latency=5",
        "--ttl-secs",
        "3600",
        "--control",
        control.to_str().unwrap(),
        "--result-dir",
        out.to_str().unwrap(),
        "--allow-date-rewrite",
    ];
    let verb = tokio::spawn(cli::run(Cli::try_parse_from(argv).unwrap()));
    let file = out.join(COUNTERS_FILE);
    // The signal handler is registered before the proxy serves, so the file means it is.
    wait_until(|| file.exists()).await;
    let killed = std::process::Command::new("kill")
        .args(["-TERM", &std::process::id().to_string()])
        .status()
        .unwrap();
    assert!(killed.success());
    verb.await.unwrap().unwrap();
    let written: Counters = serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
    assert_eq!(
        (written.mode.as_str(), written.mode_changes),
        ("latency=5", 1)
    );
}
