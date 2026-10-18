//! `GET /metrics` on a storage node (`--metrics-listen`): `kbf storage` and the storage
//! half of `kbf storage-and-daemon` have no operator API, so the route has a listener of
//! its own, which a farm's scraper reads for the series only a process that holds a cache
//! exports (`kbf_cache_lost_blobs`, `kbf_cache_blob_losses_total`).

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use bytes::Bytes;
use kbf_server::storage_half;
use kbf_server::storage_node::{StorageFlags, command, parse, start_storage};
use kbf_storage::Storage as _;
use kbf_storage::service::RemoteStorage;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpStream;

fn dir(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("storage-metrics")
        .join(format!("{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("directory");
    dir
}

/// The flags of `kbf storage` over new stores in `dir`, plain text on a loopback port, and
/// these more.
fn flags(dir: &Path, more: &[&str]) -> StorageFlags {
    let mut argv = vec![
        "storage".to_owned(),
        "--storage-listen=127.0.0.1:0".to_owned(),
        format!("--cold-store=file://{}", dir.join("cold").display()),
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

/// What a scrape of the lost-blob families must hold, whoever serves it.
fn assert_lost_blob_series(body: &str) {
    for line in [
        "# TYPE kbf_cache_lost_blobs gauge",
        "kbf_cache_lost_blobs 0",
        "# TYPE kbf_cache_blob_losses_total counter",
        "kbf_cache_blob_losses_total 0",
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
    let flags = flags(&dir("node"), &["--metrics-listen=127.0.0.1:0"]);
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

/// Catches a listener that is bound without the flag (a farm would grow a port nobody
/// asked for), and a default address other than none.
#[tokio::test]
async fn without_the_flag_there_is_no_metrics_listener() {
    let flags = flags(&dir("off"), &[]);
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
    let flags = flags(&dir("taken"), &[&format!("--metrics-listen={addr}")]);
    let refused = start_storage(&flags, never).await.err().expect("refused");
    assert!(refused.to_string().contains(&addr.to_string()), "{refused}");
}

/// Catches `--metrics-listen` missing from the storage half of `kbf storage-and-daemon`,
/// which starts from the same flags.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_storage_half_serves_the_same_series() {
    let flags = flags(&dir("half"), &["--metrics-listen=127.0.0.1:0"]);
    let half = storage_half::start(&flags, never).await.expect("starts");
    let addr = half.metrics.expect("a metrics address");
    let (code, head, body) = get(addr, "/metrics").await;
    assert_eq!(code, 200, "{head}\n{body}");
    assert_lost_blob_series(&body);
    let off = self::flags(&dir("half-off"), &[]);
    let half = storage_half::start(&off, never).await.expect("starts");
    assert_eq!(half.metrics, None);
}

/// The value of the sample `name` in a scrape.
fn sample(body: &str, name: &str) -> Option<f64> {
    body.lines()
        .find_map(|l| l.strip_prefix(name)?.strip_prefix(' ')?.parse().ok())
}

/// Catches a `/metrics` route that renders another cache than the node's, or a fresh one
/// (the series present, always 0): a blob written to the node's write tier, whose tier is
/// then abandoned at the next start (the loss the cache records itself), must show in
/// that node's scrape as a lost blob and a loss. The first life writes one blob to the
/// hot store, which seals after thirty seconds and so is still unflushed when the life
/// ends; the second start abandons that tier. Mutant: the route renders `Losses::default()`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_blob_the_nodes_cache_lost_is_scraped_from_its_metrics_listener() {
    let d = dir("loss");
    let hot = format!("--hot-store=file://{}", d.join("hot").display());

    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let first = start_storage(&flags(&d, &[&hot]), || {
        Ok(async move {
            let _ = stopped.await;
        })
    })
    .await
    .expect("starts");
    let addr = first.addr;
    let serving = tokio::spawn(first.serve());
    let channel = tonic::transport::Endpoint::from_shared(format!("http://{addr}"))
        .expect("endpoint")
        .connect()
        .await
        .expect("connect");
    RemoteStorage::new(channel, 0)
        .put(Bytes::from_static(b"staged on the tier, never flushed"))
        .await
        .expect("put");
    stop.send(()).expect("stop");
    serving.await.expect("task").expect("serves to the end");

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
