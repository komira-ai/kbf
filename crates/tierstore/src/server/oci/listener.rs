//! The OCI front's socket: HTTPS connections, or plain HTTP ones, bounded in number,
//! in the time a client has to send a request, in how long a client may leave an answer unread,
//! and in lifetime.

use std::convert::Infallible;
use std::future::poll_fn;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use hyper::Request;
use hyper::body::Incoming as RequestBody;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::{TokioIo, TokioTimer};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
use tokio::sync::{AcquireError, OwnedSemaphorePermit, Semaphore, watch};
use tokio::task::JoinSet;
use tokio_rustls::TlsAcceptor;

use super::{OciError, OciFront, OciMode};
use crate::server::idle::Watched;
use crate::server::listen::{self, Listeners};
use crate::server::{ACCEPT_PAUSE, Incoming};

/// A connection's byte stream, whichever way it is carried: one code path serves HTTP over it.
trait Io: AsyncRead + AsyncWrite + Unpin + Send {}

impl<T: AsyncRead + AsyncWrite + Unpin + Send> Io for T {}

/// The bounds on the connections of a listener.
#[derive(Clone, Copy, Debug)]
pub struct OciLimits {
    /// The most connections open at once; the others wait in the kernel's backlog.
    pub max_connections: usize,
    /// How long a client has to finish the TLS handshake. Short, since a connection that never
    /// speaks holds a slot until it is closed. A plain listener has no handshake to time.
    pub handshake_timeout: Duration,
    /// How long a client has to send the head of a request, and so how long a connection may
    /// wait, idle, for its next request.
    pub header_timeout: Duration,
    /// How long a client may go without taking a byte of an answer before the connection is
    /// closed.
    pub write_idle: Duration,
    /// The longest a connection lives. A request being answered when it ends is finished first,
    /// for at most [`drain_deadline`](Self::drain_deadline) more.
    pub lifetime: Duration,
    /// How long, after a stop or at the end of a connection's lifetime, the answer being sent is
    /// waited for before the connection is cut.
    pub drain_deadline: Duration,
}

/// The bound sockets of the front, ready to serve it over TLS, or over plain HTTP. They share
/// one limit on connections and one stop.
pub struct OciListener {
    listeners: Listeners,
    /// `None` serves plain HTTP.
    acceptor: Option<TlsAcceptor>,
    front: Arc<OciFront>,
    limits: OciLimits,
}

impl OciListener {
    /// Binds `addr`, to serve HTTPS.
    ///
    /// # Errors
    /// [`OciError::Bind`] if the address cannot be bound.
    pub async fn bind(
        addr: SocketAddr,
        acceptor: TlsAcceptor,
        front: Arc<OciFront>,
        limits: OciLimits,
    ) -> Result<Self, OciError> {
        Self::bind_as(addr, Some(acceptor), front, limits).await
    }

    /// Binds `addr`, to serve plain HTTP; any address is bound (the start warns about one that
    /// is not loopback, see [`plain_warning`](super::plain_warning)).
    ///
    /// # Errors
    /// [`OciError::Bind`] if the address cannot be bound.
    pub async fn bind_plain(
        addr: SocketAddr,
        front: Arc<OciFront>,
        limits: OciLimits,
    ) -> Result<Self, OciError> {
        Self::bind_as(addr, None, front, limits).await
    }

    async fn bind_as(
        addr: SocketAddr,
        acceptor: Option<TlsAcceptor>,
        front: Arc<OciFront>,
        limits: OciLimits,
    ) -> Result<Self, OciError> {
        Self::bind_all(&[addr], acceptor, front, limits).await
    }

    /// Binds a socket for each of `addrs`, to serve HTTPS with `acceptor`, or plain HTTP
    /// without one: one front, one limit on connections and one stop for all of them. An
    /// IPv6 address is bound for IPv6 only, so that `0.0.0.0:P` and `[::]:P` can both be.
    ///
    /// # Errors
    /// [`OciError::Bind`] for the first address that cannot be bound.
    pub async fn bind_all(
        addrs: &[SocketAddr],
        acceptor: Option<TlsAcceptor>,
        front: Arc<OciFront>,
        limits: OciLimits,
    ) -> Result<Self, OciError> {
        let bound = listen::bind_all(addrs);
        let bound = bound.map_err(|(addr, source)| OciError::Bind { addr, source })?;
        Ok(Self {
            listeners: Listeners::new(bound),
            acceptor,
            front,
            limits,
        })
    }

    /// How the listener carries its connections.
    #[must_use]
    pub fn mode(&self) -> OciMode {
        if self.acceptor.is_some() {
            OciMode::Https
        } else {
            OciMode::Http
        }
    }

    /// The address of the first socket, the one the front was first asked to bind.
    ///
    /// # Errors
    /// The socket's address cannot be read, or the front has no socket.
    pub fn local_addr(&self) -> std::io::Result<SocketAddr> {
        let first = self.local_addrs()?.into_iter().next();
        first.ok_or_else(|| std::io::Error::other("the front has no socket"))
    }

    /// The address each socket is bound to, in the order they were asked for.
    ///
    /// # Errors
    /// A socket's address cannot be read.
    pub fn local_addrs(&self) -> std::io::Result<Vec<SocketAddr>> {
        self.listeners.local_addrs()
    }

    /// Serves connections until `stop` turns true: then it takes no more, lets the connections
    /// that are open finish the request they are answering and close, waits for them for
    /// [`OciLimits::drain_deadline`], and cuts those that are left. Returns how many were cut.
    pub async fn serve(self, stop: watch::Receiver<bool>) -> usize {
        let Self {
            listeners,
            acceptor,
            front,
            limits,
        } = self;
        accept_loop(listeners, acceptor, front, limits, stop).await
    }
}

/// [`OciListener::serve`] over any source of connections: the listener of a socket, and in the
/// tests one that fails.
pub(super) async fn accept_loop<I>(
    mut incoming: I,
    acceptor: Option<TlsAcceptor>,
    front: Arc<OciFront>,
    limits: OciLimits,
    mut stop: watch::Receiver<bool>,
) -> usize
where
    I: Incoming<Stream = TcpStream> + Send,
{
    let slots = Arc::new(Semaphore::new(limits.max_connections));
    let mut connections = JoinSet::new();
    loop {
        // A slot is taken before the next accept, so a flood waits in the backlog and not
        // in tasks.
        let slot = tokio::select! {
            slot = Arc::clone(&slots).acquire_owned() => slot,
            () = stopped(&mut stop) => break,
        };
        tokio::select! {
            accepted = poll_fn(|cx| incoming.poll_incoming(cx)) => match accepted {
                Ok((tcp, peer)) => {
                    let serving = Serving {
                        acceptor: acceptor.clone(),
                        front: Arc::clone(&front),
                        limits,
                        stop: stop.clone(),
                    };
                    connections.spawn(serving.connection(tcp, peer, slot));
                }
                Err(error) => {
                    eprintln!("tierstore: oci: accept: {error}");
                    tokio::time::sleep(ACCEPT_PAUSE).await;
                }
            },
            Some(_) = connections.join_next() => {}
            () = stopped(&mut stop) => break,
        }
    }
    drop(incoming);
    let finished = async { while connections.join_next().await.is_some() {} };
    if tokio::time::timeout(limits.drain_deadline, finished)
        .await
        .is_ok()
    {
        return 0;
    }
    let cut = connections.len();
    connections.abort_all();
    cut
}

/// Resolves once `stop` is true, or its sender is gone.
async fn stopped(stop: &mut watch::Receiver<bool>) {
    let _ = stop.wait_for(|stopped| *stopped).await;
}

/// What a connection needs besides its socket.
struct Serving {
    acceptor: Option<TlsAcceptor>,
    front: Arc<OciFront>,
    limits: OciLimits,
    stop: watch::Receiver<bool>,
}

impl Serving {
    /// The byte stream of the connection: `tcp` itself when the listener is plain, and else the
    /// stream of its TLS handshake, which has [`OciLimits::handshake_timeout`] to finish. `None`
    /// (and a line on stderr) if the handshake fails or takes too long.
    async fn open(&self, tcp: TcpStream, peer: SocketAddr) -> Option<Box<dyn Io>> {
        let Some(acceptor) = &self.acceptor else {
            return Some(Box::new(tcp));
        };
        let handshake = tokio::time::timeout(self.limits.handshake_timeout, acceptor.accept(tcp));
        match handshake.await {
            Ok(Ok(tls)) => Some(Box::new(tls)),
            Ok(Err(error)) => {
                eprintln!("tierstore: oci: {peer}: TLS handshake: {error}");
                None
            }
            Err(_) => {
                eprintln!("tierstore: oci: {peer}: TLS handshake took too long");
                None
            }
        }
    }

    /// One connection, until its client leaves, it breaks, its lifetime ends or the listener
    /// stops; the slot is held throughout.
    async fn connection(
        mut self,
        tcp: TcpStream,
        peer: SocketAddr,
        _slot: Result<OwnedSemaphorePermit, AcquireError>,
    ) {
        let limits = self.limits;
        let Some(stream) = self.open(tcp, peer).await else {
            return;
        };
        let front = Arc::clone(&self.front);
        let service = service_fn(move |request: Request<RequestBody>| {
            let front = Arc::clone(&front);
            async move {
                let (method, uri, headers) = (request.method(), request.uri(), request.headers());
                let answer = front.handle(method, uri.path(), headers).await;
                Ok::<_, Infallible>(answer)
            }
        });
        let io = TokioIo::new(Watched::new(stream, limits.write_idle, peer));
        let serving = http1::Builder::new()
            .timer(TokioTimer::new())
            .header_read_timeout(limits.header_timeout)
            .serve_connection(io, service);
        tokio::pin!(serving);
        let lifetime = tokio::time::sleep(limits.lifetime);
        tokio::pin!(lifetime);
        let lifetime_ended = tokio::select! {
            // A connection that ends badly has nothing to tell the other side.
            _ = serving.as_mut() => return,
            () = &mut lifetime => true,
            () = stopped(&mut self.stop) => false,
        };
        // The request being answered is finished, and then the connection closes.
        serving.as_mut().graceful_shutdown();
        if lifetime_ended {
            // A client that takes the answer a few bytes at a time must not keep the slot past
            // the lifetime for ever. (After a stop, the listener cuts what is left.)
            let _ = tokio::time::timeout(limits.drain_deadline, serving).await;
        } else {
            let _ = serving.await;
        }
    }
}
