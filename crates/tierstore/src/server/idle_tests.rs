//! The stream that gives up on a peer that takes no byte of an answer for too long.

use std::io::{ErrorKind, IoSlice};
use std::net::SocketAddr;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWrite, AsyncWriteExt, duplex};
use tokio::time::{Instant, sleep};

use super::idle::Watched;

const IDLE: Duration = Duration::from_secs(5);

fn peer() -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], 9))
}

/// Asserts that a write that a peer that does not read cannot take, past what the pipe holds,
/// fails with `TimedOut` once the idle time has gone by, and not before.
#[tokio::test(start_paused = true)]
async fn a_write_that_the_peer_does_not_take_fails_after_the_idle_time() {
    let (_far, near) = duplex(8);
    let mut watched = Watched::new(near, IDLE, peer());
    let started = Instant::now();
    let error = watched.write_all(&[7_u8; 100]).await.unwrap_err();
    assert_eq!(error.kind(), ErrorKind::TimedOut);
    let waited = started.elapsed();
    assert!(waited >= IDLE, "{waited:?}");
    assert!(waited < IDLE + Duration::from_secs(1), "{waited:?}");
}

/// Asserts that a peer that takes some bytes before the idle time is over is never cut, however
/// long the whole takes: it reads 8 bytes every 4 seconds of a body of 40, which takes 16
/// seconds to write, three times the idle time.
#[tokio::test(start_paused = true)]
async fn a_peer_that_keeps_taking_bytes_is_not_cut() {
    let (mut far, near) = duplex(8);
    let mut watched = Watched::new(near, IDLE, peer());
    let reading = tokio::spawn(async move {
        let mut seen = 0;
        let mut buffer = [0_u8; 8];
        while seen < 40 {
            sleep(Duration::from_secs(4)).await;
            seen += far.read(&mut buffer).await.unwrap();
        }
        seen
    });
    let started = Instant::now();
    watched.write_all(&[7_u8; 40]).await.unwrap();
    assert!(started.elapsed() >= Duration::from_secs(16));
    assert_eq!(reading.await.unwrap(), 40);
}

/// Asserts that a vectored write is held to the same limit: the pipe takes the first write, the
/// second fails with `TimedOut` after the idle time, and the stream says what the pipe says
/// of vectored writes.
#[tokio::test(start_paused = true)]
async fn a_vectored_write_that_the_peer_does_not_take_fails_after_the_idle_time() {
    let (_far, near) = duplex(8);
    let vectored = AsyncWrite::is_write_vectored(&near);
    let mut watched = Watched::new(near, IDLE, peer());
    assert_eq!(AsyncWrite::is_write_vectored(&watched), vectored);
    let bytes = [7_u8; 8];
    let first = watched.write_vectored(&[IoSlice::new(&bytes)]).await;
    assert_eq!(first.unwrap(), 8);
    let started = Instant::now();
    let second = watched.write_vectored(&[IoSlice::new(&bytes)]).await;
    assert_eq!(second.unwrap_err().kind(), ErrorKind::TimedOut);
    assert!(started.elapsed() >= IDLE);
}

/// Asserts that reads, flushes and shutdowns go to the stream as they are: what the peer sends is
/// read, what is written is flushed to the peer, and the shutdown ends the peer's read.
#[tokio::test]
async fn reads_flushes_and_shutdowns_pass_through() {
    let (mut far, near) = duplex(64);
    let mut watched = Watched::new(near, IDLE, peer());
    far.write_all(b"request").await.unwrap();
    let mut got = [0_u8; 7];
    watched.read_exact(&mut got).await.unwrap();
    assert_eq!(&got, b"request");
    watched.write_all(b"answer").await.unwrap();
    watched.flush().await.unwrap();
    watched.shutdown().await.unwrap();
    let mut rest = Vec::new();
    far.read_to_end(&mut rest).await.unwrap();
    assert_eq!(rest, b"answer");
}
