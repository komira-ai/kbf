//! A client attempt on a TLS listener that ends: one connection, each wait bounded. For the
//! tests that replace a listener's TLS files and try clients against it (tests/tls_reload.rs,
//! tests/storage_tls_reload.rs).
//!
//! A wait with no end hangs the test, and the CI job that runs it, instead of failing it:
//! [`within`] bounds a wait and fails naming what it waited for. A client attempt dials once
//! ([`DialOnce`], through [`connect`]), so one that is refused is one connection that ended,
//! not a channel that dials again for as long as the server refuses it.

use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll};
use std::time::Duration;

use futures::StreamExt;
use hyper_util::rt::TokioIo;
use kbf_tls::{Incoming, TlsSource};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tonic::codegen::Service;
use tonic::codegen::http::Uri;
use tonic::transport::server::TcpIncoming;
use tonic::transport::{Channel, Endpoint};

/// `future`'s output, or a panic naming `what` if it is not ready within `limit`.
pub async fn within<T>(limit: Duration, what: &str, future: impl Future<Output = T>) -> T {
    match tokio::time::timeout(limit, future).await {
        Ok(output) => output,
        Err(_) => panic!("timed out after {limit:?} waiting for {what}"),
    }
}

/// A connector that dials once, for a client attempt.
///
/// A tonic `Channel` dials again, at once, whenever its connection has closed before a
/// request was sent on it, inside `connect()` and the call waiting behind it. A listener
/// refusing a client certificate closes the connection that way: TLS 1.3 finishes the
/// client's handshake before the server judges the certificate, so the connection looks
/// open until the server's alert ends it, and whether the client's HTTP/2 task or that
/// alert is first is a race, so a refused attempt can open a second connection, and a
/// third, each refused the same way. Here the second dial fails, and an attempt is one
/// connection: one that is refused is one that ended.
#[derive(Default)]
pub struct DialOnce {
    dialed: bool,
}

impl Service<Uri> for DialOnce {
    type Response = TokioIo<TcpStream>;
    type Error = io::Error;
    type Future = Pin<Box<dyn Future<Output = io::Result<Self::Response>> + Send>>;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, uri: Uri) -> Self::Future {
        let first = !std::mem::replace(&mut self.dialed, true);
        Box::pin(async move {
            if !first {
                return Err(io::Error::other(
                    "the connection closed, and this client dials once",
                ));
            }
            let authority = uri
                .authority()
                .ok_or_else(|| io::Error::other("the address has no host and port"))?;
            let stream = TcpStream::connect(authority.as_str()).await?;
            // As tonic's own connector does: without it a small call waits out the delayed
            // acknowledgement of the one before.
            stream.set_nodelay(true)?;
            Ok(TokioIo::new(stream))
        })
    }
}

/// The channel over one connection to `endpoint`, once its TLS handshake is done within
/// `limit`; `None` if the connection or the handshake fails (a refused server
/// certificate, say).
pub async fn connect(limit: Duration, endpoint: Endpoint) -> Option<Channel> {
    let connecting = endpoint.connect_with_connector(DialOnce::default());
    within(limit, "the connection and TLS handshake", connecting)
        .await
        .ok()
}

/// A TLS listener that completes each handshake and then ends the session: every
/// connection a client makes to it closes before a request can go out on it, the case in
/// which a `Channel` dials again. It counts the connections it accepted, and the ones the
/// client has let go of.
///
/// The server is kbf-tls's own `Incoming`, so a connection it yields is one whose
/// handshake it finished. It counts the connection, ends the TLS session, and counts
/// again when the client has let go of the socket. The client lets go once it has seen
/// the end (its connection task finishes, which drops the socket and, in the same poll,
/// the channel to it), so a call made after [`Closing::let_go`] finds a closed
/// connection, without a sleep. The connection is counted before it is closed, so the
/// count is exact when the attempt ends.
pub struct Closing {
    pub addr: SocketAddr,
    accepted: Arc<AtomicUsize>,
    let_go: Arc<AtomicUsize>,
}

impl Closing {
    /// Serves the certificate `cert` with key `key` on a loopback port of its choosing,
    /// asking for no client certificate.
    pub fn start(cert: &str, key: &str) -> Self {
        let source = TlsSource::fixed("closing", cert.as_bytes(), key.as_bytes(), None)
            .expect("a TLS configuration");
        let tcp = TcpIncoming::bind(SocketAddr::from(([127, 0, 0, 1], 0))).expect("bind");
        let addr = tcp.local_addr().expect("address");
        let accepted = Arc::new(AtomicUsize::new(0));
        let let_go = Arc::new(AtomicUsize::new(0));
        let (counted, counted_let_go) = (Arc::clone(&accepted), Arc::clone(&let_go));
        tokio::spawn(async move {
            let mut incoming = Incoming::new(tcp, Some(source));
            while let Some(Ok(mut conn)) = incoming.next().await {
                counted.fetch_add(1, Ordering::SeqCst);
                let counted_let_go = Arc::clone(&counted_let_go);
                tokio::spawn(async move {
                    let _ = conn.shutdown().await;
                    // Ends when the client closes its end, cleanly or not.
                    let _ = conn.read_to_end(&mut Vec::new()).await;
                    counted_let_go.fetch_add(1, Ordering::SeqCst);
                });
            }
        });
        Self {
            addr,
            accepted,
            let_go,
        }
    }

    /// Waits, for at most `limit`, until the client has let go of `n` connections.
    pub async fn let_go(&self, limit: Duration, n: usize) {
        let letting_go = async {
            while self.let_go.load(Ordering::SeqCst) < n {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        };
        within(
            limit,
            "the client to let go of a closed connection",
            letting_go,
        )
        .await;
    }

    /// The connections accepted so far.
    pub fn accepted(&self) -> usize {
        self.accepted.load(Ordering::SeqCst)
    }
}
