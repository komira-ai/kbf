//! The replay buffer and the retry rule (`kbf-proxy-api.md` section 10).
//!
//! A stream's request (headers plus data) is kept in a [`ReplayBuffer`] of [`REPLAY_BYTES`]
//! from its first byte. The buffer keeps filling as the client sends, until a data frame would
//! overflow it or the upstream's first response head arrives. Either closes the retry window
//! for good.
//!
//! A re-dispatch needs the whole request: the window must be open and the client's
//! END_STREAM seen ([`ReplayBuffer::replayable`]). A bidirectional call whose body is still
//! open is not re-dispatched (no splicing of a remainder onto a new upstream stream); the
//! client gets the answer, which a gRPC client retries by itself. The buffer owns `Bytes`
//! clones of the frames in U4b-1 (the skeleton counts them) and frees them at `close()`; a
//! long server-streaming call keeps at most [`REPLAY_BYTES`] until its first response frame.

use crate::route::is_bytestream;

/// The size of the replay window: the first 64 KB of a request.
pub const REPLAY_BYTES: usize = 64 * 1024;
/// Dispatches a stream may have: the first and one re-dispatch.
pub const MAX_ATTEMPTS: u8 = 2;

/// The request bytes kept for a re-dispatch. Not `Copy`: it is a state machine, and a copy
/// would fork it silently (U4b-1 also gives it `Bytes`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReplayBuffer {
    cap: usize,
    len: usize,
    open: bool,
    ended: bool,
}

impl ReplayBuffer {
    /// An empty buffer of `cap` bytes.
    #[must_use]
    pub const fn new(cap: usize) -> Self {
        Self {
            cap,
            len: 0,
            open: true,
            ended: false,
        }
    }

    /// Keeps `n` more request bytes. Returns false, and closes the window for good, if they
    /// do not fit or the window is already closed.
    pub fn push(&mut self, n: usize) -> bool {
        if self.open && self.len.saturating_add(n) <= self.cap {
            self.len += n;
            true
        } else {
            self.open = false;
            false
        }
    }

    /// Closes the window: the upstream answered, so the request may have had an effect.
    pub fn close(&mut self) {
        self.open = false;
    }

    /// Records the client's END_STREAM: the buffer now holds the whole request, if the
    /// window is still open.
    pub fn end_request(&mut self) {
        self.ended = true;
    }

    /// Whether the window is open: nothing has overflowed it and nothing has answered.
    #[must_use]
    pub const fn window_open(&self) -> bool {
        self.open
    }

    /// Whether the whole request is held (window open and the client's END_STREAM seen) and
    /// nothing has answered: the only state a re-dispatch is allowed from.
    #[must_use]
    pub const fn replayable(&self) -> bool {
        self.open && self.ended
    }

    /// Bytes held.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.len
    }

    /// Whether nothing is held.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }
}

/// How a dispatch failed before any upstream response.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Failure {
    /// The upstream connection could not be opened.
    Connect,
    /// The upstream sent `REFUSED_STREAM`: it did not process the stream.
    RefusedStream,
    /// The upstream's GOAWAY named a `last_stream_id` below this stream's id (including a
    /// write error on an idle pooled connection the peer had closed, with no request byte
    /// sent): provably unprocessed, so safe like `RefusedStream`. The connection is marked
    /// draining on the first GOAWAY, graceful two-phase form included.
    GoAway,
    /// Anything else (a reset mid-request, a protocol error): not safe to repeat.
    Other,
}

/// Whether the proxy may dispatch the request again. The retry is a new placement, not a
/// reuse: the caller drops the old permit (after resetting the old upstream stream) and picks
/// a member again with `attempts + 1`. `attempts` counts dispatches so far. Never for
/// ByteStream, never after an upstream response, never once the request outgrew the buffer or
/// while the client's request is still open.
#[must_use]
pub fn may_retry(path: &str, failure: Failure, buffer: &ReplayBuffer, attempts: u8) -> bool {
    !is_bytestream(path)
        && attempts < MAX_ATTEMPTS
        && buffer.replayable()
        && matches!(
            failure,
            Failure::Connect | Failure::RefusedStream | Failure::GoAway
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXEC: &str = "/build.bazel.remote.execution.v2.Execution/Execute";

    #[test]
    fn the_window_closes_on_overflow_and_stays_closed() {
        let mut b = ReplayBuffer::new(10);
        assert!(b.is_empty());
        assert!(b.push(6));
        assert!(b.push(4));
        assert_eq!(b.len(), 10);
        assert!(b.window_open());
        assert!(!b.push(1));
        assert!(!b.window_open());
        b.end_request();
        assert!(!b.replayable(), "an overflowed request is never whole");
        assert!(!b.push(0));
    }

    fn whole(bytes: usize) -> ReplayBuffer {
        let mut b = ReplayBuffer::new(REPLAY_BYTES);
        assert!(b.push(bytes));
        b.end_request();
        b
    }

    /// Asserts that a request that was whole may be dispatched again until the upstream answers,
    /// and never after: closing the window (the first response head) ends the retries.
    #[test]
    fn an_upstream_response_closes_the_window() {
        let mut b = whole(100);
        assert!(may_retry(EXEC, Failure::Connect, &b, 1));
        b.close();
        assert!(!b.window_open());
        assert!(!may_retry(EXEC, Failure::Connect, &b, 1));
    }

    #[test]
    fn an_open_request_is_never_redispatched() {
        let mut b = ReplayBuffer::new(REPLAY_BYTES);
        assert!(b.push(100));
        assert!(b.window_open());
        assert!(!b.replayable());
        for f in [Failure::Connect, Failure::RefusedStream, Failure::GoAway] {
            assert!(
                !may_retry("/kbf.worker.v1.Worker/Session", f, &b, 1),
                "{f:?}"
            );
        }
        b.end_request();
        assert!(may_retry(
            "/kbf.worker.v1.Worker/Session",
            Failure::GoAway,
            &b,
            1
        ));
    }

    #[test]
    fn only_a_safe_failure_is_retried_and_never_bytestream() {
        let b = whole(0);
        for (f, want) in [
            (Failure::Connect, true),
            (Failure::RefusedStream, true),
            (Failure::GoAway, true),
            (Failure::Other, false),
        ] {
            assert_eq!(may_retry(EXEC, f, &b, 1), want, "{f:?}");
            assert!(
                !may_retry("/google.bytestream.ByteStream/Write", f, &b, 1),
                "{f:?}"
            );
        }
        assert!(!may_retry(EXEC, Failure::Connect, &b, MAX_ATTEMPTS));
        // A request that overflowed the buffer is never replayed, even on a refused stream.
        let mut big = ReplayBuffer::new(1);
        assert!(!big.push(2));
        big.end_request();
        assert!(!may_retry(EXEC, Failure::RefusedStream, &big, 1));
    }
}
