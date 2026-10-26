//! The idle time of an answer on a connection, run on virtual time over an in-memory pipe: a client
//! that reads slowly but steadily is served, and one that stops reading is cut. No wait here races
//! a real clock, so what the tests prove does not depend on how loaded the host is.

use std::net::SocketAddr;
use std::time::Duration;

use bytes::Bytes;
use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream, duplex};
use tokio::time::{Instant, sleep};

use crate::Backend;

use super::auth::sign;
use super::testing::{BUCKET, Rig};

/// The idle time of the server of these tests.
const IDLE: Duration = Duration::from_secs(1);

/// The size of the object, many times the pipe.
const OBJECT: usize = 4 << 20;

/// What the pipe between the client and the server holds.
const PIPE: usize = 64 << 10;

/// What the client reads at a time.
const CHUNK: usize = 32 << 10;

/// The bytes of the object: a pattern, so that a piece in the wrong place shows.
fn object() -> Vec<u8> {
    let mut bytes = Vec::with_capacity(OBJECT);
    for i in 0..OBJECT {
        bytes.push(u8::try_from(i % 251).unwrap());
    }
    bytes
}

/// A server with the idle time `IDLE` that serves one connection, and the client end of that
/// connection, on which a signed get of the object, closing after the answer, was sent.
async fn open_get() -> DuplexStream {
    let rig = Rig::with(64 << 20, |config| config.body_idle_timeout = IDLE).await;
    rig.hot_mem.put("big", Bytes::from(object())).await.unwrap();
    let uri = format!("/{BUCKET}/big");
    let (parts, _) = sign(&rig.config, "GET", &uri, &[], b"").into_parts();
    let mut text = format!("GET {uri} HTTP/1.1\r\n");
    for (name, value) in &parts.headers {
        text.push_str(&format!("{name}: {}\r\n", value.to_str().unwrap()));
    }
    text.push_str("connection: close\r\n\r\n");
    let (mut client, server_end) = duplex(PIPE);
    let peer = SocketAddr::from(([127, 0, 0, 1], 9));
    rig.server.connection(server_end, peer);
    client.write_all(text.as_bytes()).await.unwrap();
    client
}

/// The position in `received` where the body begins.
fn body_start(received: &[u8]) -> usize {
    let found = received.windows(4).position(|w| w == b"\r\n\r\n");
    found.unwrap() + 4
}

/// Asserts that a client that reads its answer a chunk at a time, with a wait of half the idle
/// time between chunks, is served whole: the answer is 200 and the object byte for byte, though
/// the pipe holds a sixty-fourth of the object and the whole takes more than ten times the idle
/// time. Each wait is shorter than the idle time, so the server is never left with a write that nobody
/// takes for the idle time.
#[tokio::test(start_paused = true)]
async fn a_client_that_reads_slowly_but_steadily_gets_the_whole_answer_over_many_idle_times() {
    let mut client = open_get().await;
    let started = Instant::now();
    let mut received = Vec::new();
    let mut chunk = vec![0_u8; CHUNK];
    loop {
        let read = client.read(&mut chunk).await.unwrap();
        if read == 0 {
            break;
        }
        received.extend_from_slice(&chunk[..read]);
        sleep(IDLE / 2).await;
    }
    assert!(received.starts_with(b"HTTP/1.1 200"));
    let start = body_start(&received);
    assert!(received[start..] == object()[..]);
    assert!(started.elapsed() > IDLE * 10, "{:?}", started.elapsed());
}

/// Asserts that a client that reads one chunk and then waits twice the idle time without reading is
/// cut: the connection ends with a part of the object, so the idle time applies to this path, and
/// the slow reader above is not served only because nothing here is ever cut.
#[tokio::test(start_paused = true)]
async fn a_client_that_stops_reading_is_cut_after_the_idle_time() {
    let mut client = open_get().await;
    let mut chunk = vec![0_u8; CHUNK];
    let read = client.read(&mut chunk).await.unwrap();
    assert!(read > 0);
    sleep(IDLE * 2).await;
    let mut received = chunk[..read].to_vec();
    client.read_to_end(&mut received).await.unwrap();
    assert!(received.starts_with(b"HTTP/1.1 200"));
    let start = body_start(&received);
    assert!(received.len() - start < OBJECT);
}
