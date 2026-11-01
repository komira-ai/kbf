//! A static file server on loopback, the baseline of cell G: what a tree read costs when the
//! store is as thin as it can be, HTTP/1.1 with keep-alive over files already on disk.
//!
//! It serves `GET /<relative path>` from a directory: `200` with the file, `404` for a file that
//! is not there, `400` for anything it cannot read or a path that leaves the directory. Nothing
//! else is a feature of it.

use std::net::SocketAddr;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use tokio::io::{AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;

use crate::error::BenchError;

/// Longest request head the server reads.
const MAX_HEAD: usize = 8192;

/// Largest body copied behind its head to go out in one write; a larger one is its own write,
/// which TCP_NODELAY keeps from waiting.
const ONE_WRITE_MAX: usize = 64 * 1024;

/// A running server.
pub struct FileServer {
    addr: SocketAddr,
    connections: Arc<AtomicU64>,
    /// Accepted connections whose socket had TCP_NODELAY set, for the tests.
    #[cfg(test)]
    nodelay: Arc<AtomicU64>,
    task: JoinHandle<()>,
}

impl FileServer {
    /// Starts serving `root` on a free loopback port.
    ///
    /// # Errors
    /// [`BenchError::Setup`] if no port can be bound.
    pub async fn start(root: PathBuf) -> Result<Self, BenchError> {
        Self::start_on(root, SocketAddr::from(([127, 0, 0, 1], 0))).await
    }

    /// Starts serving `root` on `addr`.
    ///
    /// # Errors
    /// [`BenchError::Setup`] if `addr` cannot be bound.
    pub async fn start_on(root: PathBuf, addr: SocketAddr) -> Result<Self, BenchError> {
        let listener = TcpListener::bind(addr).await.map_err(unavailable)?;
        let addr = listener.local_addr().map_err(unavailable)?;
        let connections = Arc::new(AtomicU64::new(0));
        let counted = connections.clone();
        #[cfg(test)]
        let nodelay = Arc::new(AtomicU64::new(0));
        #[cfg(test)]
        let nodelay_seen = nodelay.clone();
        let task = tokio::spawn(async move {
            loop {
                // An accept that fails (out of descriptors) is tried again after other tasks ran.
                listener.accept().await.into_iter().for_each(|(socket, _)| {
                    counted.fetch_add(1, Ordering::SeqCst);
                    // Best effort: a socket that refuses the option is still served, and a small
                    // answer is one write either way.
                    socket.set_nodelay(true).ok();
                    #[cfg(test)]
                    if socket.nodelay().unwrap_or(false) {
                        nodelay_seen.fetch_add(1, Ordering::SeqCst);
                    }
                    tokio::spawn(serve(socket, root.clone()));
                });
                tokio::task::yield_now().await;
            }
        });
        Ok(Self {
            addr,
            connections,
            #[cfg(test)]
            nodelay,
            task,
        })
    }

    /// The base URL, `http://127.0.0.1:<port>`.
    #[must_use]
    pub fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// Connections accepted so far.
    #[must_use]
    pub fn connections(&self) -> u64 {
        self.connections.load(Ordering::SeqCst)
    }

    /// Stops accepting. Connections already open end when their client closes them.
    pub fn stop(&self) {
        self.task.abort();
    }
}

fn unavailable(e: std::io::Error) -> BenchError {
    BenchError::Setup(format!("file server: {e}"))
}

/// The relative path a request line asks for, if it is `GET /<path> HTTP/1.x` and the path stays
/// inside the directory.
fn requested(head: &str) -> Option<PathBuf> {
    let mut words = head.lines().next()?.split(' ');
    let (method, target, version) = (words.next()?, words.next()?, words.next()?);
    let relative = Path::new(target.strip_prefix('/')?);
    let inside = relative
        .components()
        .all(|c| matches!(c, Component::Normal(_)));
    (method == "GET" && version.starts_with("HTTP/1.") && inside).then(|| relative.to_owned())
}

async fn respond<W: AsyncWrite + Unpin>(
    socket: &mut W,
    status: &str,
    body: &[u8],
) -> std::io::Result<()> {
    let head = format!(
        "HTTP/1.1 {status}\r\ncontent-length: {}\r\n\r\n",
        body.len()
    );
    if body.len() <= ONE_WRITE_MAX {
        // One write, so the answer never waits on the peer's ACK of its own head (Nagle against
        // a delayed ACK is ~40 ms a request).
        let mut answer = head.into_bytes();
        answer.extend_from_slice(body);
        socket.write_all(&answer).await
    } else {
        socket.write_all(head.as_bytes()).await?;
        socket.write_all(body).await
    }
}

/// Serves one connection until the client closes it or sends what is not a request.
async fn serve(mut socket: TcpStream, root: PathBuf) {
    let mut buffer = Vec::new();
    loop {
        let end = loop {
            if let Some(at) = buffer.windows(4).position(|w| w == b"\r\n\r\n") {
                break at + 4;
            }
            let mut chunk = [0u8; 1024];
            match socket.read(&mut chunk).await {
                Ok(n) if n > 0 && buffer.len() < MAX_HEAD => buffer.extend_from_slice(&chunk[..n]),
                _ => return,
            }
        };
        let head = String::from_utf8_lossy(&buffer[..end]).into_owned();
        buffer.drain(..end);
        let answered = match requested(&head) {
            None => respond(&mut socket, "400 Bad Request", b"").await,
            Some(relative) => {
                let path = root.join(relative);
                match tokio::task::spawn_blocking(move || std::fs::read(path)).await {
                    Ok(Ok(body)) => respond(&mut socket, "200 OK", &body).await,
                    _ => respond(&mut socket, "404 Not Found", b"").await,
                }
            }
        };
        if answered.is_err() {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::pin::Pin;
    use std::task::{Context, Poll};

    /// A writer that keeps what it is given and counts the writes it took it in.
    #[derive(Default)]
    struct Counting {
        writes: usize,
        bytes: Vec<u8>,
    }

    impl AsyncWrite for Counting {
        fn poll_write(
            mut self: Pin<&mut Self>,
            _: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            self.writes += 1;
            self.bytes.extend_from_slice(buf);
            Poll::Ready(Ok(buf.len()))
        }
        fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
        fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    /// Catches an answer sent as a head write and then a body write: on a socket without
    /// TCP_NODELAY the second small write waits for the peer's delayed ACK of the first. A small
    /// answer is one write, head and body together, and its bytes are the whole response.
    #[tokio::test]
    async fn a_small_answer_is_written_in_one_write() {
        let mut out = Counting::default();
        respond(&mut out, "200 OK", b"hello").await.unwrap();
        assert_eq!(out.writes, 1);
        assert_eq!(
            out.bytes,
            b"HTTP/1.1 200 OK\r\ncontent-length: 5\r\n\r\nhello"
        );
    }

    /// Catches a connection served with Nagle on: every connection the server accepts has
    /// TCP_NODELAY set, however many clients there are.
    #[tokio::test]
    async fn every_accepted_connection_has_nodelay_set() {
        let dir = dir("tb-fileserver-nodelay");
        let server = FileServer::start(dir.path().to_owned()).await.unwrap();
        let addr = server.url().trim_start_matches("http://").to_owned();
        for _ in 0..2 {
            let mut socket = TcpStream::connect(&addr).await.unwrap();
            socket
                .write_all(b"GET /d000/f1 HTTP/1.1\r\n\r\n")
                .await
                .unwrap();
            let mut first = [0u8; 12];
            socket.read_exact(&mut first).await.unwrap();
        }
        assert_eq!(server.connections(), 2);
        assert_eq!(server.nodelay.load(Ordering::SeqCst), 2);
        server.stop();
    }

    fn dir(name: &str) -> kbf_testkit::dir::TestDir {
        let dir = kbf_testkit::dir::TestDir::under(std::env::temp_dir(), name);
        std::fs::create_dir_all(dir.path().join("d000")).unwrap();
        std::fs::write(dir.path().join("d000/f1"), b"hello").unwrap();
        dir
    }

    /// Catches a server that does not give a file's bytes, calls a missing file present, or opens
    /// a connection per request (keep-alive is the point of the baseline): three requests of one
    /// client use one connection.
    #[tokio::test]
    async fn it_serves_files_over_one_kept_alive_connection() {
        let dir = dir("tb-fileserver");
        let server = FileServer::start(dir.path().to_owned()).await.unwrap();
        let client = reqwest::Client::new();
        for _ in 0..3 {
            let got = client
                .get(format!("{}/d000/f1", server.url()))
                .send()
                .await
                .unwrap();
            assert_eq!(got.status(), 200);
            assert_eq!(got.bytes().await.unwrap().as_ref(), b"hello");
        }
        assert_eq!(server.connections(), 1);
        let missing = client
            .get(format!("{}/d000/none", server.url()))
            .send()
            .await
            .unwrap();
        assert_eq!(missing.status(), 404);
        server.stop();
    }

    /// Catches a path that leaves the directory being read, a request that is not a GET being
    /// served, and a client that sends junk or closes early keeping the connection busy.
    #[tokio::test]
    async fn it_refuses_what_it_cannot_serve_and_ends_with_the_client() {
        let dir = dir("tb-fileserver-bad");
        let server = FileServer::start(dir.path().to_owned()).await.unwrap();
        let addr = server.url().trim_start_matches("http://").to_owned();
        let ask = |request: &'static str| {
            let addr = addr.clone();
            async move {
                let mut socket = TcpStream::connect(addr).await.unwrap();
                socket.write_all(request.as_bytes()).await.unwrap();
                let mut answer = vec![0u8; 12];
                socket.read_exact(&mut answer).await.unwrap();
                String::from_utf8(answer).unwrap()
            }
        };
        let bad = "HTTP/1.1 400";
        assert_eq!(ask("GET /../tb-outside HTTP/1.1\r\n\r\n").await, bad);
        assert_eq!(ask("GET //etc/passwd HTTP/1.1\r\n\r\n").await, bad);
        assert_eq!(ask("POST /d000/f1 HTTP/1.1\r\n\r\n").await, bad);
        assert_eq!(ask("nonsense\r\n\r\n").await, bad);
        assert_eq!(ask("GET d000/f1 HTTP/1.1\r\n\r\n").await, bad);
        assert_eq!(ask("GET /d000/f1 HTTP/2.0\r\n\r\n").await, bad);
        let mut socket = TcpStream::connect(&addr).await.unwrap();
        socket.write_all(b"GET /d000/f1 HT").await.unwrap();
        drop(socket);
        let mut flood = TcpStream::connect(&addr).await.unwrap();
        // The server may close on the flood before it is all sent: either end of that is fine.
        flood.write_all(&[b'a'; MAX_HEAD + 1024]).await.ok();
        let mut rest = Vec::new();
        flood.read_to_end(&mut rest).await.ok();
        assert!(rest.is_empty());
        server.stop();
    }

    /// Catches a client that goes away in the middle of an answer taking the server down with
    /// it: a second client is served after the first left half way through 8 MiB.
    #[tokio::test]
    async fn a_client_that_leaves_in_the_middle_of_an_answer_does_not_stop_the_server() {
        let dir = dir("tb-fileserver-leave");
        std::fs::write(dir.path().join("big"), vec![1u8; 8 << 20]).unwrap();
        let server = FileServer::start(dir.path().to_owned()).await.unwrap();
        let addr = server.url().trim_start_matches("http://").to_owned();
        let mut socket = TcpStream::connect(&addr).await.unwrap();
        socket
            .write_all(b"GET /big HTTP/1.1\r\n\r\n")
            .await
            .unwrap();
        let mut first = [0u8; 16];
        socket.read_exact(&mut first).await.unwrap();
        drop(socket);
        let again = reqwest::get(format!("{}/d000/f1", server.url()))
            .await
            .unwrap();
        assert_eq!(again.bytes().await.unwrap().as_ref(), b"hello");
        assert_eq!(server.connections(), 2);
        server.stop();
    }

    /// Catches a port that cannot be bound being taken for a server: starting on an address that
    /// is in use is a setup error.
    #[tokio::test]
    async fn an_address_in_use_is_a_setup_error() {
        let dir = dir("tb-fileserver-busy");
        let taken = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = taken.local_addr().unwrap();
        let err = FileServer::start_on(dir.path().to_owned(), addr)
            .await
            .err()
            .unwrap();
        assert!(err.to_string().starts_with("file server: "), "{err}");
    }
}
