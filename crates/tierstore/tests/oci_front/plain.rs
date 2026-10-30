//! The front over plain HTTP on loopback: the routes answer as they do over TLS, and the limits
//! of the listener hold. None of these asserts a time (see `limits`).

use std::net::SocketAddr;

use tokio::net::TcpListener;

use crate::client::Conn;
use crate::limits::{SAFETY, SHORT, hold_an_answer_back, served_within_safety};
use crate::rig::Rig;

const MANIFEST: &str = r#"{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json","config":{},"layers":[]}"#;

/// Asserts the version check over plain HTTP: `GET` and `HEAD` of `/v2/` are 200 with `{}` and
/// the registry version header, and a connection is reused for the next request.
#[tokio::test]
async fn the_ping_answers_over_plain_http() {
    let rig = Rig::start_plain().await;
    let mut conn = rig.conn().await;
    let got = conn.request("GET", "/v2/", &[]).await.expect("an answer");
    assert_eq!(got.status, 200);
    assert_eq!(got.body, b"{}");
    assert_eq!(got.must("docker-distribution-api-version"), "registry/2.0");
    let head = conn.request("HEAD", "/v2/", &[]).await.expect("an answer");
    assert_eq!((head.status, head.body.len(), head.short), (200, 0, false));
    assert_eq!(head.must("content-length"), "2");
    assert_eq!(rig.metrics.requests("ping", "ok"), 2);
    drop(conn);
    assert_eq!(rig.stop().await, 0);
}

/// Asserts a manifest and a blob are served over plain HTTP with the headers a client reads, a
/// range of the blob is 206, and a manifest that is not its digest is never sent.
#[tokio::test]
async fn a_manifest_and_a_blob_are_served_over_plain_http() {
    let rig = Rig::start_plain().await;
    let manifest = rig.seed(MANIFEST.as_bytes()).await;
    let got = rig
        .get(&format!("/v2/library/app/manifests/{manifest}"))
        .await;
    assert_eq!(got.status, 200);
    assert_eq!(
        got.must("content-type"),
        "application/vnd.oci.image.manifest.v1+json"
    );
    assert_eq!(got.must("docker-content-digest"), manifest);
    assert_eq!(got.body, MANIFEST.as_bytes());

    let content: Vec<u8> = (0..100_000u32).map(|i| (i % 251) as u8).collect();
    let blob = rig.seed(&content).await;
    let path = format!("/v2/library/app/blobs/{blob}");
    let got = rig.get(&path).await;
    assert_eq!(got.status, 200);
    assert_eq!(got.must("content-length"), content.len().to_string());
    assert_eq!(got.must("docker-content-digest"), blob);
    assert!(!got.short);
    assert!(got.body == content, "the body is the blob");
    let ranged = rig.ask("GET", &path, &[("range", "bytes=10-19")]).await;
    let ranged = ranged.expect("an answer");
    assert_eq!(ranged.status, 206);
    assert_eq!(ranged.body, content[10..20]);

    let wrong = "0".repeat(64);
    rig.put_under(&wrong, MANIFEST.as_bytes()).await;
    let bad = rig.get(&format!("/v2/a/manifests/sha256:{wrong}")).await;
    assert_eq!(bad.status, 500);
    assert_eq!(rig.metrics.mismatches(), 1);
    assert_eq!(rig.get("/v2/a/blobs/sha256:nothing").await.status, 400);
    rig.stop().await;
}

/// Asserts a plain front on `::1` serves, where the test host has IPv6 loopback; where it does not
/// the test returns without asserting.
#[tokio::test]
async fn a_plain_front_serves_on_the_ipv6_loopback() {
    let listen: SocketAddr = "[::1]:0".parse().expect("an address");
    if TcpListener::bind(listen).await.is_err() {
        eprintln!("no IPv6 loopback on this host: skipped");
        return;
    }
    let rig = Rig::plain_at(listen).await;
    assert!(rig.addr.ip().is_loopback() && rig.addr.is_ipv6());
    assert_eq!(rig.get("/v2/").await.status, 200);
    rig.stop().await;
}

/// Asserts that a client that sends no request is closed when the header timeout passes, and
/// that the connection held the only slot until then.
#[tokio::test]
async fn a_plain_client_that_sends_no_request_is_closed_at_the_header_timeout() {
    let rig = Rig::plain_with(|limits| {
        limits.max_connections = 1;
        limits.header_timeout = SHORT;
    })
    .await;
    let mut silent = rig.conn().await;
    let closed = tokio::time::timeout(SAFETY, silent.closed()).await;
    assert_eq!(closed, Ok(true));
    assert_eq!(served_within_safety(&rig).await, 200);
    rig.stop().await;
}

/// Asserts that an answer the client does not read is cut when the connection has been unable to
/// write for the write idle time: the only slot it held is free again.
#[tokio::test]
async fn a_plain_answer_left_unread_is_cut_at_the_write_idle_time() {
    let rig = Rig::plain_with(|limits| {
        limits.max_connections = 1;
        limits.write_idle = SHORT;
    })
    .await;
    let _unread = hold_an_answer_back(&rig).await;
    assert_eq!(served_within_safety(&rig).await, 200);
    rig.stop().await;
}

/// Asserts that a connection is closed when its lifetime ends (the lifetime is zero).
#[tokio::test]
async fn a_plain_connection_is_closed_when_its_lifetime_ends() {
    let rig = Rig::plain_with(|limits| limits.lifetime = std::time::Duration::ZERO).await;
    let mut conn = rig.conn().await;
    assert!(conn.closed().await);
    rig.stop().await;
}

/// Asserts that a stop closes the connections that are idle and takes no new one, and that a stop
/// with a response still being sent to a client that does not read it cuts that connection when
/// the drain deadline (zero) passes.
#[tokio::test]
async fn a_stop_drains_a_plain_front() {
    let rig = Rig::start_plain().await;
    let addr = rig.addr;
    let mut conn = rig.conn().await;
    let ping = conn.request("GET", "/v2/", &[]).await.expect("an answer");
    assert_eq!(ping.status, 200);
    assert_eq!(rig.stop().await, 0);
    assert!(conn.closed().await);
    assert!(Conn::open_plain(addr).await.is_err());

    let rig = Rig::plain_with(|limits| limits.drain_deadline = std::time::Duration::ZERO).await;
    let _conn = hold_an_answer_back(&rig).await;
    assert_eq!(rig.stop().await, 1);
}
