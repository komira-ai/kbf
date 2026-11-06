//! A relay on a loopback port in front of a job service that loses the first answer: what a
//! client sees when the network drops an answer after the server has acted.
//!
//! Every byte from the client is passed to the server. From the server, the frames of the
//! connection itself (stream 0: settings, pings, window updates) are passed back, and so is
//! every frame of the first connection until the first `HEADERS` frame of a request's stream:
//! the head of the first answer, which the server sends only once its handler has returned. At
//! that frame the relay closes both sides of that connection without passing it on. Every later
//! connection is relayed whole. [`DropFirstAnswer::dropped`] says whether an answer was lost, so
//! a test cannot pass on a relay that lost nothing.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;

/// The type of an HTTP/2 `HEADERS` frame.
const HEADERS: u8 = 0x1;

/// The relay, until it drops.
pub struct DropFirstAnswer {
    addr: SocketAddr,
    dropped: Arc<AtomicBool>,
    task: JoinHandle<()>,
}

impl Drop for DropFirstAnswer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl DropFirstAnswer {
    /// A relay to `upstream` on a port of `127.0.0.1` the system picks.
    pub async fn start(upstream: SocketAddr) -> DropFirstAnswer {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind a loopback port");
        let addr = listener.local_addr().expect("the relay's address");
        let dropped = Arc::new(AtomicBool::new(false));
        let task = tokio::spawn(accept(listener, upstream, Arc::clone(&dropped)));
        DropFirstAnswer {
            addr,
            dropped,
            task,
        }
    }

    /// The origin a client dials to go through the relay.
    pub fn origin(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// Whether the relay has lost an answer.
    pub fn dropped(&self) -> bool {
        self.dropped.load(Ordering::SeqCst)
    }
}

async fn accept(listener: TcpListener, upstream: SocketAddr, dropped: Arc<AtomicBool>) {
    let mut first = true;
    while let Ok((client, _)) = listener.accept().await {
        let Ok(server) = TcpStream::connect(upstream).await else {
            continue;
        };
        if std::mem::take(&mut first) {
            tokio::spawn(relay_dropping(client, server, Arc::clone(&dropped)));
        } else {
            tokio::spawn(relay(client, server));
        }
    }
}

/// Relays a connection whole.
async fn relay(mut client: TcpStream, mut server: TcpStream) {
    let _ = tokio::io::copy_bidirectional(&mut client, &mut server).await;
}

/// Relays a connection until the server sends the head of an answer, then closes it.
async fn relay_dropping(client: TcpStream, server: TcpStream, dropped: Arc<AtomicBool>) {
    let (mut client_read, mut client_write) = client.into_split();
    let (mut server_read, mut server_write) = server.into_split();
    let upward = tokio::spawn(async move {
        let _ = tokio::io::copy(&mut client_read, &mut server_write).await;
    });
    let mut pending: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 16 * 1024];
    // Until the server closes the connection or fails a read (a read of 0 bytes is its end).
    'read: while let Ok(read @ 1..) = server_read.read(&mut chunk).await {
        pending.extend_from_slice(&chunk[..read]);
        while let Some(len) = whole_frame(&pending) {
            if answer_head(&pending) {
                dropped.store(true, Ordering::SeqCst);
                break 'read;
            }
            if client_write.write_all(&pending[..len]).await.is_err() {
                break 'read;
            }
            pending.drain(..len);
        }
    }
    upward.abort();
    // Dropping both halves of both sockets closes the connection on each side.
}

/// The length of the frame at the start of `bytes` (its 9-byte head and its payload), when the
/// whole frame is there.
pub fn whole_frame(bytes: &[u8]) -> Option<usize> {
    let head = bytes.get(..9)?;
    let payload = usize::from(head[0]) << 16 | usize::from(head[1]) << 8 | usize::from(head[2]);
    let len = 9 + payload;
    (bytes.len() >= len).then_some(len)
}

/// Whether the frame at the start of `bytes` is a `HEADERS` frame of a stream other than the
/// connection's own: the head of an answer.
pub fn answer_head(bytes: &[u8]) -> bool {
    let stream = u32::from_be_bytes([bytes[5], bytes[6], bytes[7], bytes[8]]) & 0x7fff_ffff;
    bytes[3] == HEADERS && stream != 0
}
