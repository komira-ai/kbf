//! The OCI front's socket: HTTPS connections, bounded in number, in the time a client has to send
//! a request, in how long a client may leave an answer unread, and in lifetime.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use hyper::Request;
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::{TokioIo, TokioTimer};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{AcquireError, OwnedSemaphorePermit, Semaphore, watch};
use tokio::task::JoinSet;
use tokio_rustls::TlsAcceptor;

use super::{OciError, OciFront};
use crate::server::idle::Watched;

/// How long [`OciListener::serve`] waits after a failed accept before the next.
const ACCEPT_PAUSE: Duration = Duration::from_millis(100);

/// The bounds on the connections of a listener.
#[derive(Clone, Copy, Debug)]
pub struct OciLimits {
    /// The most connections open at once; the others wait in the kernel's backlog.
    pub max_connections: usize,
    /// How long a client has to finish the TLS handshake and to send the head of a request,
    /// and so how long a connection may wait, idle, for its next request.
    pub header_timeout: Duration,
    /// How long a client may go without taking a byte of an answer before the connection is
    /// closed.
    pub write_idle: Duration,
    /// The longest a connection lives. A request being answered when it ends is finished first.
    pub lifetime: Duration,
    /// How long, after a stop, the connections still open are waited for before they are cut.
    pub drain_deadline: Duration,
}

/// A bound socket, ready to serve the front over TLS.
pub struct OciListener {
    listener: TcpListener,
    acceptor: TlsAcceptor,
    front: Arc<OciFront>,
    limits: OciLimits,
}

impl OciListener {
    /// Binds `addr`.
    ///
    /// # Errors
    /// [`OciError::Bind`] if the address cannot be bound.
    pub async fn bind(
        addr: SocketAddr,
        acceptor: TlsAcceptor,
        front: Arc<OciFront>,
        limits: OciLimits,
    ) -> Result<Self, OciError> {
        let bound = TcpListener::bind(addr).await;
        let listener = bound.map_err(|source| OciError::Bind { addr, source })?;
        Ok(Self {
            listener,
            acceptor,
            front,
            limits,
        })
    }

    /// The address the socket is bound to.
    ///
    /// # Errors
    /// The socket's address cannot be read.
    pub fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.listener.local_addr()
    }

    /// Serves connections until `stop` turns true: then it takes no more, lets the connections
    /// that are open finish the request they are answering and close, waits for them for
    /// [`OciLimits::drain_deadline`], and cuts those that are left. Returns how many were cut.
    pub async fn serve(self, mut stop: watch::Receiver<bool>) -> usize {
        let Self {
            listener,
            acceptor,
            front,
            limits,
        } = self;
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
                accepted = listener.accept() => match accepted {
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
        drop(listener);
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
}

/// Resolves once `stop` is true, or its sender is gone.
async fn stopped(stop: &mut watch::Receiver<bool>) {
    let _ = stop.wait_for(|stopped| *stopped).await;
}

/// What a connection needs besides its socket.
struct Serving {
    acceptor: TlsAcceptor,
    front: Arc<OciFront>,
    limits: OciLimits,
    stop: watch::Receiver<bool>,
}

impl Serving {
    /// One connection, until its client leaves, it breaks, its lifetime ends or the listener
    /// stops; the slot is held throughout.
    async fn connection(
        mut self,
        tcp: TcpStream,
        peer: SocketAddr,
        _slot: Result<OwnedSemaphorePermit, AcquireError>,
    ) {
        let limits = self.limits;
        let handshake = tokio::time::timeout(limits.header_timeout, self.acceptor.accept(tcp));
        let tls = match handshake.await {
            Ok(Ok(tls)) => tls,
            Ok(Err(error)) => return eprintln!("tierstore: oci: {peer}: TLS handshake: {error}"),
            Err(_) => return eprintln!("tierstore: oci: {peer}: TLS handshake took too long"),
        };
        let front = Arc::clone(&self.front);
        let service = service_fn(move |request: Request<Incoming>| {
            let front = Arc::clone(&front);
            async move {
                let (method, uri, headers) = (request.method(), request.uri(), request.headers());
                let answer = front.handle(method, uri.path(), headers).await;
                Ok::<_, Infallible>(answer)
            }
        });
        let io = TokioIo::new(Watched::new(tls, limits.write_idle, peer));
        let serving = http1::Builder::new()
            .timer(TokioTimer::new())
            .header_read_timeout(limits.header_timeout)
            .serve_connection(io, service);
        tokio::pin!(serving);
        let lifetime = tokio::time::sleep(limits.lifetime);
        tokio::pin!(lifetime);
        tokio::select! {
            // A connection that ends badly has nothing to tell the other side.
            _ = serving.as_mut() => return,
            () = &mut lifetime => {}
            () = stopped(&mut self.stop) => {}
        }
        // The request being answered is finished, and then the connection closes.
        serving.as_mut().graceful_shutdown();
        let _ = serving.await;
    }
}
