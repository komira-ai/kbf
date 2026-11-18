//! A connection that goes silent is found by the client's HTTP/2 keep-alive and replaced. The
//! client reaches the fake through a loopback proxy that reads the HTTP/2 frames the client
//! sends: at the first PING of the client's own, the proxy stops passing anything either way
//! on that connection and keeps it open, as a NAT that dropped it or a service host that went
//! away without a FIN would. Time is the tokio clock, frozen once the first call is answered:
//! the idle interval and the wait for the PING's answer pass on it, so the test waits for
//! events (the PING seen, the connection closed by the client), never for a duration.

mod support;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use kbf_job_client::fake::TOKEN;
use kbf_job_client::{Client, Config, Token};
use support::{fake, spec};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Notify;

/// The connection preface a client sends before its first frame.
const PREFACE: usize = 24;

/// The HTTP/2 frame type of a PING.
const PING: u8 = 0x6;

/// The flag of a PING that answers another.
const ACK: u8 = 0x1;

/// What the proxy saw.
#[derive(Default)]
struct Seen {
    /// Connections accepted from the client.
    accepted: AtomicUsize,
    /// A PING of the client's own was seen.
    pinged: Notify,
    /// A connection the proxy fell silent on was closed by the client.
    closed: Notify,
}

/// A proxy on a loopback port picked by the system, to `upstream`.
async fn proxy(upstream: String) -> (String, Arc<Seen>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let seen = Arc::new(Seen::default());
    let shared = Arc::clone(&seen);
    tokio::spawn(async move {
        while let Ok((client, _)) = listener.accept().await {
            shared.accepted.fetch_add(1, Ordering::SeqCst);
            let server = TcpStream::connect(&upstream).await.unwrap();
            tokio::spawn(relay(client, server, Arc::clone(&shared)));
        }
    });
    (format!("http://{addr}"), seen)
}

/// Passes the frames of one connection both ways until the client sends a PING of its own;
/// then passes nothing, reads what the client sends until it closes the connection, and says
/// so.
async fn relay(client: TcpStream, server: TcpStream, seen: Arc<Seen>) {
    let (from_client, to_client) = client.into_split();
    let (from_server, mut to_server) = server.into_split();
    let silent = Arc::new(AtomicBool::new(false));
    tokio::spawn(answers(from_server, to_client, Arc::clone(&silent)));
    let mut from_client = from_client;
    let mut preface = [0; PREFACE];
    if from_client.read_exact(&mut preface).await.is_err() {
        return;
    }
    let _ = to_server.write_all(&preface).await;
    loop {
        let mut head = [0; 9];
        if from_client.read_exact(&mut head).await.is_err() {
            break;
        }
        let length = usize::from(head[0]) << 16 | usize::from(head[1]) << 8 | usize::from(head[2]);
        let mut payload = vec![0; length];
        if from_client.read_exact(&mut payload).await.is_err() {
            break;
        }
        if head[3] == PING && head[4] & ACK == 0 && !silent.swap(true, Ordering::SeqCst) {
            seen.pinged.notify_one();
        }
        if !silent.load(Ordering::SeqCst) {
            let _ = to_server.write_all(&head).await;
            let _ = to_server.write_all(&payload).await;
        }
    }
    if silent.load(Ordering::SeqCst) {
        seen.closed.notify_one();
    }
}

/// Passes the server's bytes to the client while the connection is not silent.
async fn answers(
    mut from_server: OwnedReadHalf,
    mut to_client: OwnedWriteHalf,
    silent: Arc<AtomicBool>,
) {
    let mut buf = vec![0; 16 * 1024];
    while let Ok(read @ 1..) = from_server.read(&mut buf).await {
        if !silent.load(Ordering::SeqCst) && to_client.write_all(&buf[..read]).await.is_err() {
            return;
        }
    }
}

/// Asserts that a connection that goes silent is found and replaced by the client itself:
/// once the connection has been idle the client sends a PING; with no answer to it, the
/// client closes the connection; and the next call is answered through a second connection.
/// Catches a client with no keep-alive, whose next call would wait out its whole deadline on
/// the dead connection: then no PING is ever sent, and the frozen clock reaches the test's
/// guard instead.
#[tokio::test]
async fn a_connection_that_goes_silent_is_found_by_a_ping_and_replaced() {
    let (server, _) = fake().await;
    let (origin, seen) = proxy(server.origin().trim_start_matches("http://").to_owned()).await;
    let config = Config::new(origin.parse().unwrap(), Token::new(TOKEN).unwrap());
    let client = Client::new(config).unwrap();
    let job = client.submit_job(spec("j")).await.unwrap();
    assert_eq!(seen.accepted.load(Ordering::SeqCst), 1);

    tokio::time::pause();
    let guard = Duration::from_secs(3_600);
    tokio::time::timeout(guard, seen.pinged.notified())
        .await
        .expect("an idle connection is pinged");
    tokio::time::timeout(guard, seen.closed.notified())
        .await
        .expect("a ping with no answer closes the connection");
    tokio::time::resume();

    let read = client.get_job(&job.id).await.unwrap();
    assert_eq!(read.id, job.id);
    assert_eq!(
        seen.accepted.load(Ordering::SeqCst),
        2,
        "a second connection"
    );
}
