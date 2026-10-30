//! The bounds on the listener's connections. None of these asserts a time: a bound of zero makes
//! the case happen at once, and the rest wait for the server to do what it was told.

use std::time::Duration;

use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpStream;

use crate::client::Conn;
use crate::rig::{Rig, hex_of};

/// Whether the server closes `tcp` without sending a byte of HTTP.
async fn closed_by_server(mut tcp: TcpStream) -> bool {
    let mut buf = Vec::new();
    let _ = tcp.read_to_end(&mut buf).await;
    !buf.starts_with(b"HTTP/")
}

/// Asserts that bytes that are not a TLS handshake get the connection closed, and that the
/// listener goes on serving.
#[tokio::test]
async fn bytes_that_are_not_a_handshake_close_the_connection() {
    let rig = Rig::start().await;
    let mut tcp = TcpStream::connect(rig.addr).await.expect("connected");
    tcp.write_all(b"GET /v2/ HTTP/1.1\r\nhost: oci.test\r\n\r\n")
        .await
        .expect("sent");
    assert!(closed_by_server(tcp).await);
    assert_eq!(rig.get("/v2/").await.status, 200);
    rig.stop().await;
}

/// Asserts that a client that has not completed the handshake within the header timeout is
/// closed (the timeout is zero, so no handshake can be in time).
#[tokio::test]
async fn a_handshake_out_of_time_closes_the_connection() {
    let rig = Rig::with(|limits| limits.header_timeout = Duration::ZERO).await;
    let tcp = TcpStream::connect(rig.addr).await.expect("connected");
    assert!(closed_by_server(tcp).await);
    rig.stop().await;
}

/// Asserts that a connection is closed when its lifetime ends (the lifetime is zero, so it
/// ends as the connection begins) without an answer to a request that was not made.
#[tokio::test]
async fn a_connection_is_closed_when_its_lifetime_ends() {
    let rig = Rig::with(|limits| limits.lifetime = Duration::ZERO).await;
    let mut conn = rig.conn().await;
    assert!(conn.closed().await);
    rig.stop().await;
}

/// Asserts that a stop closes the connections that are idle, takes no new connection, and
/// returns with none cut.
#[tokio::test]
async fn a_stop_closes_idle_connections_and_the_socket() {
    let rig = Rig::start().await;
    let (addr, connector) = (rig.addr, rig.connector.clone());
    let mut conn = rig.conn().await;
    // The connection has been served, so the listener has it.
    let ping = conn.request("GET", "/v2/", &[]).await.expect("an answer");
    assert_eq!(ping.status, 200);
    assert_eq!(rig.stop().await, 0);
    assert!(conn.closed().await);
    assert!(Conn::open(addr, &connector).await.is_err());
}

/// Asserts that a stop with a response still being sent, and a client that is not reading it,
/// cuts that connection when the drain deadline passes (it is zero) and says so.
#[tokio::test]
async fn a_response_still_being_sent_is_cut_at_the_drain_deadline() {
    let rig = Rig::with(|limits| limits.drain_deadline = Duration::ZERO).await;
    // Too large for the socket buffers of both ends, and not checked against its digest before
    // the stop: the first chunks are all the server reads.
    let big = vec![7u8; 64 << 20];
    let hex = hex_of(b"not these bytes");
    rig.put_under(&hex, &big).await;
    let mut conn = rig.conn().await;
    conn.send("GET", &format!("/v2/a/blobs/sha256:{hex}"), &[])
        .await;
    assert_eq!(conn.head_only().await, Some(200));
    assert_eq!(rig.stop().await, 1);
}

/// Asserts that with room for one connection a second waits for the first to end, and then
/// is served. (That the second waits is not proved by this test, which would need a clock; that
/// it is served once the first leaves is.)
#[tokio::test]
async fn a_connection_over_the_limit_waits_for_a_slot() {
    let rig = Rig::with(|limits| limits.max_connections = 1).await;
    let mut first = rig.conn().await;
    assert_eq!(
        first
            .request("GET", "/v2/", &[])
            .await
            .expect("an answer")
            .status,
        200
    );
    let (addr, connector) = (rig.addr, rig.connector.clone());
    let second = tokio::spawn(async move { Conn::open(addr, &connector).await });
    for _ in 0..4 {
        assert_eq!(
            first
                .request("GET", "/v2/", &[])
                .await
                .expect("an answer")
                .status,
            200
        );
        assert!(
            !second.is_finished(),
            "the second connection was taken with no free slot"
        );
    }
    drop(first);
    let mut second = second.await.expect("joined").expect("a connection");
    assert_eq!(
        second
            .request("GET", "/v2/", &[])
            .await
            .expect("an answer")
            .status,
        200
    );
    drop(second);
    assert_eq!(rig.stop().await, 0);
}

/// Asserts that an address that is in use cannot be bound, and the error names it.
#[tokio::test]
async fn an_address_in_use_is_refused() {
    let rig = Rig::start().await;
    let refused = rig.bind_again().await.err().expect("refused");
    let text = refused.to_string();
    assert!(
        text.starts_with(&format!("binding {}: ", rig.addr)),
        "{text}"
    );
    assert!(!refused.is_invalid_flags());
    rig.stop().await;
}
