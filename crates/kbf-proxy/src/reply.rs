//! The answers the proxy gives itself: `UNAVAILABLE`, 404, the probes, the update API's reply.

use bytes::Bytes;
use http::{HeaderMap, HeaderValue, Response, StatusCode, header};

use crate::body::ProxyBody;
use crate::control::UpdateReply;

/// `grpc-status` of `UNAVAILABLE`.
pub(crate) const GRPC_UNAVAILABLE: &str = "14";
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
        text(StatusCode::SERVICE_UNAVAILABLE, message)
    }
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
