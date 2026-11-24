//! The listeners' test of a client that is gone: a connection is closed once it has moved
//! nothing either way for a while, in place of hyper's test, an HTTP/2 PING left
//! unanswered for a while.
//!
//! hyper writes its PING behind the bytes the server has already written on the
//! connection, and while it waits for the answer it counts nothing else the client sends.
//! So a client reading a download over a slow path, whose answer comes only once the
//! megabytes ahead of the PING have crossed, was taken for gone and its connection closed,
//! failing every action on it ("h2 protocol error: error reading a body from connection"
//! in buck2). Here the connection is moving while the client sends anything at all, or
//! while the socket takes bytes it had held back, which it does only as the client
//! acknowledges the bytes ahead of them.

use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::task::{Context, Poll, ready};
use std::time::Duration;

use futures::{Stream, StreamExt as _};
use kbf_tls::Conn;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::time::{Instant, Sleep};
use tonic::transport::server::Connected;

use crate::serve::StreamTiming;

/// How long hyper waits for the answer to its PING before it closes the connection: far
/// longer than [`limit`], so that the watch decides, not hyper. hyper's PING stays, sent on
/// a connection that has read nothing for the keepalive interval, because a client that is
/// there answers it, and the answer is a read that moves the connection.
pub(crate) const PING_ANSWER_WAIT: Duration = Duration::from_secs(24 * 60 * 60);

/// How long a connection of the listeners may move nothing before it is closed: the PING
/// interval and the keepalive timeout (`--http2-keepalive-interval-secs`,
/// `--http2-keepalive-timeout-secs`), as long as a client that is gone was given when the
/// PING's answer decided. None with the interval 0, which sends no PING, so a quiet
/// connection would have nothing to read.
pub(crate) fn limit(timing: StreamTiming) -> Option<Duration> {
    let on = !timing.keepalive_interval.is_zero();
    on.then(|| (timing.keepalive_interval).saturating_add(timing.keepalive_timeout))
}

/// The connections of a listener, `incoming`, each closed once it has moved nothing for
/// the [`limit`] of `timing`.
pub(crate) fn watched<S>(
    incoming: S,
    timing: StreamTiming,
) -> impl Stream<Item = io::Result<Watched<Conn>>>
where
    S: Stream<Item = io::Result<Conn>>,
{
    let limit = limit(timing);
    incoming.map(move |accepted| {
        accepted.map(|conn| {
            let peer = conn.remote_addr();
            Watched::new(conn, limit, peer)
        })
    })
}

/// A served connection, `io`, whose reads and writes that wait fail (`TimedOut`) once it
/// has moved nothing for its limit: no read brought a byte, and no write the socket had held
/// back went through. A write the socket takes at once proves nothing (the socket buffers
/// it whether the client is there or not), nor does an empty read.
///
/// Its reads and writes are polled by one task, as hyper's connection is: the limit's timer
/// wakes the task that last found the connection waiting.
pub(crate) struct Watched<IO> {
    io: IO,
    watch: Option<Watch>,
}

struct Watch {
    limit: Duration,
    /// Since when nothing has moved.
    moved: Instant,
    /// Whether the socket held back the last write.
    held: bool,
    /// Whether the limit has passed: the connection fails from then on.
    expired: bool,
    timer: Pin<Box<Sleep>>,
    /// For the line logged when the limit passes.
    peer: Option<SocketAddr>,
}

impl<IO> Watched<IO> {
    /// `io`, closed after `limit` without moving; with no limit, `io` as it is. `peer` is
    /// named in the line logged when the limit passes.
    pub(crate) fn new(io: IO, limit: Option<Duration>, peer: Option<SocketAddr>) -> Self {
        let watch = limit.map(|limit| {
            let moved = Instant::now();
            Watch {
                limit,
                moved,
                held: false,
                expired: false,
                timer: Box::pin(tokio::time::sleep_until(moved + limit)),
                peer,
            }
        });
        Self { io, watch }
    }

    /// What `polled`, a write or a flush, means for the watch: one the socket held back
    /// waits for the limit like a read does, and one it takes after holding back the one
    /// before moved the connection.
    fn wrote<T>(
        &mut self,
        cx: &mut Context<'_>,
        polled: Poll<io::Result<T>>,
    ) -> Poll<io::Result<T>> {
        let Some(watch) = &mut self.watch else {
            return polled;
        };
        match polled {
            Poll::Pending => {
                watch.held = true;
                watch.poll_limit(cx).map(Err)
            }
            Poll::Ready(Ok(done)) if watch.held => {
                watch.moved();
                Poll::Ready(Ok(done))
            }
            ready => ready,
        }
    }
}

impl Watch {
    /// Records that the connection moved.
    fn moved(&mut self) {
        self.moved = Instant::now();
        self.held = false;
    }

    /// Pending until the limit has passed since the connection last moved, then the error
    /// the connection fails with, from then on: the line saying why is logged once.
    fn poll_limit(&mut self, cx: &mut Context<'_>) -> Poll<io::Error> {
        let secs = self.limit.as_secs();
        if !self.expired {
            let deadline = self.moved + self.limit;
            if self.timer.deadline() != deadline {
                self.timer.as_mut().reset(deadline);
            }
            ready!(self.timer.as_mut().poll(cx));
            self.expired = true;
            tracing::warn!(
                peer = ?self.peer,
                secs,
                "closing a connection that moved nothing either way for the keepalive's limit: no byte read from the client, none it held back sent"
            );
        }
        Poll::Ready(io::Error::new(
            io::ErrorKind::TimedOut,
            format!("the connection moved nothing either way for {secs} s"),
        ))
    }
}

impl<IO: AsyncRead + Unpin> AsyncRead for Watched<IO> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        let before = buf.filled().len();
        let polled = Pin::new(&mut this.io).poll_read(cx, buf);
        let Some(watch) = &mut this.watch else {
            return polled;
        };
        match polled {
            Poll::Pending => watch.poll_limit(cx).map(Err),
            Poll::Ready(read) => {
                if buf.filled().len() > before {
                    watch.moved();
                }
                Poll::Ready(read)
            }
        }
    }
}

impl<IO: AsyncWrite + Unpin> AsyncWrite for Watched<IO> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        let polled = Pin::new(&mut this.io).poll_write(cx, buf);
        this.wrote(cx, polled)
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        let polled = Pin::new(&mut this.io).poll_write_vectored(cx, bufs);
        this.wrote(cx, polled)
    }

    fn is_write_vectored(&self) -> bool {
        self.io.is_write_vectored()
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        let polled = Pin::new(&mut this.io).poll_flush(cx);
        this.wrote(cx, polled)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().io).poll_shutdown(cx)
    }
}

impl<IO: Connected> Connected for Watched<IO> {
    type ConnectInfo = IO::ConnectInfo;

    fn connect_info(&self) -> Self::ConnectInfo {
        self.io.connect_info()
    }
}
