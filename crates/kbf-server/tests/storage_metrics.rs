//! `GET /metrics` on a storage node (`--metrics-listen`): `kbf storage` and the storage
//! half of `kbf storage-and-daemon` have no operator API, so the route has a listener of
//! its own, which a farm's scraper reads for the series only a process that holds a cache
//! exports (`kbf_cache_lost_blobs`, `kbf_cache_blob_losses_total`).

use std::net::SocketAddr;
use std::path::Path;

use bytes::Bytes;
use kbf_server::storage_half;
use kbf_server::storage_node::{
    COLD_RELEASE_DEADLINE, StorageFlags, command, parse, start_storage,
};
use kbf_storage::Storage as _;
use kbf_storage::service::RemoteStorage;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpStream;

/// A new, empty directory for one test, removed when it drops: bind it before the node
/// that holds files in it, so it is removed after that node is gone.
fn dir(name: &str) -> kbf_testkit::dir::TestDir {
    kbf_testkit::dir::TestDir::under(
        env!("CARGO_TARGET_TMPDIR"),
        &format!("storage-metrics-{name}"),
    )
}

/// The flags of `kbf storage` over new stores in `dir`, plain text on a loopback port, and
/// these more.
fn flags(dir: &Path, more: &[&str]) -> StorageFlags {
    let mut argv = vec![
        "storage".to_owned(),
        "--storage-listen=127.0.0.1:0".to_owned(),
        format!("--cold-store=file://{}", dir.join("cold").display()),
        "--cold-store-floor-bytes=1".to_owned(),
        format!("--meta-dir={}", dir.join("meta").display()),
    ];
    argv.extend(more.iter().map(|m| (*m).to_owned()));
    parse(&command().try_get_matches_from(argv).expect("flags")).expect("taken")
}

fn never() -> std::io::Result<std::future::Pending<()>> {
    Ok(std::future::pending())
}

/// One `GET path`: the status code, the head and the body.
async fn get(addr: SocketAddr, path: &str) -> (u16, String, String) {
    let mut tcp = TcpStream::connect(addr).await.expect("connect");
    let request = format!("GET {path} HTTP/1.1\r\nHost: kbf\r\nConnection: close\r\n\r\n");
    tcp.write_all(request.as_bytes()).await.expect("send");
    let mut response = String::new();
    tcp.read_to_string(&mut response).await.expect("read");
    let (head, body) = response.split_once("\r\n\r\n").expect("a head and a body");
    let code = head
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .expect("a status code");
    (code, head.to_ascii_lowercase(), body.to_owned())
}

/// What a scrape of the lost-blob families and the in-flight gauges must hold, whoever
/// serves it.
fn assert_lost_blob_series(body: &str) {
    for line in [
        "# TYPE kbf_cache_lost_blobs gauge",
        "kbf_cache_lost_blobs 0",
        "# TYPE kbf_cache_blob_losses_total counter",
        "kbf_cache_blob_losses_total 0",
        // The bytes the node holds for requests in flight: a storage node's Writes are
        // counted in the process gauge it exports.
        "# TYPE kbf_bytes_in_flight gauge",
        "# TYPE kbf_bytes_in_flight_peak gauge",
    ] {
        assert!(body.lines().any(|l| l == line), "{line:?} in\n{body}");
    }
}

/// Catches a storage node with no `/metrics` (the harness's lost-blob gates would fail
/// closed on every farm), an answer that is not the Prometheus text format, a flag that
/// is parsed and then ignored (no listener at the address), and the lost-blob families
/// left out of the route (the scrape would answer 200 without them).
#[tokio::test]
async fn a_storage_node_serves_the_lost_blob_series_on_its_metrics_listener() {
    let node_dir = dir("node");
    let flags = flags(&node_dir, &["--metrics-listen=127.0.0.1:0"]);
    let node = start_storage(&flags, never).await.expect("starts");
    let addr = node.metrics.expect("a metrics address");
    assert!(addr.ip().is_loopback() && addr.port() != 0, "{addr}");
    assert_ne!(addr, node.addr, "its own listener");
    let (code, head, body) = get(addr, "/metrics").await;
    assert_eq!(code, 200, "{head}\n{body}");
    assert!(
        head.contains("content-type: text/plain; version=0.0.4; charset=utf-8"),
        "{head}"
    );
    assert_lost_blob_series(&body);
    // The route is /metrics and nothing else: the node has no operator API.
    assert_eq!(get(addr, "/v1/nodes").await.0, 404);
    // Still scrapeable while the node serves, and after the handle moved into `serve`.
    tokio::spawn(node.serve());
    assert_lost_blob_series(&get(addr, "/metrics").await.2);
}

/// Catches the node's metrics listener served around `serve_api` (the handler reads the
/// gate's `Caller`, so a listener without the `gate_requests` layer answers 500), and one
/// given a proxy secret (the node is not behind the proxy, so a loopback call carrying
/// none would be refused with 403). Catches: serve the routes with plain `axum::serve`;
/// pass `Some(secret)` to `serve_api`.
#[tokio::test]
async fn the_metrics_listener_admits_a_direct_loopback_call_through_the_gate() {
    let gate_dir = dir("gate");
    let flags = flags(&gate_dir, &["--metrics-listen=127.0.0.1:0"]);
    let node = start_storage(&flags, never).await.expect("starts");
    let addr = node.metrics.expect("a metrics address");
    assert!(addr.ip().is_loopback(), "{addr}");
    let (code, head, body) = get(addr, "/metrics").await;
    assert_eq!(code, 200, "{head}\n{body}");
    assert_lost_blob_series(&body);
}

/// Catches a listener that is bound without the flag (a farm would grow a port nobody
/// asked for), and a default address other than none.
#[tokio::test]
async fn without_the_flag_there_is_no_metrics_listener() {
    let off_dir = dir("off");
    let flags = flags(&off_dir, &[]);
    assert_eq!(flags.metrics, None);
    let node = start_storage(&flags, never).await.expect("starts");
    assert_eq!(node.metrics, None);
}

/// Catches an address in use that is skipped (the node would start without the series
/// its farm gates on) rather than failing the start, naming the address.
#[tokio::test]
async fn a_metrics_address_in_use_fails_the_start() {
    let taken = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = taken.local_addr().expect("addr");
    let taken_dir = dir("taken");
    let flags = flags(&taken_dir, &[&format!("--metrics-listen={addr}")]);
    let refused = start_storage(&flags, never).await.err().expect("refused");
    assert!(refused.to_string().contains(&addr.to_string()), "{refused}");
}

/// Catches `--metrics-listen` missing from the storage half of `kbf storage-and-daemon`,
/// which starts from the same flags.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_storage_half_serves_the_same_series() {
    let half_dir = dir("half");
    let flags = flags(&half_dir, &["--metrics-listen=127.0.0.1:0"]);
    let half = storage_half::start(&flags, never).await.expect("starts");
    let addr = half.metrics.expect("a metrics address");
    let (code, head, body) = get(addr, "/metrics").await;
    assert_eq!(code, 200, "{head}\n{body}");
    assert_lost_blob_series(&body);
    let off_dir = dir("half-off");
    let off = self::flags(&off_dir, &[]);
    let half = storage_half::start(&off, never).await.expect("starts");
    assert_eq!(half.metrics, None);
}

/// Catches: a storage half whose cache is opened on an uncounted cold store (the families
/// stay at zero while the node writes), including a half whose families are missing before
/// its first write: every op has a line at start, and a blob put through the handle moves the
/// put count to at least one and the bytes written to at least the blob's.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_storage_half_counts_what_its_cache_asks_of_the_cold_store() {
    let half_dir = dir("cold-counts");
    let flags = flags(&half_dir, &["--metrics-listen=127.0.0.1:0"]);
    let half = storage_half::start(&flags, never).await.expect("starts");
    let (addr, storage) = (half.metrics.expect("metrics"), half.storage());
    tokio::spawn(half.serve());
    let body = get(addr, "/metrics").await.2;
    for op in ["get", "head", "put", "delete", "list"] {
        let series = format!("kbf_cold_store_requests_total{{op=\"{op}\"}}");
        assert!(
            body.lines().any(|l| l.starts_with(&series)),
            "{series} in\n{body}"
        );
    }
    let blob = Bytes::from_static(b"a blob the node writes to its cold store");
    storage.put(blob.clone()).await.expect("put");
    let body = get(addr, "/metrics").await.2;
    let put = sample(&body, "kbf_cold_store_requests_total{op=\"put\"}");
    assert!(put.is_some_and(|put| put >= 1.0), "{body}");
    let written = sample(&body, "kbf_cold_store_bytes_total{dir=\"write\"}");
    assert!(written.is_some_and(|w| w >= blob.len() as f64), "{body}");
}

/// Catches: a storage node (`kbf storage`) whose cache is opened on an uncounted cold store (the families
/// stay at zero while the node writes), including a node whose families are missing before
/// its first write: every op has a line at start, and a blob put through the handle moves the
/// put count to at least one and the bytes written to at least the blob's.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_storage_node_counts_what_its_cache_asks_of_the_cold_store() {
    let node_dir = dir("node-cold-counts");
    let flags = flags(&node_dir, &["--metrics-listen=127.0.0.1:0"]);
    let node = start_storage(&flags, never).await.expect("starts");
    let (addr, served) = (node.metrics.expect("metrics"), node.addr);
    tokio::spawn(node.serve());
    let channel = tonic::transport::Endpoint::from_shared(format!("http://{served}"))
        .expect("endpoint")
        .connect()
        .await
        .expect("connect");
    let storage = RemoteStorage::new(channel, 0);
    let body = get(addr, "/metrics").await.2;
    for op in ["get", "head", "put", "delete", "list"] {
        let series = format!("kbf_cold_store_requests_total{{op=\"{op}\"}}");
        assert!(
            body.lines().any(|l| l.starts_with(&series)),
            "{series} in\n{body}"
        );
    }
    let blob = Bytes::from_static(b"a blob the node writes to its cold store");
    storage.put(blob.clone()).await.expect("put");
    let body = get(addr, "/metrics").await.2;
    let put = sample(&body, "kbf_cold_store_requests_total{op=\"put\"}");
    assert!(put.is_some_and(|put| put >= 1.0), "{body}");
    let written = sample(&body, "kbf_cold_store_bytes_total{dir=\"write\"}");
    assert!(written.is_some_and(|w| w >= blob.len() as f64), "{body}");
}

/// The five rows of `kbf_storage_legacy_read_total` in a scrape, in the order bytestream,
/// segments, cas in-process, action store, ac in-process.
fn legacy_rows(body: &str) -> Vec<u64> {
    [
        r#"{kind="cas",source="bytestream"}"#,
        r#"{kind="cas",source="segments"}"#,
        r#"{kind="cas",source="in_process"}"#,
        r#"{kind="ac",source="action_store"}"#,
        r#"{kind="ac",source="in_process"}"#,
    ]
    .iter()
    .map(|labels| {
        let name = format!("kbf_storage_legacy_read_total{labels}");
        sample(body, &name).unwrap_or_else(|| panic!("no {name} in\n{body}")) as u64
    })
    .collect()
}

/// Catches a legacy-read counter that is not the one the node's scrape renders (a fresh
/// handle given to the service, or to the daemon half's storage, would leave the scrape at
/// 0), and a daemon half of a node without a frontend that reads around the counter. The
/// node is the storage half of `kbf storage-and-daemon`, started as the binary starts it:
/// a blob read over gRPC moves `cas/bytestream` to 1 and nothing else; the same blob read
/// through the handle the daemon half uses moves `cas/in_process` to 1; a read of a blob
/// the node lacks moves neither.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_scrape_counts_the_reads_over_the_wire_and_the_daemon_halfs() {
    let half_dir = dir("legacy-reads");
    let flags = flags(&half_dir, &["--metrics-listen=127.0.0.1:0"]);
    let half = storage_half::start(&flags, never).await.expect("starts");
    let (metrics, addr, storage) = (half.metrics.expect("metrics"), half.addr, half.storage());
    tokio::spawn(half.serve());
    let channel = tonic::transport::Endpoint::from_shared(format!("http://{addr}"))
        .expect("endpoint")
        .connect()
        .await
        .expect("connect");
    let remote = RemoteStorage::new(channel, 0);
    let body = Bytes::from_static(b"a blob the node holds");
    let digest = remote.put(body.clone()).await.expect("put");
    assert_eq!(legacy_rows(&get(metrics, "/metrics").await.2), [0; 5]);

    assert_eq!(remote.read_blob(&digest).await.expect("read"), body);
    assert_eq!(
        legacy_rows(&get(metrics, "/metrics").await.2),
        [1, 0, 0, 0, 0]
    );

    assert_eq!(storage.read_blob(&digest).await.expect("read"), body);
    assert_eq!(
        legacy_rows(&get(metrics, "/metrics").await.2),
        [1, 0, 1, 0, 0]
    );

    // The daemon half of a node without a frontend reads through a `LocalStore` over it.
    let local = kbf_server::LocalStore::over(std::sync::Arc::clone(&storage));
    assert_eq!(local.read_blob(&digest).await.expect("read"), body.to_vec());
    assert_eq!(
        legacy_rows(&get(metrics, "/metrics").await.2),
        [1, 0, 2, 0, 0]
    );

    let absent = kbf_segments::sha256(b"a blob nobody put");
    assert!(remote.read_blob(&absent).await.is_err());
    assert!(storage.read_blob(&absent).await.is_err());
    assert!(local.read_blob(&absent).await.is_err());
    assert_eq!(
        legacy_rows(&get(metrics, "/metrics").await.2),
        [1, 0, 2, 0, 0]
    );
}

/// The value of the sample `name` in a scrape.
fn sample(body: &str, name: &str) -> Option<f64> {
    body.lines()
        .find_map(|l| l.strip_prefix(name)?.strip_prefix(' ')?.parse().ok())
}

/// A first life of a storage node over `d` with the write tier `hot`: it starts, takes a
/// blob into the tier over a channel, is told to stop, and `serve()` returns. The stop
/// flushes the tier to the cold store, so the blob ends the life flushed.
async fn first_life(d: &Path, hot: &str) {
    first_life_with(d, &[hot], || {}).await;
}

/// Sets the permission bits of `path`.
fn set_mode(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt as _;
    let perms = std::fs::Permissions::from_mode(mode);
    std::fs::set_permissions(path, perms).expect("mode");
}

/// [`first_life`] over the flags `more`, with `before_stop` run after the blob is put and
/// before the node is told to stop.
async fn first_life_with(d: &Path, more: &[&str], before_stop: impl FnOnce()) {
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let first = start_storage(&flags(d, more), || {
        Ok(async move {
            let _ = stopped.await;
        })
    })
    .await
    .expect("starts");
    let addr = first.addr;
    let serving = tokio::spawn(first.serve_checked(COLD_RELEASE_DEADLINE));
    let channel = tonic::transport::Endpoint::from_shared(format!("http://{addr}"))
        .expect("endpoint")
        .connect()
        .await
        .expect("connect");
    RemoteStorage::new(channel, 0)
        .put(Bytes::from_static(b"staged on the tier, never flushed"))
        .await
        .expect("put");
    before_stop();
    stop.send(()).expect("stop");
    let (served, released) = serving.await.expect("task");
    served.expect("serves to the end");
    // The same bounded wait as the node's: a handle leaked fails here, it does not hang.
    released.expect("the cold store is released within the deadline");
}

/// Catches a `/metrics` route that renders another cache than the node's, or a fresh one
/// (the series present, always 0): a blob written to the node's write tier, whose tier is
/// then abandoned at the next start (the loss the cache records itself), must show in
/// that node's scrape as a lost blob and a loss. A stop flushes the tier to the cold
/// store, so the first life must not be able to: it writes one blob to the hot store
/// (which seals after thirty seconds, so the flusher has not touched it), then makes the
/// cold store's `tmp` directory read-only, which refuses every flush with a permission
/// error (a flush creates its file there), and stops with `--ram-drain-deadline=1`: the
/// flush fails until the one second ends, and the blob is left on the tier. The mode is
/// restored before the second start, which abandons that tier. Needs a user the mode
/// binds (not root). Catches: the route renders `Losses::default()`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_blob_the_nodes_cache_lost_is_scraped_from_its_metrics_listener() {
    let d = dir("loss");
    let hot = format!("--hot-store=file://{}", d.join("hot").display());
    let cold_tmp = d.join("cold/tmp");
    let more = [hot.as_str(), "--ram-drain-deadline=1"];

    first_life_with(&d, &more, || set_mode(&cold_tmp, 0o555)).await;
    set_mode(&cold_tmp, 0o755);

    let tier = std::fs::read_to_string(d.join("hot/TIER")).expect("the tier file");
    let second = flags(
        &d,
        &[
            &hot,
            &format!("--hot-store-abandon={}", tier.trim()),
            "--metrics-listen=127.0.0.1:0",
        ],
    );
    let node = start_storage(&second, never).await.expect("restarts");
    let body = get(node.metrics.expect("a metrics address"), "/metrics")
        .await
        .2;
    let gauge = sample(&body, "kbf_cache_lost_blobs");
    let counter = sample(&body, "kbf_cache_blob_losses_total");
    assert!(gauge >= Some(1.0), "{body}");
    assert!(counter >= Some(1.0), "{body}");
}

/// Catches a `serve()` that returns while something of the node still holds its cold
/// store: the cache's own touch flusher (aborted when the cache drops, gone when it is
/// next polled) and its write-pool threads (they end after the queue closes) hold the
/// writer, and so the directory's lock, past the return. A restart on the same
/// directories failed `Locked { WouldBlock }`. Fifty lives, each restarted the moment
/// `serve()` returns, with no clock in the test. Catches: `StorageHandle::serve` does
/// not wait for `cold_released` (7 of 10 runs red).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_returned_serve_has_released_the_stores() {
    for life in 0..50 {
        let d = dir(&format!("restart-{life}"));
        let hot = format!("--hot-store=file://{}", d.join("hot").display());
        first_life(&d, &hot).await;
        let tier = std::fs::read_to_string(d.join("hot/TIER")).expect("the tier file");
        let second = flags(&d, &[&hot, &format!("--hot-store-abandon={}", tier.trim())]);
        start_storage(&second, never)
            .await
            .unwrap_or_else(|e| panic!("life {life} restarts: {e}"));
    }
}
