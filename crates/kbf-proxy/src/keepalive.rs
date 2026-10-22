//! PING keepalive, both sides (design note section 10).
//!
//! **Downstream** a PING goes to every client connection every `keepalive_interval_s` of
//! silence, and a connection whose peer does not answer within `keepalive_ack_s` is closed. It
//! closes dead peers, never streams: a live client answers the PING whatever its streams are
//! doing, so a 3 hour Execute that has been quiet for hours is untouched. Hyper does the
//! pinging; this module only configures it.
//!
//! **Upstream** a PING goes to every child connection every [`UPSTREAM_INTERVAL`]. It keeps a
//! quiet loopback connection warm and tells the pool when a child has stopped answering. An
//! unanswered PING does not close the connection: a quiesce or a database takeover can stall
//! a loopback child for longer than [`UPSTREAM_ACK`] and its streams are still good. It marks
//! the connection *suspect* (the pool puts no new stream on it) until the PING is answered,
//! and only [`UPSTREAM_BACKSTOP`] without an answer ends it.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use h2::{Ping, PingPong};
use hyper::server::conn::http2;
use hyper_util::rt::{TokioExecutor, TokioTimer};
use tokio::sync::Notify;

use crate::config::Limits;

/// How often an upstream connection is pinged.
pub(crate) const UPSTREAM_INTERVAL: Duration = Duration::from_secs(30);
/// How long an upstream PING may go unanswered before the connection is suspect.
pub(crate) const UPSTREAM_ACK: Duration = Duration::from_secs(20);
/// How long an upstream PING may go unanswered before the connection is closed.
pub(crate) const UPSTREAM_BACKSTOP: Duration = Duration::from_secs(300);

/// Turns on the downstream PING (and gives hyper the timer it needs for it).
pub(crate) fn downstream(http: &mut http2::Builder<TokioExecutor>, limits: &Limits) {
    http.timer(TokioTimer::new())
        .keep_alive_interval(Duration::from_secs(limits.keepalive_interval_s))
        .keep_alive_timeout(Duration::from_secs(limits.keepalive_ack_s));
}

/// Pings an upstream connection for as long as it answers, and returns when it should be
/// closed: the peer has not answered for [`UPSTREAM_BACKSTOP`], or the connection is gone.
/// `suspect` is set while a PING is unanswered past [`UPSTREAM_ACK`] and cleared by the answer,
/// which rings `bell` so a stream parked in the pool for want of a usable connection looks again.
pub(crate) async fn upstream(mut ping: PingPong, suspect: &AtomicBool, bell: &Notify) {
    loop {
        tokio::time::sleep(UPSTREAM_INTERVAL).await;
        // One PING at a time: h2 refuses a second while one is pending, so the same future is
        // waited on through the late answer and not dropped at the first timeout.
        let pong = ping.ping(Ping::opaque());
        tokio::pin!(pong);
        let first = tokio::time::timeout(UPSTREAM_ACK, pong.as_mut()).await;
        let answered = match first {
            Err(_) => {
                suspect.store(true, Ordering::Release);
                tokio::time::timeout(UPSTREAM_BACKSTOP - UPSTREAM_ACK, pong).await
            }
            on_time => on_time,
        };
        if !matches!(answered, Ok(Ok(_))) {
            return;
        }
        suspect.store(false, Ordering::Release);
        bell.notify_waiters();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    /// Catches a suspicion that is never lifted: a child that answers a PING late (a quiesce
    /// that ended) is used for new streams again, and a stream parked at the pool's cap is woken. The server end is not polled for 60 s, so
    /// it reads the PING of 30 s, and answers it, only then.
    #[tokio::test(start_paused = true)]
    async fn a_late_answer_clears_the_suspicion() {
        let (near, far) = tokio::io::duplex(1 << 16);
        let server = tokio::spawn(async move {
            let mut conn = h2::server::handshake(far).await.expect("server handshake");
            tokio::time::sleep(Duration::from_secs(60)).await;
            // Answers PINGs while it is polled, for as long as the test needs.
            let _ = tokio::time::timeout(Duration::from_secs(100), conn.accept()).await;
        });
        let (_send, mut connection) = h2::client::handshake(near).await.expect("client handshake");
        let ping = connection.ping_pong().expect("a fresh connection");
        tokio::spawn(connection);
        let suspect = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&suspect);
        let bell = Arc::new(Notify::new());
        let ring = Arc::clone(&bell);
        let pinging = tokio::spawn(async move { upstream(ping, &flag, &ring).await });
        let waiter = Arc::clone(&bell);
        let parked = tokio::spawn(async move { waiter.notified().await });

        tokio::time::sleep(Duration::from_secs(40)).await;
        assert!(!suspect.load(Ordering::Acquire), "20 s of grace");
        tokio::time::sleep(Duration::from_secs(15)).await;
        assert!(suspect.load(Ordering::Acquire), "unanswered at 55 s");
        tokio::time::sleep(Duration::from_secs(10)).await;
        assert!(!suspect.load(Ordering::Acquire), "answered at 60 s");
        assert!(parked.is_finished(), "the answer rings the pool's bell");
        assert!(
            !pinging.is_finished(),
            "an answered connection is not closed"
        );
        server.await.expect("server");
    }
}
