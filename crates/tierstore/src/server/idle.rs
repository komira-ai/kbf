//! A connection that is closed when its client takes no byte of an answer for too long.

use std::future::Future;
use std::io::{self, IoSlice};
use std::net::SocketAddr;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::time::{Sleep, sleep};

/// A byte stream whose writes must make progress: a write that cannot take a byte because the
/// peer does not read it fails with `TimedOut` once that has gone on for the idle time, and a
/// line is logged. A write that takes any byte starts the time again, so a peer that reads
/// slowly and steadily is served however long the whole takes. Reads pass through: a request
/// body has its own limit on idle time (see `Reader`), and the head of a request has the
/// header timeout.
pub(crate) struct Watched<T> {
    inner: T,
    idle: Duration,
    peer: SocketAddr,
    /// Runs while the writes find the stream full; it is gone once a write is taken.
    stalled: Option<Pin<Box<Sleep>>>,
}

impl<T> Watched<T> {
    /// `inner`, whose writes may stall for `idle` at most. `peer` is named in the log line.
    pub(crate) fn new(inner: T, idle: Duration, peer: SocketAddr) -> Self {
        Self {
            inner,
            idle,
            peer,
            stalled: None,
        }
    }

    /// Passes on the outcome of a write: a write that was taken stops the time, and a write that
    /// was not starts it, if it is not running, and fails once it is over.
    fn watch(
        &mut self,
        cx: &mut Context<'_>,
        written: Poll<io::Result<usize>>,
    ) -> Poll<io::Result<usize>> {
        if written.is_ready() {
            self.stalled = None;
            return written;
        }
        let idle = self.idle;
        let timer = self.stalled.get_or_insert_with(|| Box::pin(sleep(idle)));
        match timer.as_mut().poll(cx) {
            Poll::Ready(()) => {
                let peer = self.peer;
                eprintln!("tierstore: {peer}: no byte of the answer taken for {idle:?}, closed");
                Poll::Ready(Err(io::Error::from(io::ErrorKind::TimedOut)))
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

impl<T: AsyncRead + Unpin> AsyncRead for Watched<T> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_read(cx, buf)
    }
}

impl<T: AsyncWrite + Unpin> AsyncWrite for Watched<T> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        let written = Pin::new(&mut this.inner).poll_write(cx, buf);
        this.watch(cx, written)
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        let written = Pin::new(&mut this.inner).poll_write_vectored(cx, bufs);
        this.watch(cx, written)
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}
