//! The seam of the update API: the handler of `/v1/update/frontend*` and what it is given and
//! answers.
//!
//! The proxy serves the path itself and calls a handler for it. The handler `kbf-installer`
//! runs it with is [`NoUpdates`]: a proxy over a pool updates no frontend (a frontend is
//! drained through the admin API, restarted and put back by whoever updates it), so the path
//! answers 503.

use std::time::Duration;

use http::{HeaderName, HeaderValue};

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

/// The pluggable handler of the update API. The proxy authenticates first ([`crate::auth::decide`]
/// with the update class), enforces [`MAX_UPDATE_BODY`] and [`UPDATE_DEADLINE`], and then calls
/// this.
///
/// A client that got a 504 or lost its connection retries, so every command must be
/// idempotent and the handler must answer a repeat of a command in flight with the state of the
/// first, not start a second.
pub trait UpdateHandler: Send + Sync {
    /// Handles one request.
    fn handle(&self, request: UpdateRequest) -> impl Future<Output = UpdateReply> + Send;
}

/// The reply of the built-in handler: 503 and a JSON error.
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

    #[tokio::test]
    async fn the_built_in_handler_answers_503() {
        let reply = NoUpdates
            .handle(UpdateRequest {
                method: "GET".into(),
                path: "/v1/update/frontend".into(),
                body: Vec::new(),
            })
            .await;
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
}
