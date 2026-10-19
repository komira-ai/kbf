//! The update API as a handler: one request as plain values in, one reply as plain values
//! out (`docs/design/frontend-proxy.md` section 4, unit U8b). That is the whole surface a
//! listener needs from the API, and it keeps the listener's own types on the listener's
//! side of the call.
//!
//! [`Handler`] owns the [`actor`](super::actor) (on a thread of its own, see there) and a
//! [`Router`] over it, and answers a [`HandlerRequest`] with a [`HandlerReply`].
//!
//! **The caller authenticates.** A request that reaches [`Handler::handle`] is taken to
//! come from the operator: the router's token check and rate limit
//! ([`auth::BearerAuth`](super::auth::BearerAuth)) are not in this path, because the
//! listener in front checks the credential before it calls, and a second check of a
//! credential that is not passed down would never see one. Do not expose a [`Handler`]
//! anywhere a caller has not been authenticated.
//!
//! **No forwarding.** The router's forwarded paths (`/v1/update/desired` and the like) are
//! answered 404 here: the listener sends those to the child itself.

use std::io;
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use hyper::header::{CONTENT_TYPE, HeaderName, HeaderValue};
use hyper::{Request, Response, StatusCode};

use super::actor::{self, ActorTiming};
use super::auth::{Authorize, Denied, Principal};
use super::core::{ApiError, Core};
use super::router::{self, Router};
use super::traits::{ForwardError, Forwarder};

/// A request to the update API, after the listener authenticated it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HandlerRequest {
    /// `GET` or `POST`.
    pub method: String,
    /// The path, `/v1/update/frontend` and what follows it.
    pub path: String,
    /// The JSON body.
    pub body: Vec<u8>,
}

/// The answer to a [`HandlerRequest`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HandlerReply {
    /// HTTP status.
    pub status: u16,
    /// Response headers other than `Content-Type`, for example `Allow` on a 405. The body
    /// is always JSON; the listener says so.
    pub headers: Vec<(HeaderName, HeaderValue)>,
    /// JSON body.
    pub body: Vec<u8>,
}

/// Every request is from the operator: see the module doc.
struct AlreadyAuthenticated;

impl Authorize for AlreadyAuthenticated {
    fn authorize(&self, _req: &Request<Bytes>) -> Result<Principal, Denied> {
        Ok(Principal::Operator)
    }
}

/// The forwarded paths are the listener's to send on, not this handler's.
struct NoForward;

#[async_trait]
impl Forwarder for NoForward {
    async fn forward(&self, _req: Request<Bytes>) -> Result<Response<Bytes>, ForwardError> {
        Ok(router::error(&ApiError {
            status: StatusCode::NOT_FOUND,
            code: "not_found",
            message: "no such route".to_owned(),
            extra: None,
        }))
    }
}

pub struct Handler {
    router: Router,
}

impl Handler {
    /// Starts the actor over `core` and returns the handler in front of it.
    ///
    /// `on_death` runs once, on the actor's thread, if the actor ends without every handle
    /// to it having been dropped (see [`actor::spawn`]); the caller passes what stops its
    /// process. From then on every request is answered 503.
    pub fn start(
        core: Core,
        timing: ActorTiming,
        on_death: impl FnOnce() + Send + 'static,
    ) -> io::Result<Handler> {
        let api = actor::spawn(core, timing, on_death)?;
        Ok(Handler {
            router: Router::new(Arc::new(AlreadyAuthenticated), api, Arc::new(NoForward)),
        })
    }

    /// Answers one request. Never waits on the actor longer than the deadline of
    /// [`ActorTiming`], and never blocks the task it runs on.
    pub async fn handle(&self, call: HandlerRequest) -> HandlerReply {
        let response = match request(call) {
            Ok(request) => self.router.call(request).await,
            Err(refused) => router::error(&refused),
        };
        reply(response)
    }
}

/// The request as the router reads it, with no credential and no peer: there is none.
fn request(call: HandlerRequest) -> Result<Request<Bytes>, ApiError> {
    Request::builder()
        .method(call.method.as_str())
        .uri(call.path)
        .body(Bytes::from(call.body))
        .map_err(|e| ApiError {
            status: StatusCode::BAD_REQUEST,
            code: "bad_request",
            message: e.to_string(),
            extra: None,
        })
}

fn reply(response: Response<Bytes>) -> HandlerReply {
    let (head, body) = response.into_parts();
    HandlerReply {
        status: head.status.as_u16(),
        headers: head
            .headers
            .iter()
            .filter(|(name, _)| *name != CONTENT_TYPE)
            .map(|(name, value)| (name.clone(), value.clone()))
            .collect(),
        body: body.to_vec(),
    }
}
