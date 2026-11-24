//! The watch on a served connection ([`Watched`]): what moves it, what does not, and the
//! error and the log line once it has not moved for its limit. The connection is one end
//! of an in-memory pipe, the client the other; the clock is paused and moved by hand.

use std::io;
use std::pin::pin;
use std::task::Poll;
use std::time::Duration;

use futures::poll;
use kbf_tls::Liveness;
use tokio::io::{AsyncReadExt as _, AsyncWrite as _, AsyncWriteExt as _, DuplexStream};
use tokio::time::advance;
use tonic::transport::server::Connected as _;

use crate::conn_watch::{PING_ANSWER_WAIT, Watched, limit};
use crate::serve::StreamTiming;
use crate::test_log::capture;

const LIMIT: Duration = Duration::from_secs(10);
const SECOND: Duration = Duration::from_secs(1);

/// A watched end of a pipe that holds `bytes`, and the other end, the client's.
fn pipe(bytes: usize, limit: Option<Duration>) -> (Watched<DuplexStream>, DuplexStream) {
    let (server, client) = tokio::io::duplex(bytes);
    (
        Watched::new(server, limit, None, Liveness::default()),
        client,
    )
}

/// The kind and text of the error `polled` ended with, if it ended with one.
fn failed<T>(polled: Poll<io::Result<T>>) -> Option<(io::ErrorKind, String)> {
    match polled {
        Poll::Ready(Err(e)) => Some((e.kind(), e.to_string())),
        _ => None,
    }
}

/// The error a connection fails with once it has not moved for [`LIMIT`].
fn past_the_limit() -> Option<(io::ErrorKind, String)> {
    let text = "the connection moved nothing either way for 10 s";
    Some((io::ErrorKind::TimedOut, text.to_owned()))
}

/// Catches a watch that never closes a connection, or closes it early: a read waiting on
/// a client that sends nothing is still waiting a second short of the limit, and fails
/// `TimedOut` at the limit, with a warning that says why, logged once however many reads
/// fail after it.
#[tokio::test(start_paused = true)]
async fn a_connection_that_reads_nothing_fails_at_the_limit() {
    let log = capture();
    let (mut watched, _client) = pipe(64, Some(LIMIT));
    let mut buf = [0; 8];
    let mut read = pin!(watched.read(&mut buf));

    assert!(poll!(read.as_mut()).is_pending());
    advance(LIMIT - SECOND).await;
    assert!(poll!(read.as_mut()).is_pending());
    advance(SECOND).await;

    assert_eq!(failed(poll!(read)), past_the_limit());
    let mut again = [0; 8];
    assert_eq!(
        failed(poll!(pin!(watched.read(&mut again)))),
        past_the_limit()
    );
    let text = log.text();
    let line = "closing a connection that moved nothing either way for the keepalive's limit: \
                no byte read from the client, none it held back sent peer=None secs=10";
    assert_eq!(text.matches(line).count(), 1, "{text}");
}

/// Catches a read that does not move the connection: a byte read a second before the limit
/// gives the connection the whole limit again from then.
#[tokio::test(start_paused = true)]
async fn a_byte_read_moves_the_connection() {
    let (mut watched, mut client) = pipe(64, Some(LIMIT));
    let mut buf = [0; 8];
    advance(LIMIT - SECOND).await;
    client.write_all(b"x").await.expect("written");
    assert_eq!(watched.read(&mut buf).await.expect("read"), 1);

    let mut read = pin!(watched.read(&mut buf));
    advance(LIMIT - SECOND).await;
    assert!(poll!(read.as_mut()).is_pending());
    advance(SECOND).await;

    assert_eq!(failed(poll!(read)), past_the_limit());
}

/// The end of the client's stream reads as the end, not as the connection moving or
/// failing: what the server does with it is the server's.
#[tokio::test(start_paused = true)]
async fn the_end_of_the_clients_stream_reads_as_the_end() {
    let (mut watched, client) = pipe(64, Some(LIMIT));
    drop(client);
    let mut buf = [0; 8];

    assert_eq!(watched.read(&mut buf).await.expect("read"), 0);
}

/// Catches a watch that counts every write as the connection moving: a write the pipe
/// takes at once, a second before the limit, proves nothing about the client, and the
/// connection still fails at the limit.
#[tokio::test(start_paused = true)]
async fn a_write_taken_at_once_does_not_move_the_connection() {
    let (mut watched, _client) = pipe(64, Some(LIMIT));
    advance(LIMIT - SECOND).await;
    watched.write_all(b"ping").await.expect("taken");
    let mut buf = [0; 8];
    let read = pin!(watched.read(&mut buf));

    advance(SECOND).await;

    assert_eq!(failed(poll!(read)), past_the_limit());
}

/// Catches a watch that counts no write as the connection moving: a write the full pipe
/// held back goes through once the client reads, a second before the limit, and that gives
/// the connection the whole limit again from then.
#[tokio::test(start_paused = true)]
async fn a_write_held_back_then_taken_moves_the_connection() {
    let (mut watched, mut client) = pipe(4, Some(LIMIT));
    watched.write_all(b"full").await.expect("taken");
    {
        let mut write = pin!(watched.write(b"more"));
        assert!(poll!(write.as_mut()).is_pending());
        advance(LIMIT - SECOND).await;
        let mut drained = [0; 4];
        client.read_exact(&mut drained).await.expect("read");
        assert!(matches!(poll!(write), Poll::Ready(Ok(4))));
    }
    let mut buf = [0; 8];
    let mut read = pin!(watched.read(&mut buf));

    advance(LIMIT - SECOND).await;
    assert!(poll!(read.as_mut()).is_pending());
    advance(SECOND).await;

    assert_eq!(failed(poll!(read)), past_the_limit());
}

/// Catches a write that waits forever on a client that is gone, or fails early: a write
/// the full pipe holds back is still waiting a second short of the limit, and fails at it,
/// as a read does.
#[tokio::test(start_paused = true)]
async fn a_write_held_back_for_the_limit_fails() {
    let (mut watched, _client) = pipe(4, Some(LIMIT));
    watched.write_all(b"full").await.expect("taken");
    let mut write = pin!(watched.write(b"more"));

    assert!(poll!(write.as_mut()).is_pending());
    advance(LIMIT - SECOND).await;
    assert!(poll!(write.as_mut()).is_pending());
    advance(SECOND).await;

    assert_eq!(failed(poll!(write)), past_the_limit());
}

/// An error of the connection itself passes through as it is, on a read and a write: the
/// client gone with a reset is not reported as a quiet connection.
#[tokio::test(start_paused = true)]
async fn an_error_of_the_connection_passes_through() {
    let (mut watched, client) = pipe(64, Some(LIMIT));
    drop(client);

    let wrote = watched.write(b"x").await.expect_err("the client is gone");

    assert_eq!(wrote.kind(), io::ErrorKind::BrokenPipe);
}

/// Vectored writes and flushes are watched as writes are, and the rest of the connection
/// is the inner one's: whether it writes vectored, its shutdown, its connect info.
#[tokio::test(start_paused = true)]
async fn vectored_writes_and_flushes_are_watched_and_the_rest_passes_through() {
    let (mut watched, mut client) = pipe(4, Some(LIMIT));
    let (inner, _) = tokio::io::duplex(4);
    assert_eq!(watched.is_write_vectored(), inner.is_write_vectored());
    let () = watched.connect_info();
    let full = [io::IoSlice::new(b"full")];
    assert_eq!(watched.write_vectored(&full).await.expect("taken"), 4);
    watched.flush().await.expect("flushed");
    {
        let more = [io::IoSlice::new(b"more")];
        let mut write = pin!(watched.write_vectored(&more));
        assert!(poll!(write.as_mut()).is_pending());
        advance(LIMIT).await;
        assert_eq!(failed(poll!(write)), past_the_limit());
    }

    watched.shutdown().await.expect("shut down");
    let mut read = Vec::new();
    client.read_to_end(&mut read).await.expect("read");
    assert_eq!(read, b"full");
}

/// Catches a watch that closes connections with the keepalive off: with no limit, a read
/// and a held-back write both wait as long as the inner connection makes them.
#[tokio::test(start_paused = true)]
async fn without_a_limit_nothing_is_closed() {
    let (mut watched, _client) = pipe(4, None);
    let a_year = Duration::from_secs(365 * 24 * 60 * 60);
    let mut buf = [0; 8];
    {
        let mut read = pin!(watched.read(&mut buf));
        assert!(poll!(read.as_mut()).is_pending());
        advance(a_year).await;
        assert!(poll!(read).is_pending());
    }
    watched.write_all(b"full").await.expect("taken");
    watched.flush().await.expect("flushed");
    let more = [io::IoSlice::new(b"more")];
    let mut write = pin!(watched.write_vectored(&more));

    assert!(poll!(write.as_mut()).is_pending());
    advance(a_year).await;

    assert!(poll!(write).is_pending());
}

/// The limit is the PING interval and the keepalive timeout together, the time a client
/// that is gone was given when the PING's answer decided; with no PING sent there is none,
/// as a quiet connection would have nothing to read. hyper's own wait for the answer is
/// longer, so the watch decides.
#[test]
fn the_limit_is_the_ping_interval_and_timeout_and_none_without_pings() {
    let timing = StreamTiming::default();
    let off = StreamTiming {
        keepalive_interval: Duration::ZERO,
        ..timing
    };

    assert_eq!(limit(timing), Some(Duration::from_secs(150)));
    assert_eq!(limit(off), None);
    assert!(PING_ANSWER_WAIT > Duration::from_secs(150));
}

/// Catches a quiet close the streams on the connection cannot tell from their client
/// leaving: the connection's flag is not closed quiet while the watch waits, and is from
/// the moment the limit passes, before the error that ends the streams is returned.
#[tokio::test(start_paused = true)]
async fn the_limit_marks_the_connection_closed_quiet() {
    let (server, _client) = tokio::io::duplex(64);
    let live = Liveness::default();
    let mut watched = Watched::new(server, Some(LIMIT), None, live.clone());
    let mut buf = [0; 8];
    let mut read = pin!(watched.read(&mut buf));

    assert!(poll!(read.as_mut()).is_pending());
    advance(LIMIT - SECOND).await;
    assert!(poll!(read.as_mut()).is_pending());
    assert!(!live.is_gone(), "a connection inside its limit is there");
    advance(SECOND).await;

    assert_eq!(failed(poll!(read)), past_the_limit());
    assert!(live.closed_quiet(), "the watch's close is recorded");
}
