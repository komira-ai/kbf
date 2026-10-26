//! Connections over in-memory pipes, for the tests that run the connections, the accept loop and
//! the drain on the paused clock. There is no socket and no wait on a real clock: a test counts
//! what happens (requests in flight, connections open, requests cut) and awaits it.

use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use bytes::Bytes;
use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream, duplex};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::Backend;

use super::auth::sign;
use super::testing::{BUCKET, Rig};
use super::{Drained, Incoming, Server, ServerConfig};

/// What the pipe between a client and the server holds.
const PIPE: usize = 64 << 10;

/// What the client of a get reads at a time.
const CHUNK: usize = 32 << 10;

/// The size of the object `big`: many times what the pipe holds, so that a client that does not
/// read holds its answer back.
pub(super) const OBJECT: usize = 4 << 20;

/// The longest a test waits for something that must happen. It only stops a test that hangs, and
/// then the test fails: no test passes by the end of this time. On the paused clock it comes
/// once every task is waiting, so a hung test fails at once.
const SAFETY: Duration = Duration::from_secs(86_400);

/// The header line that has the server close the connection after the answer.
const CLOSE: &str = "connection: close\r\n";

/// What `waiting` gives, or a failure of the test if it never gives it.
async fn bounded<F: Future>(waiting: F) -> F::Output {
    let waited = tokio::time::timeout(SAFETY, waiting).await;
    waited.expect("an event that never came")
}

/// The address that the server is told its peer has.
fn peer() -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], 9))
}

/// A connection that `server` serves as it serves one it accepted, and the client end of it.
pub(super) fn attach(server: &Arc<Server>) -> DuplexStream {
    let (client, far) = duplex(PIPE);
    server.connection(far, peer());
    client
}

/// Waits until `ready` holds of `server`. It is asked now and after every notice by which the
/// server says that a request or a connection began or ended.
pub(super) async fn until(server: &Server, ready: impl Fn(&Server) -> bool) {
    let waiting = async {
        loop {
            let changed = server.idle.notified();
            tokio::pin!(changed);
            let _ = changed.as_mut().enable();
            if ready(server) {
                return;
            }
            changed.await;
        }
    };
    bounded(waiting).await;
}

/// The pipes of a [`Serving`] as the source of connections of the accept loop.
struct Pipes {
    arriving: mpsc::UnboundedReceiver<DuplexStream>,
    /// How many of the next accepts fail.
    failing: usize,
}

impl Incoming for Pipes {
    type Stream = DuplexStream;

    fn poll_incoming(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<io::Result<(DuplexStream, SocketAddr)>> {
        if self.failing > 0 {
            self.failing -= 1;
            return Poll::Ready(Err(io::Error::other("an accept that failed")));
        }
        let arrived = self.arriving.poll_recv(cx);
        let door = "a door that is open";
        arrived.map(|stream| Ok((stream.expect(door), peer())))
    }
}

/// A server whose accept loop takes its connections from pipes, as `serve` takes them from a
/// socket, running on a task of its own.
pub(super) struct Serving {
    pub(super) rig: Rig,
    door: mpsc::UnboundedSender<DuplexStream>,
    /// The accept loop; it ends with the drain.
    pub(super) served: JoinHandle<Drained>,
}

impl Serving {
    /// A server whose settings are changed by `change`.
    pub(super) async fn start(change: impl FnOnce(&mut ServerConfig)) -> Self {
        Self::start_after(0, change).await
    }

    /// The same, where the first `failures` accepts fail.
    pub(super) async fn start_after(
        failures: usize,
        change: impl FnOnce(&mut ServerConfig),
    ) -> Self {
        let rig = Rig::with(64 << 20, change).await;
        let (door, arriving) = mpsc::unbounded_channel();
        let incoming = Pipes {
            arriving,
            failing: failures,
        };
        let server = Arc::clone(&rig.server);
        let served = tokio::spawn(async move { server.serve_from(incoming).await });
        Self { rig, door, served }
    }

    /// A connection to the server, or none once the server has stopped taking connections.
    pub(super) fn connect(&self) -> Option<DuplexStream> {
        let (client, far) = duplex(PIPE);
        self.door.send(far).ok().map(|()| client)
    }

    /// Waits until the server has stopped taking connections.
    pub(super) async fn refusing(&self) {
        bounded(self.door.closed()).await;
    }
}

/// How the drain ended: waits for the accept loop `served` to return.
pub(super) async fn drained(served: JoinHandle<Drained>) -> Drained {
    bounded(served).await.expect("the accept loop ended")
}

/// The head of a request signed for the server of `rig`, as a client writes it: the request line,
/// the signed headers, `connection` (a header line, or nothing) and the length of `body`.
pub(super) fn head(
    rig: &Rig,
    method: &str,
    uri: &str,
    extra: &[(&str, &str)],
    body: &[u8],
    connection: &str,
) -> String {
    let (parts, _) = sign(&rig.config, method, uri, extra, body).into_parts();
    let mut text = format!("{method} {uri} HTTP/1.1\r\n");
    for (name, value) in &parts.headers {
        text.push_str(&format!("{name}: {}\r\n", value.to_str().unwrap()));
    }
    text.push_str(connection);
    text.push_str(&format!("content-length: {}\r\n\r\n", body.len()));
    text
}

/// Everything the server sends on the connection until it closes it.
pub(super) async fn read_answer(client: &mut DuplexStream) -> String {
    let mut bytes = Vec::new();
    bounded(client.read_to_end(&mut bytes)).await.unwrap();
    String::from_utf8_lossy(&bytes).into_owned()
}

/// Reads the rest of the answer, to the end of the connection, after `answer`.
pub(super) async fn read_rest(client: &mut DuplexStream, answer: &mut Vec<u8>) {
    bounded(client.read_to_end(answer)).await.unwrap();
}

/// Reads the rest of the answer, to the end of the connection, after `answer`, one piece at a
/// time and waiting `gap` on the clock before each. Gives the number of reads.
pub(super) async fn read_rest_slowly(
    client: &mut DuplexStream,
    answer: &mut Vec<u8>,
    gap: Duration,
) -> u32 {
    let mut piece = vec![0_u8; CHUNK];
    let mut reads = 0;
    loop {
        tokio::time::sleep(gap).await;
        let read = bounded(client.read(&mut piece)).await.unwrap();
        reads += 1;
        if read == 0 {
            return reads;
        }
        answer.extend_from_slice(&piece[..read]);
    }
}

/// The status code of an answer.
pub(super) fn status_of(answer: &str) -> u16 {
    answer[9..12].parse().unwrap()
}

/// Sends a request on a connection of its own, which closes after the answer, and reads the
/// answer. The request is signed for the server, with these extra headers.
pub(super) async fn call(
    serving: &Serving,
    method: &str,
    uri: &str,
    extra: &[(&str, &str)],
    body: &[u8],
) -> String {
    let mut client = serving.connect().expect("a server that takes connections");
    let text = head(&serving.rig, method, uri, extra, body, CLOSE);
    client.write_all(text.as_bytes()).await.unwrap();
    client.write_all(body).await.unwrap();
    read_answer(&mut client).await
}

/// Sends a probe (`/healthz` or `/readyz`), which is not signed, on a connection of its own,
/// which closes after the answer, and reads the answer.
pub(super) async fn probe(serving: &Serving, path: &str) -> String {
    let mut client = serving.connect().expect("a server that takes connections");
    let text = format!("GET {path} HTTP/1.1\r\nhost: tier.test\r\n{CLOSE}\r\n");
    client.write_all(text.as_bytes()).await.unwrap();
    read_answer(&mut client).await
}

/// A put whose body was sent in part, so that the server holds it in flight.
pub(super) struct Stalled {
    client: DuplexStream,
    rest: Vec<u8>,
}

impl Stalled {
    /// Sends on `client` the head of a put to `uri` signed for the server of `rig` with these extra
    /// headers, and the first `sent` bytes of `body`. The connection closes after the answer.
    pub(super) async fn open(
        mut client: DuplexStream,
        rig: &Rig,
        uri: &str,
        extra: &[(&str, &str)],
        body: &[u8],
        sent: usize,
    ) -> Self {
        let text = head(rig, "PUT", uri, extra, body, CLOSE);
        client.write_all(text.as_bytes()).await.unwrap();
        client.write_all(&body[..sent]).await.unwrap();
        Self {
            client,
            rest: body[sent..].to_vec(),
        }
    }

    /// Sends the rest of the body and reads the answer.
    pub(super) async fn finish(mut self) -> String {
        self.client.write_all(&self.rest).await.unwrap();
        read_answer(&mut self.client).await
    }

    /// Reads the answer without sending the rest of the body.
    pub(super) async fn answer(mut self) -> String {
        read_answer(&mut self.client).await
    }

    /// Sends the rest of the body a byte at a time, waiting `gap` on the clock before each, and
    /// reads the answer. Gives the answer and the number of waits.
    pub(super) async fn trickle(mut self, gap: Duration) -> (String, u32) {
        let mut waits = 0;
        for byte in std::mem::take(&mut self.rest) {
            tokio::time::sleep(gap).await;
            self.client.write_all(&[byte]).await.unwrap();
            waits += 1;
        }
        (read_answer(&mut self.client).await, waits)
    }
}

/// Puts the object `big`, of `OBJECT` bytes, in the hot tier.
pub(super) async fn put_big(rig: &Rig) {
    let object = Bytes::from(vec![7_u8; OBJECT]);
    rig.hot_mem.put("big", object).await.unwrap();
}

/// Sends a signed get of `key` on `client`, which closes after the answer, and reads the first
/// piece of the answer, so that the server has begun to send it. Gives the piece.
pub(super) async fn begin_get(rig: &Rig, client: &mut DuplexStream, key: &str) -> Vec<u8> {
    let uri = format!("/{BUCKET}/{key}");
    let text = head(rig, "GET", &uri, &[], b"", CLOSE);
    client.write_all(text.as_bytes()).await.unwrap();
    let mut piece = vec![0_u8; CHUNK];
    let read = bounded(client.read(&mut piece)).await.unwrap();
    piece.truncate(read);
    piece
}
