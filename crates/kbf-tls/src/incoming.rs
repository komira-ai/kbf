//! The accepted connections of a listener whose TLS configuration can change.

use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll};
use std::time::Duration;

use futures::Stream;
use rustls::pki_types::CertificateDer;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;
use tokio::task::JoinSet;
use tokio_rustls::TlsAcceptor;
use tokio_rustls::server::TlsStream;
use tonic::Request;
use tonic::codegen::http::Extensions;
use tonic::transport::server::{Connected, TcpConnectInfo, TcpIncoming, TlsConnectInfo};

use crate::source::{Poller, TlsSource};

/// How long a client has to finish its handshake before the connection is dropped, so a
/// client that connects and says nothing holds nothing for long.
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// The chain a client presented, leaf first, as tonic reports it.
type Chain = Arc<Vec<CertificateDer<'static>>>;

/// What a service learns about a connection.
#[derive(Clone, Debug)]
pub struct ConnInfo {
    /// The TCP addresses.
    pub tcp: TcpConnectInfo,
    /// The certificates the client presented and the handshake verified; `None` for a
    /// plain connection or a TLS client that presented none.
    pub certs: Option<Chain>,
    /// Whether the connection is still there; see [`Liveness`].
    pub live: Liveness,
}

/// Whether a connection is gone, as its owner has seen it. It is set when a read finds
/// the peer closed, when a read or a write fails on the socket (a reset, a broken pipe, a
/// TLS failure), which happens before the HTTP/2 layer learns of it and ends the streams,
/// and when the connection is dropped. So a stream that ends while its connection is not
/// gone was ended by its peer on a connection that stays: a reset, not a loss. A loss the
/// socket does not report (a half-open connection, which only a keepalive timeout finds)
/// is set by the drop of the connection the server then closes, which can come a moment
/// after its streams end.
#[derive(Clone, Debug, Default)]
pub struct Liveness(Arc<AtomicBool>);

impl Liveness {
    /// Whether the connection is gone.
    #[must_use]
    pub fn is_gone(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }

    /// Records the connection gone.
    pub fn mark_gone(&self) {
        self.0.store(true, Ordering::Release);
    }
}

enum Io {
    Plain(TcpStream),
    Tls(Box<TlsStream<TcpStream>>),
}

/// An accepted connection: plain, or past its TLS handshake.
pub struct Conn {
    io: Io,
    info: ConnInfo,
}

impl Conn {
    /// The peer's address, if the socket still knows it.
    #[must_use]
    pub fn remote_addr(&self) -> Option<SocketAddr> {
        self.info.tcp.remote_addr
    }
}

impl Connected for Conn {
    type ConnectInfo = ConnInfo;

    fn connect_info(&self) -> ConnInfo {
        self.info.clone()
    }
}

/// Runs `$call` on the connection's stream and marks the connection gone if it failed:
/// a write the socket refused means the peer is lost, whatever the HTTP/2 layer has yet
/// seen.
macro_rules! each_io {
    ($self:expr, $io:ident => $call:expr) => {{
        let this = $self.get_mut();
        let polled = match &mut this.io {
            Io::Plain($io) => $call,
            Io::Tls($io) => $call,
        };
        if let Poll::Ready(Err(_)) = &polled {
            this.info.live.mark_gone();
        }
        polled
    }};
}

impl AsyncRead for Conn {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        let room = buf.remaining();
        let before = buf.filled().len();
        let polled = match &mut this.io {
            Io::Plain(io) => Pin::new(io).poll_read(cx, buf),
            Io::Tls(io) => Pin::new(io).poll_read(cx, buf),
        };
        if let Poll::Ready(read) = &polled {
            let ended = match read {
                Err(_) => true,
                // Ok with nothing read is the end of the stream, but only when the buffer
                // had room: a read into no room completes the same way on a live stream.
                Ok(()) => match room {
                    0 => false,
                    _ => buf.filled().len() == before,
                },
            };
            if ended {
                this.info.live.mark_gone();
            }
        }
        polled
    }
}

impl Drop for Conn {
    fn drop(&mut self) {
        self.info.live.mark_gone();
    }
}

impl AsyncWrite for Conn {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        each_io!(self, io => Pin::new(io).poll_write(cx, buf))
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        each_io!(self, io => Pin::new(io).poll_flush(cx))
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        each_io!(self, io => Pin::new(io).poll_shutdown(cx))
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        each_io!(self, io => Pin::new(io).poll_write_vectored(cx, bufs))
    }

    fn is_write_vectored(&self) -> bool {
        match &self.io {
            Io::Plain(io) => io.is_write_vectored(),
            Io::Tls(io) => io.is_write_vectored(),
        }
    }
}

/// The TLS half of an [`Incoming`]: the source, the handshakes running, and the task that
/// keeps the source current.
struct Tls {
    source: TlsSource,
    handshake: Duration,
    pending: JoinSet<Option<Conn>>,
    _poller: Poller,
}

/// The connections a listener accepted, as a stream a tonic server takes
/// (`serve_with_incoming`). With a [`TlsSource`], each is handshaken under the source's
/// configuration as of its accept, in a task of its own, so a client that is slow to
/// handshake delays no one; one whose handshake fails is dropped. Without, they are the
/// plain connections of `tcp`.
pub struct Incoming {
    tcp: TcpIncoming,
    tls: Option<Tls>,
}

impl Incoming {
    /// The connections of `tcp`, secured by `tls` if there is one. With one, starts the task
    /// that looks at its files every interval, which ends when this is dropped; so it must
    /// be called inside a Tokio runtime.
    #[must_use]
    pub fn new(tcp: TcpIncoming, tls: Option<TlsSource>) -> Self {
        Self {
            tcp,
            tls: tls.map(|source| Tls {
                handshake: HANDSHAKE_TIMEOUT,
                pending: JoinSet::new(),
                _poller: source.poll(),
                source,
            }),
        }
    }

    /// The same with `handshake` as the time a client has to complete its handshake.
    #[must_use]
    pub fn with_handshake_timeout(mut self, handshake: Duration) -> Self {
        if let Some(tls) = &mut self.tls {
            tls.handshake = handshake;
        }
        self
    }
}

impl Incoming {
    /// What accepting `tcp` leads to: a plain listener's connection at once, a TLS
    /// listener's handshake started in a task of its own (its connection comes from
    /// `poll_next` when it ends).
    fn admit(&mut self, tcp: TcpStream) -> Option<Conn> {
        let info = tcp.connect_info();
        let Some(tls) = &mut self.tls else {
            return Some(Conn {
                io: Io::Plain(tcp),
                info: ConnInfo {
                    tcp: info,
                    certs: None,
                    live: Liveness::default(),
                },
            });
        };
        let acceptor = TlsAcceptor::from(tls.source.config());
        tls.pending
            .spawn(handshake(acceptor, tcp, info, tls.handshake));
        None
    }
}

impl Stream for Incoming {
    type Item = io::Result<Conn>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        // A TCP listener's stream does not end, and an accept error is the stream's
        // item, which tonic logs and goes on from.
        while let Poll::Ready(Some(accepted)) = Pin::new(&mut this.tcp).poll_next(cx) {
            if let Some(ready) = accepted.map(|tcp| this.admit(tcp)).transpose() {
                return Poll::Ready(Some(ready));
            }
        }
        if let Some(tls) = &mut this.tls {
            // A handshake that failed ends as `None`: nothing for the server to serve.
            while let Poll::Ready(Some(done)) = tls.pending.poll_join_next(cx) {
                if let Ok(Some(conn)) = done {
                    return Poll::Ready(Some(Ok(conn)));
                }
            }
        }
        Poll::Pending
    }
}

async fn handshake(
    acceptor: TlsAcceptor,
    tcp: TcpStream,
    info: TcpConnectInfo,
    limit: Duration,
) -> Option<Conn> {
    match tokio::time::timeout(limit, acceptor.accept(tcp)).await {
        Ok(Ok(tls)) => {
            let certs = tls
                .get_ref()
                .1
                .peer_certificates()
                .map(|chain| Arc::new(chain.iter().map(|c| c.clone().into_owned()).collect()));
            Some(Conn {
                io: Io::Tls(Box::new(tls)),
                info: ConnInfo {
                    tcp: info,
                    certs,
                    live: Liveness::default(),
                },
            })
        }
        Ok(Err(e)) => {
            tracing::debug!(peer = ?info.remote_addr, error = %e, "TLS handshake failed");
            None
        }
        Err(_) => {
            tracing::debug!(peer = ?info.remote_addr, "TLS handshake timed out");
            None
        }
    }
}

/// The certificates the caller of `request` presented, leaf first: from a connection of an
/// [`Incoming`] or of tonic's own TLS acceptor. `None` on a plain connection or without a
/// client certificate.
#[must_use]
pub fn peer_certs<T>(request: &Request<T>) -> Option<Chain> {
    match request.extensions().get::<ConnInfo>() {
        Some(info) => info.certs.clone(),
        None => request.peer_certs(),
    }
}

/// [`peer_certs`] for a layer, which sees the HTTP request's extensions before tonic
/// has made a `Request` of it.
#[must_use]
pub fn peer_certs_of(extensions: &Extensions) -> Option<Chain> {
    match extensions.get::<ConnInfo>() {
        Some(info) => info.certs.clone(),
        None => extensions
            .get::<TlsConnectInfo<TcpConnectInfo>>()
            .and_then(TlsConnectInfo::peer_certs),
    }
}

/// The address of the caller of `request`, from either kind of connection.
#[must_use]
pub fn remote_addr<T>(request: &Request<T>) -> Option<SocketAddr> {
    match request.extensions().get::<ConnInfo>() {
        Some(info) => info.tcp.remote_addr,
        None => request.remote_addr(),
    }
}

/// Whether the connection `request` came on is gone, as an [`Incoming`] saw it. `None` for
/// a request that did not come through one: there is nothing to ask.
#[must_use]
pub fn liveness<T>(request: &Request<T>) -> Option<Liveness> {
    request
        .extensions()
        .get::<ConnInfo>()
        .map(|info| info.live.clone())
}
