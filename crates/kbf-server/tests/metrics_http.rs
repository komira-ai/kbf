//! `GET /metrics` on the operator API listener over a real listener: the Prometheus
//! text answer, and the upload-path families moving when a client uploads, asks and
//! reads over the REAPI listener (`docs/api.md#get-metrics`).

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use futures::stream;
use kbf_front::Cache;
use kbf_proto::google::bytestream::byte_stream_client::ByteStreamClient;
use kbf_proto::google::bytestream::{ReadRequest, WriteRequest};
use kbf_proto::reapi::FindMissingBlobsRequest;
use kbf_proto::reapi::content_addressable_storage_client::ContentAddressableStorageClient;
use kbf_server::{Api, Listeners, bind_server_with_api};
use sha2::Digest as _;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tonic::transport::{Channel, Endpoint};

fn loopback() -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], 0))
}

/// A server with the operator API in this process; its REAPI and API addresses.
fn start() -> (SocketAddr, SocketAddr) {
    let listeners = Listeners {
        reapi: loopback(),
        worker: loopback(),
        reapi_tls: None,
        worker_tls: None,
        heartbeat_interval: Duration::from_millis(200),
        hello_wait: Duration::from_secs(5),
        tick: Duration::from_millis(50),
        unservable_wait: kbf_sched::UNSERVABLE_WAIT,
        finished_retention: kbf_sched::FINISHED_RETENTION,
        shutdown_timeout: Duration::from_secs(10),
        store_probe_timeout: kbf_server::health::STORE_PROBE_TIMEOUT,
        invocation_memory: kbf_front::DEFAULT_INVOCATION_MEMORY,
    };
    let api = Api {
        listen: loopback(),
        token: None,
    };
    let cache = Arc::new(Cache::memory());
    let bound =
        bind_server_with_api(cache, listeners, Some(api), std::future::pending()).expect("bind");
    let (reapi, api) = (bound.reapi, bound.api.expect("an API address"));
    tokio::spawn(async move { bound.serving.await.expect("serve") });
    (reapi, api)
}

/// One `GET /metrics`: the status code, the head and the body.
async fn scrape(api: SocketAddr) -> (u16, String, String) {
    let mut tcp = TcpStream::connect(api).await.expect("connect");
    let request = "GET /metrics HTTP/1.1\r\nHost: kbf\r\nConnection: close\r\n\r\n";
    tcp.write_all(request.as_bytes()).await.expect("send");
    let mut response = String::new();
    tcp.read_to_string(&mut response).await.expect("read");
    let (head, body) = response.split_once("\r\n\r\n").expect("a head and a body");
    let code = head
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .expect("a status code");
    (code, head.to_owned(), body.to_owned())
}

/// The value of the sample line `series` (name and labels) in a scrape's `body`.
fn sample(body: &str, series: &str) -> u64 {
    let values: Vec<&str> = body
        .lines()
        .filter_map(|l| l.strip_prefix(series)?.strip_prefix(' '))
        .collect();
    assert_eq!(values.len(), 1, "{series} once in\n{body}");
    values[0].parse().expect("an integer sample")
}

fn channel(reapi: SocketAddr) -> Channel {
    Endpoint::from_shared(format!("http://{reapi}"))
        .expect("endpoint")
        .connect_lazy()
}

fn hex(bytes: &[u8]) -> String {
    sha2::Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Catches no `/metrics` route on the operator API (404 before this route existed),
/// an answer that is not the Prometheus text format a scraper accepts, and families
/// served from a cache other than the one the REAPI listener stores into (a client's
/// Write, FindMissingBlobs and Read would then not move them).
#[tokio::test]
async fn metrics_count_what_the_reapi_listener_serves() {
    let (reapi, api) = start();
    let (code, head, body) = scrape(api).await;
    assert_eq!(code, 200, "{head}\n{body}");
    let head = head.to_ascii_lowercase();
    assert!(
        head.contains("content-type: text/plain; version=0.0.4; charset=utf-8"),
        "{head}"
    );
    assert!(
        body.contains("# TYPE kbf_bytestream_write_requests_total counter"),
        "{body}"
    );
    assert_eq!(sample(&body, "kbf_bytestream_write_requests_total"), 0);

    let data = b"an output a client uploads".to_vec();
    let name = format!("blobs/{}/{}", hex(&data), data.len());
    let write = WriteRequest {
        resource_name: format!("uploads/metrics/{name}"),
        write_offset: 0,
        finish_write: true,
        data: data.clone(),
    };
    let mut bytestream = ByteStreamClient::new(channel(reapi));
    bytestream
        .write(stream::iter([write]))
        .await
        .expect("Write");
    let find = FindMissingBlobsRequest {
        blob_digests: vec![kbf_proto::reapi::Digest {
            hash: hex(&data),
            size_bytes: i64::try_from(data.len()).expect("small"),
        }],
        ..FindMissingBlobsRequest::default()
    };
    ContentAddressableStorageClient::new(channel(reapi))
        .find_missing_blobs(find)
        .await
        .expect("FindMissingBlobs");
    let read = ReadRequest {
        resource_name: name,
        ..ReadRequest::default()
    };
    let mut chunks = bytestream.read(read).await.expect("Read").into_inner();
    while chunks.message().await.expect("a message").is_some() {}

    let (_, _, body) = scrape(api).await;
    for (series, value) in [
        ("kbf_bytestream_write_requests_total", 1),
        ("kbf_bytestream_write_bytes_total", data.len() as u64),
        ("kbf_bytestream_write_in_flight", 0),
        ("kbf_segment_put_seconds_count", 1),
        ("kbf_segment_footer_read_seconds_count", 1),
        ("kbf_segment_commit_seconds_count", 1),
        ("kbf_segment_blobs_sum", 1),
        ("kbf_meta_commit_seconds_count{command=\"put_blobs\"}", 1),
        ("kbf_find_missing_requests_total", 1),
        ("kbf_find_missing_digests_total", 1),
        ("kbf_find_missing_missing_total", 0),
        ("kbf_object_get_seconds_count", 1),
    ] {
        assert_eq!(sample(&body, series), value, "{series}");
    }
}
