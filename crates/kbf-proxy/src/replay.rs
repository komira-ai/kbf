//! The replay buffer and the retry rule (design section 3, "Properties kept from the Envoy
//! spec", and "Fenced-not-committed").
//!
//! **One buffer serves hold and retry.** A stream's request (headers plus data) is kept in a
//! [`ReplayBuffer`] of [`REPLAY_BYTES`] from its first byte. While the stream is held the
//! buffer is what the hold gate holds (the window is the same 64 KB); once the stream is
//! bound the buffer keeps filling as the client sends, until a data frame would overflow it
//! or the upstream's first response byte arrives. Either closes the retry window for good.

use http::HeaderMap;

use crate::route::is_bytestream;

/// The size of the replay window: the first 64 KB of a request.
pub const REPLAY_BYTES: usize = 64 * 1024;
/// Dispatches a stream may have: the first and one re-dispatch.
pub const MAX_ATTEMPTS: u8 = 2;
/// The header a child sets on a trailers-only answer when it is certain nothing committed.
pub const FENCED: &str = "x-kbf-fenced";
/// The one value of [`FENCED`] the proxy acts on.
pub const FENCED_VALUE: &str = "nothing-committed";
/// The gRPC status of a fenced answer: `UNAVAILABLE`, so a client that is shown it (because
/// the window closed) retries by itself, which is correct because nothing committed.
pub const FENCED_GRPC_STATUS: &str = "14";

/// The request bytes kept for a re-dispatch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReplayBuffer {
    cap: usize,
    len: usize,
    open: bool,
}

impl ReplayBuffer {
    /// An empty buffer of `cap` bytes.
    #[must_use]
    pub const fn new(cap: usize) -> Self {
        Self {
            cap,
            len: 0,
            open: true,
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

    /// Whether the whole request so far is held and nothing has answered.
    #[must_use]
    pub const fn replayable(&self) -> bool {
        self.open
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
    /// The child answered "fenced, nothing committed" (see [`is_fenced`]).
    Fenced,
    /// Anything else (a reset mid-request, a protocol error): not safe to repeat.
    Other,
}

/// Whether the proxy may dispatch the request again, to the slot that is active now.
/// `attempts` counts dispatches so far. Never for ByteStream, never after an upstream
/// response, never once the request outgrew the buffer.
#[must_use]
pub fn may_retry(path: &str, failure: Failure, buffer: &ReplayBuffer, attempts: u8) -> bool {
    !is_bytestream(path)
        && attempts < MAX_ATTEMPTS
        && buffer.replayable()
        && matches!(
            failure,
            Failure::Connect | Failure::RefusedStream | Failure::Fenced
        )
}

/// Whether a child's answer is the fenced-not-committed answer: a trailers-only response
/// (`end_stream` on the headers frame) with `grpc-status` 14 and exactly one [`FENCED`]
/// header whose value is [`FENCED_VALUE`].
#[must_use]
pub fn is_fenced(headers: &HeaderMap, end_stream: bool) -> bool {
    let mut marker = headers.get_all(FENCED).iter();
    end_stream
        && headers
            .get("grpc-status")
            .is_some_and(|s| s == FENCED_GRPC_STATUS)
        && matches!((marker.next(), marker.next()), (Some(v), None) if v == FENCED_VALUE)
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::HeaderValue;

    const EXEC: &str = "/build.bazel.remote.execution.v2.Execution/Execute";

    #[test]
    fn the_window_closes_on_overflow_and_stays_closed() {
        let mut b = ReplayBuffer::new(10);
        assert!(b.is_empty());
        assert!(b.push(6));
        assert!(b.push(4));
        assert_eq!(b.len(), 10);
        assert!(b.replayable());
        assert!(!b.push(1));
        assert!(!b.replayable());
        assert!(!b.push(0));
    }

    #[test]
    fn an_upstream_response_closes_the_window() {
        let mut b = ReplayBuffer::new(REPLAY_BYTES);
        assert!(b.push(100));
        assert!(may_retry(EXEC, Failure::Connect, &b, 1));
        b.close();
        assert!(!may_retry(EXEC, Failure::Connect, &b, 1));
    }

    #[test]
    fn only_a_safe_failure_is_retried_and_never_bytestream() {
        let b = ReplayBuffer::new(REPLAY_BYTES);
        for (f, want) in [
            (Failure::Connect, true),
            (Failure::RefusedStream, true),
            (Failure::Fenced, true),
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
        assert!(!may_retry(EXEC, Failure::RefusedStream, &big, 1));
    }

    #[test]
    fn the_fenced_answer_is_trailers_only_status_14_and_one_marker() {
        let mut h = HeaderMap::new();
        h.insert("grpc-status", HeaderValue::from_static("14"));
        h.insert(FENCED, HeaderValue::from_static(FENCED_VALUE));
        assert!(is_fenced(&h, true));
        assert!(
            !is_fenced(&h, false),
            "a body followed: something may have run"
        );
        h.append(FENCED, HeaderValue::from_static(FENCED_VALUE));
        assert!(!is_fenced(&h, true), "two markers are not one");
        h.insert(FENCED, HeaderValue::from_static("yes"));
        assert!(!is_fenced(&h, true));
        h.insert(FENCED, HeaderValue::from_static(FENCED_VALUE));
        h.insert("grpc-status", HeaderValue::from_static("13"));
        assert!(!is_fenced(&h, true));
        h.remove("grpc-status");
        assert!(!is_fenced(&h, true));
        assert!(!is_fenced(&HeaderMap::new(), true));
    }
}
