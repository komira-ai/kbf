//! The frontend's listening sockets, bound here, and the latch that closes one.

use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{Context, Poll, Waker};

use futures::Stream;
use kbf_tls::{Incoming, TlsSource};
use tonic::transport::server::TcpIncoming;

use crate::serve::ServeError;

/// A bound TCP listener, not yet serving.
pub(crate) struct Socket {
    /// Where it listens.
    pub local: SocketAddr,
    listener: std::net::TcpListener,
}

impl Socket {
    /// A listener on `addr`.
    ///
    /// # Errors
    /// The address cannot be bound.
    pub fn bind(addr: SocketAddr) -> Result<Self, ServeError> {
        let bound = std::net::TcpListener::bind(addr).and_then(|listener| {
            listener.set_nonblocking(true)?;
            let local = listener.local_addr()?;
            Ok(Self { local, listener })
        });
        bound.map_err(bind_error(addr))
    }

    /// The listener for a gRPC server, and the latch that closes it: connections it
    /// accepts have Nagle's algorithm off (see [`crate::serve::bind`]), and are served
    /// over TLS when `tls` is given ([`Incoming`]: it must be called inside a Tokio
    /// runtime).
    ///
    /// # Errors
    /// The runtime cannot register the socket.
    pub fn grpc(self, tls: Option<TlsSource>) -> Result<(Stoppable, Latch), ServeError> {
        let registered = tokio::net::TcpListener::from_std(self.listener);
        let listener = registered.map_err(bind_error(self.local))?;
        let incoming = TcpIncoming::from(listener).with_nodelay(Some(true));
        let open = Arc::new(Mutex::new(Open {
            incoming: Some(Incoming::new(incoming, tls)),
            waker: None,
        }));
        Ok((Stoppable(Arc::clone(&open)), Latch(open)))
    }

    /// The listener for the operator API.
    ///
    /// # Errors
    /// The runtime cannot register the socket.
    pub fn http(self) -> Result<tokio::net::TcpListener, ServeError> {
        let registered = tokio::net::TcpListener::from_std(self.listener);
        registered.map_err(bind_error(self.local))
    }
}

/// The error of a socket on `addr` that could not be made or registered.
fn bind_error(addr: SocketAddr) -> impl FnOnce(io::Error) -> ServeError {
    move |source| ServeError::Bind { addr, source }
}

/// What a gRPC listener holds: the socket, until it is closed, and the waker of the task
/// waiting to accept on it.
struct Open {
    incoming: Option<Incoming>,
    waker: Option<Waker>,
}

/// A gRPC listener that can be closed from outside: [`Latch::close`] ends the stream of
/// connections and closes the socket at once, wherever the server is in accepting. The
/// transport's own shutdown keeps the socket open until the last connection is gone, and
/// stops accepting only when it next looks at the listener; a process that stops
/// listening (a handover stops the worker listener) must not go on holding the socket
/// open, where connections would queue for nobody.
pub(crate) struct Stoppable(Arc<Mutex<Open>>);

impl Stream for Stoppable {
    type Item = <Incoming as Stream>::Item;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let mut open = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        open.waker = Some(cx.waker().clone());
        match open.incoming.as_mut() {
            Some(incoming) => Pin::new(incoming).poll_next(cx),
            None => Poll::Ready(None),
        }
    }
}

impl Drop for Stoppable {
    fn drop(&mut self) {
        Latch(Arc::clone(&self.0)).close();
    }
}

/// Closes a [`Stoppable`].
#[derive(Clone)]
pub(crate) struct Latch(Arc<Mutex<Open>>);

impl Latch {
    /// Closes the socket, and wakes the task waiting on it so that it sees the stream end.
    pub fn close(&self) {
        let mut open = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        open.incoming = None;
        if let Some(waker) = open.waker.take() {
            waker.wake();
        }
    }
}

#[cfg(test)]
mod tests;
