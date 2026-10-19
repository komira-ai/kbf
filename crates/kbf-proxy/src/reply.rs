//! The answers the proxy gives itself: `UNAVAILABLE`, 404, the probes, the update API's reply.

use bytes::Bytes;
use http::{HeaderMap, HeaderValue, Response, StatusCode, header};

use crate::auth::Refusal;
use crate::body::ProxyBody;
use crate::control::UpdateReply;

/// `grpc-status` of `UNAVAILABLE`.
pub(crate) const GRPC_UNAVAILABLE: &str = "14";
/// `grpc-status` of `UNAUTHENTICATED`.
pub(crate) const GRPC_UNAUTHENTICATED: &str = "16";
/// `grpc-status` of `PERMISSION_DENIED`.
pub(crate) const GRPC_PERMISSION_DENIED: &str = "7";
/// `grpc-status` of `RESOURCE_EXHAUSTED`.
pub(crate) const GRPC_RESOURCE_EXHAUSTED: &str = "8";
/// `grpc-status` of `UNIMPLEMENTED`.
pub(crate) const GRPC_UNIMPLEMENTED: &str = "12";
/// The content type gRPC clients send and expect.
pub(crate) const GRPC_CONTENT_TYPE: &str = "application/grpc";

/// Whether a request's content type is gRPC.
pub(crate) fn is_grpc(headers: &HeaderMap) -> bool {
    headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.starts_with(GRPC_CONTENT_TYPE))
}

/// The trailers of a gRPC stream that failed after its headers went out.
pub(crate) fn unavailable_trailers(message: &'static str) -> HeaderMap {
    let mut trailers = HeaderMap::new();
    trailers.insert("grpc-status", HeaderValue::from_static(GRPC_UNAVAILABLE));
    trailers.insert("grpc-message", HeaderValue::from_static(message));
    trailers
}

/// A trailers-only gRPC answer: the status in the one headers frame, END_STREAM on it.
fn grpc_only(status: &'static str, message: &'static str) -> Response<ProxyBody> {
    let mut response = Response::new(ProxyBody::empty());
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(GRPC_CONTENT_TYPE),
    );
    headers.insert("grpc-status", HeaderValue::from_static(status));
    headers.insert("grpc-message", HeaderValue::from_static(message));
    response
}

/// A plain HTTP answer.
pub(crate) fn text(status: StatusCode, message: impl Into<Bytes>) -> Response<ProxyBody> {
    let mut response = Response::new(ProxyBody::bytes(message.into()));
    *response.status_mut() = status;
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static("text/plain"));
    response
}

/// The answer to a stream the proxy cannot serve right now: trailers-only `UNAVAILABLE` for
/// gRPC, 503 for everything else.
pub(crate) fn unavailable(grpc: bool, message: &'static str) -> Response<ProxyBody> {
    if grpc {
        grpc_only(GRPC_UNAVAILABLE, message)
    } else {
        let mut response = text(StatusCode::SERVICE_UNAVAILABLE, message);
        // A held HTTP/1.1 request that outlives its bound is told when to come back.
        response
            .headers_mut()
            .insert(header::RETRY_AFTER, HeaderValue::from_static("1"));
        response
    }
}

/// The answer to a call the auth rules refused: the gRPC status in a trailers-only response,
/// or 401, 403 or 429. Nothing says which credential was wrong.
pub(crate) fn refused(grpc: bool, refusal: Refusal) -> Response<ProxyBody> {
    let (status, code, message) = match refusal {
        Refusal::Unauthenticated => (
            StatusCode::UNAUTHORIZED,
            GRPC_UNAUTHENTICATED,
            "authentication required",
        ),
        Refusal::PermissionDenied => (
            StatusCode::FORBIDDEN,
            GRPC_PERMISSION_DENIED,
            "this identity may not call this",
        ),
        Refusal::RateLimited => (
            StatusCode::TOO_MANY_REQUESTS,
            GRPC_RESOURCE_EXHAUSTED,
            "too many wrong credentials from this address",
        ),
    };
    if grpc {
        return grpc_only(code, message);
    }
    let mut response = text(status, message);
    if refusal == Refusal::Unauthenticated {
        response
            .headers_mut()
            .insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
    }
    response
}

/// A malformed HTTP/1.1 request head: 400, and the connection is not reused.
pub(crate) fn bad_request(message: &'static str) -> Response<ProxyBody> {
    let mut response = text(StatusCode::BAD_REQUEST, message);
    response
        .headers_mut()
        .insert(header::CONNECTION, HeaderValue::from_static("close"));
    response
}

/// A gRPC path asked for over HTTP/1.1: 426, naming the protocol that serves it.
pub(crate) fn upgrade_required() -> Response<ProxyBody> {
    let mut response = text(
        StatusCode::UPGRADE_REQUIRED,
        "this path is served over HTTP/2",
    );
    let headers = response.headers_mut();
    headers.insert(header::UPGRADE, HeaderValue::from_static("h2"));
    headers.insert(header::CONNECTION, HeaderValue::from_static("Upgrade"));
    response
}

/// The answer to a path outside the route table: `UNIMPLEMENTED` for gRPC, 404 otherwise.
pub(crate) fn unknown(grpc: bool) -> Response<ProxyBody> {
    if grpc {
        grpc_only(GRPC_UNIMPLEMENTED, "unknown service")
    } else {
        text(StatusCode::NOT_FOUND, "not found")
    }
}

/// An update handler's reply as an HTTP answer.
pub(crate) fn from_update(reply: UpdateReply) -> Response<ProxyBody> {
    let mut response = Response::new(ProxyBody::bytes(Bytes::from(reply.body)));
    *response.status_mut() = StatusCode::from_u16(reply.status).unwrap_or(StatusCode::BAD_GATEWAY);
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    for (name, value) in reply.headers {
        headers.insert(name, value);
    }
    response
}
