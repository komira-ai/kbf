//! The cold store's request counts on `GET /metrics` of a server started from its flags
//! (`kbf_server::start`): the families are there before any request, and a blob a client
//! uploads moves the put counts, so the store the cache writes to is the counted one.

use clap::Parser;
use futures::stream;
use kbf_proto::google::bytestream::WriteRequest;
use kbf_proto::google::bytestream::byte_stream_client::ByteStreamClient;
use kbf_server::Args;
use sha2::Digest as _;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpStream;
use tonic::transport::Endpoint;

/// One `GET /metrics` body.
async fn scrape(api: std::net::SocketAddr) -> String {
    let mut tcp = TcpStream::connect(api).await.expect("connect");
    let request = "GET /metrics HTTP/1.1\r\nHost: kbf\r\nConnection: close\r\n\r\n";
    tcp.write_all(request.as_bytes()).await.expect("send");
    let mut response = String::new();
    tcp.read_to_string(&mut response).await.expect("read");
    let (_, body) = response.split_once("\r\n\r\n").expect("a head and a body");
    body.to_owned()
}

/// The value of the sample line `series` in `body`.
fn sample(body: &str, series: &str) -> u64 {
    let line = body
        .lines()
        .find_map(|l| l.strip_prefix(series)?.strip_prefix(' '))
        .unwrap_or_else(|| panic!("{series} in\n{body}"));
    line.parse().expect("an integer sample")
}

/// Catches: a start that leaves the cold store uncounted (the families stay at zero while
/// blobs are written), and families that are missing until the first request: before any
/// upload every op has a line, and after one upload of known size the put count is at least
/// one and the bytes written are at least the blob's.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_started_server_counts_what_its_cache_asks_of_the_cold_store() {
    let dir = kbf_testkit::dir::TestDir::under(env!("CARGO_TARGET_TMPDIR"), "cold-store-metrics");
    let at = |leaf: &str| dir.join(leaf).display().to_string();
    let args = Args::try_parse_from([
        "kbf-server".to_owned(),
        "--listen=127.0.0.1:0".to_owned(),
        "--worker-listen=127.0.0.1:0".to_owned(),
        "--api-listen=127.0.0.1:0".to_owned(),
        format!("--cold-store=file://{}", at("cold")),
        "--cold-store-floor-bytes=1".to_owned(),
        format!("--meta-dir={}", at("meta")),
    ])
    .expect("flags");
    let server = kbf_server::start(&args, || Ok(std::future::pending::<()>()))
        .await
        .expect("starts");
    let (reapi, api) = (server.reapi, server.api.expect("an API address"));
    tokio::spawn(server.serve());

    let body = scrape(api).await;
    for op in ["get", "head", "put", "delete", "list"] {
        let series = format!("kbf_cold_store_errors_total{{op=\"{op}\"}}");
        assert_eq!(sample(&body, &series), 0, "{series}");
    }
    assert_eq!(
        sample(&body, "kbf_cold_store_requests_total{op=\"put\"}"),
        0
    );
    assert_eq!(
        sample(&body, "kbf_cold_store_bytes_total{dir=\"write\"}"),
        0
    );

    let data = b"an output a client uploads".to_vec();
    let hex: String = sha2::Sha256::digest(&data)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let write = WriteRequest {
        resource_name: format!("uploads/cold-metrics/blobs/{hex}/{}", data.len()),
        write_offset: 0,
        finish_write: true,
        data: data.clone(),
    };
    let channel = Endpoint::from_shared(format!("http://{reapi}"))
        .expect("endpoint")
        .connect_lazy();
    ByteStreamClient::new(channel)
        .write(stream::iter([write]))
        .await
        .expect("Write");
    let body = scrape(api).await;
    assert!(
        sample(&body, "kbf_cold_store_requests_total{op=\"put\"}") >= 1,
        "{body}"
    );
    assert!(
        sample(&body, "kbf_cold_store_bytes_total{dir=\"write\"}") >= data.len() as u64,
        "{body}"
    );
}
