//! The idle limit on an answer over a real connection: the one test of it on a TCP stream. The
//! other tests of the idle limit, of bodies and of answers, are in the crate, on a paused clock
//! over in-memory pipes (`src/server/body_stall_tests.rs`, `src/server/drain_tests.rs`).

use std::time::Duration;

use bytes::Bytes;
use tierstore::Backend;
use tierstore::server::Drained;
use tokio::io::AsyncReadExt;

use crate::stalled::send_get;
use crate::support::Harness;

/// The idle time of the server of this test. The test waits for the server to act on it and
/// asserts nothing about how long that takes.
const IDLE: Duration = Duration::from_millis(200);

/// The longest the test waits for the server to close the connection. It only stops a test that
/// hangs, and then the test fails.
const SAFETY: Duration = Duration::from_secs(60);

/// The size of an object that the socket buffers cannot hold, so that a client that does not read
/// holds its answer back.
const BIG: usize = 48 << 20;

/// Asserts that a get whose client stops reading is closed by the server over a real TCP
/// connection: the client reads the first piece of the answer and nothing more, then reads to the
/// end of the connection, which only the server closing it brings, and gets a part of the answer;
/// when the drain then ends the server has no connection open and nothing was cut. The test waits
/// for the end of the connection and for the end of the drain, and asserts nothing about how long
/// either takes.
#[tokio::test]
async fn a_get_whose_client_stops_reading_is_closed_by_the_server() {
    let started = Harness::start_fixed(|config| config.body_idle_timeout = IDLE);
    let h = started.await;
    let object = Bytes::from(vec![7_u8; BIG]);
    h.hot_mem.put("big", object).await.unwrap();
    let mut stream = send_get(&h, "big").await;
    let mut piece = vec![0_u8; 1 << 10];
    let first = tokio::time::timeout(SAFETY, stream.read(&mut piece)).await;
    let read = first.unwrap().unwrap();
    assert!(read > 0);
    let mut received = piece[..read].to_vec();
    let ended = tokio::time::timeout(SAFETY, stream.read_to_end(&mut received)).await;
    assert!(ended.is_ok());
    assert!(received.starts_with(b"HTTP/1.1 200"));
    assert!(received.len() < BIG);
    h.server.begin_drain();
    let drained = tokio::time::timeout(SAFETY, h.served).await;
    assert_eq!(drained.unwrap().unwrap(), Drained { abandoned: 0 });
    assert_eq!(h.server.open_connections(), 0);
}
