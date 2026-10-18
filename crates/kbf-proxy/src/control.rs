//! The seams U7 and U8 use: the update-API handler and the status the proxy reports.
//!
//! The admin calls on a child (`takeover`, `quiesce`, `stand_down`, `resume`, `role`) are not
//! here. The proxy makes none of them: U7a's supervisor is their only caller, so the trait
//! (`ChildControl`) lives in `kbf-installer` with the supervisor, and the proxy only supplies
//! the child's admin port ([`crate::config::UpstreamTable::admin_port`]) and the headers that
//! authenticate a call to it ([`crate::identity::admin_headers`]).

use std::time::Duration;

use http::{HeaderName, HeaderValue};

use crate::slot::Slot;

/// The proxy's count for one slot, for `GET /v1/update/frontend`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SlotStats {
    /// Streams bound to the slot, held-then-released and queued ones included.
    pub open_streams: u64,
}

/// One read of everything `GET /v1/update/frontend` reports. The fields are read one after
/// another, so a stream between release and bind is in none of the counts: the snapshot is
/// a hint, and only `ActiveSlot::retire` is authoritative.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ProxySnapshot {
    /// The active slot.
    pub active: Option<Slot>,
    /// Whether new streams are being held.
    pub holding: bool,
    /// Streams in the hold queue.
    pub held: usize,
    /// Each slot's stats, A then B.
    pub per_slot: [SlotStats; 2],
    /// Whether `/readyz` would answer 200 (an active child and a poll that passed).
    pub ready: bool,
    /// Holds that ended by a guard's drop, nobody having released them into a slot.
    pub holds_abandoned: u64,
}

impl ProxySnapshot {
    /// The stats of `slot`.
    #[must_use]
    pub const fn stats(&self, slot: Slot) -> SlotStats {
        match slot {
            Slot::A => self.per_slot[0],
            Slot::B => self.per_slot[1],
        }
    }
}

/// The largest update-API body the proxy passes on; a larger one is answered 413 by the
/// proxy ([`UpdateReply::payload_too_large`]) without calling the handler.
pub const MAX_UPDATE_BODY: usize = 4096;
/// How long the proxy waits for the handler; after that it answers 504
/// ([`UpdateReply::timed_out`]) to the client and lets the handler finish: the proxy runs the
/// handler in its own task, so a command a client gave up on is neither cut short nor run twice.
pub const UPDATE_DEADLINE: Duration = Duration::from_secs(30);

/// A request to `/v1/update/frontend*`, after the proxy authenticated and rate limited it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UpdateRequest {
    /// `GET` or `POST`.
    pub method: String,
    /// The path, `/v1/update/frontend` and what follows it.
    pub path: String,
    /// The JSON body, at most [`MAX_UPDATE_BODY`] bytes.
    pub body: Vec<u8>,
}

impl UpdateRequest {
    /// Whether a body of `len` bytes may reach the handler.
    #[must_use]
    pub const fn body_fits(len: usize) -> bool {
        len <= MAX_UPDATE_BODY
    }
}

/// The answer to an [`UpdateRequest`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UpdateReply {
    /// HTTP status.
    pub status: u16,
    /// Extra response headers, for example `Retry-After` on a 409 or 429. `Content-Type:
    /// application/json` is always set by the proxy.
    pub headers: Vec<(HeaderName, HeaderValue)>,
    /// JSON body.
    pub body: Vec<u8>,
}

impl UpdateReply {
    /// A reply with no extra headers.
    #[must_use]
    pub const fn json(status: u16, body: Vec<u8>) -> Self {
        Self {
            status,
            headers: Vec::new(),
            body,
        }
    }

    /// The same reply with one more header.
    #[must_use]
    pub fn with_header(mut self, name: HeaderName, value: HeaderValue) -> Self {
        self.headers.push((name, value));
        self
    }

    /// The proxy's own answer to a body over [`MAX_UPDATE_BODY`].
    #[must_use]
    pub fn payload_too_large() -> Self {
        Self::json(413, br#"{"error":"the body is over 4096 bytes"}"#.to_vec())
    }

    /// The proxy's own answer to a handler that passed [`UPDATE_DEADLINE`].
    #[must_use]
    pub fn timed_out() -> Self {
        Self::json(
            504,
            br#"{"error":"the update handler did not answer"}"#.to_vec(),
        )
    }
}

/// The pluggable handler of the update API: U8a's actor. The proxy authenticates first
/// ([`crate::auth::decide`] with the update class, the one place that later swaps to OIDC),
/// rate limits, enforces [`MAX_UPDATE_BODY`] and [`UPDATE_DEADLINE`], and then calls this.
/// The handler owns no state machine: it asks U7a's actor (a trait in `kbf-installer`) for
/// `abort`, `rollback` and `force-retire`, and U7a alone writes `update.json`.
///
/// A client that got a 504 or lost its connection retries, so every command must be
/// idempotent and the handler must answer a repeat of a command in flight with the state of the
/// first, not start a second.
pub trait UpdateHandler: Send + Sync {
    /// Handles one request.
    fn handle(&self, request: UpdateRequest) -> impl Future<Output = UpdateReply> + Send;
}

/// The reply of the built-in handler until U8 lands: 503 and a JSON error.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoUpdates;

impl UpdateHandler for NoUpdates {
    async fn handle(&self, _request: UpdateRequest) -> UpdateReply {
        UpdateReply::json(503, br#"{"error":"updates are not wired"}"#.to_vec())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::pin::pin;
    use std::task::{Context, Poll, Waker};

    fn ready<F: Future>(f: F) -> F::Output {
        let mut f = pin!(f);
        match f.as_mut().poll(&mut Context::from_waker(Waker::noop())) {
            Poll::Ready(v) => v,
            Poll::Pending => panic!("the future is not ready"),
        }
    }

    #[test]
    fn until_u8_the_update_api_answers_503() {
        let reply = ready(NoUpdates.handle(UpdateRequest {
            method: "GET".into(),
            path: "/v1/update/frontend".into(),
            body: Vec::new(),
        }));
        assert_eq!(reply.status, 503);
        assert!(reply.headers.is_empty());
        assert!(reply.body.starts_with(b"{"));
    }

    #[test]
    fn a_reply_carries_headers_and_the_proxys_own_answers_are_json() {
        let reply = UpdateReply::json(429, b"{}".to_vec())
            .with_header(http::header::RETRY_AFTER, HeaderValue::from_static("30"));
        assert_eq!(reply.headers.len(), 1);
        assert_eq!(reply.headers[0].0, http::header::RETRY_AFTER);
        assert_eq!(reply.headers[0].1, "30");
        for own in [UpdateReply::payload_too_large(), UpdateReply::timed_out()] {
            assert!(own.body.starts_with(b"{\"error\""));
        }
        assert_eq!(UpdateReply::payload_too_large().status, 413);
        assert_eq!(UpdateReply::timed_out().status, 504);
    }

    #[test]
    fn the_body_bound_is_4096_bytes_inclusive() {
        assert!(UpdateRequest::body_fits(0));
        assert!(UpdateRequest::body_fits(4096));
        assert!(!UpdateRequest::body_fits(4097));
        assert_eq!(UPDATE_DEADLINE, Duration::from_secs(30));
    }

    #[test]
    fn a_snapshot_reads_each_slot_apart() {
        let snap = ProxySnapshot {
            active: Some(Slot::B),
            holding: true,
            held: 3,
            per_slot: [SlotStats { open_streams: 200 }, SlotStats::default()],
            ready: false,
            holds_abandoned: 1,
        };
        assert_eq!(snap.stats(Slot::A).open_streams, 200);
        assert_eq!(snap.stats(Slot::B), SlotStats::default());
        assert_eq!(ProxySnapshot::default().active, None);
    }
}
