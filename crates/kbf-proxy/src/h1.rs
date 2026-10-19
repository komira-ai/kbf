//! HTTP/1.1 on the public port: for `/v1/*`, the update API and the probes, and nothing else
//! (design note decision C4).
//!
//! HTTP/1.1 cannot multiplex, so a gRPC service (REAPI, worker sessions, enrollment) is not
//! served on it: such a path is `426 Upgrade Required`, naming `h2`. What is served is parsed
//! by hyper's HTTP/1.1 server (which reads the head with `httparse`) under the limits below,
//! and then checked here before the request reaches the one place every request is
//! classified:
//!
//! - **Head limits.** At most [`MAX_HEAD_BYTES`] of request head and [`MAX_HEADERS`] headers;
//!   a longer head is refused by the parser (431) and the connection closes. A client that
//!   sends its head too slowly is closed after the pre-stream timeout.
//! - **Origin-form only.** `CONNECT`, an absolute-form target (`GET http://host/x`) and any
//!   `Upgrade` header are `400`: the proxy is not a forward proxy and does not switch
//!   protocols.
//! - **Looks like HTTP/2.** The target becomes absolute (`http://<host>/path`, from `Host`)
//!   and the connection-level headers an h2 hop may not carry are dropped, so the request
//!   goes through the same route, auth, hold and upstream code as one that came over h2.
//!
//! A held request stops hyper reading the next one on its connection (one request is in
//! flight at a time), and at the hold's bound it is answered `503` with `Retry-After`.

use http::header::{self, HeaderName};
use http::uri::{Authority, Parts, Scheme};
use http::{HeaderMap, Method, Request, Response, Uri};
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper_util::rt::TokioTimer;

use crate::body::ProxyBody;
use crate::config::Limits;
use crate::reply::{bad_request, upgrade_required};
use crate::route::{Route, route_of};

/// The most bytes of request head (request line and headers) read.
pub(crate) const MAX_HEAD_BYTES: usize = 16 * 1024;
const _: () = assert!(MAX_HEAD_BYTES >= 8192, "hyper's floor for `max_buf_size`");
/// The most headers in a request.
pub(crate) const MAX_HEADERS: usize = 100;
/// The authority of a request that names no usable `Host`.
const NO_HOST: &str = "frontend";

/// Headers an HTTP/2 hop does not carry (RFC 9113, section 8.2.2).
const CONNECTION_LEVEL: [HeaderName; 5] = [
    header::CONNECTION,
    HeaderName::from_static("keep-alive"),
    HeaderName::from_static("proxy-connection"),
    header::TRANSFER_ENCODING,
    header::TE,
];

/// The HTTP/1.1 server's settings.
pub(crate) fn builder(limits: &Limits) -> http1::Builder {
    let mut http = http1::Builder::new();
    http.max_buf_size(MAX_HEAD_BYTES)
        .max_headers(MAX_HEADERS)
        .timer(TokioTimer::new())
        .header_read_timeout(limits.pre_stream_timeout());
    http
}

/// Checks a request that came over HTTP/1.1 and makes it look like one that came over h2.
///
/// # Errors
/// The answer to send instead: `400` for a request form the proxy does not take, `426` for a
/// gRPC path.
pub(crate) fn admit(
    mut request: Request<Incoming>,
) -> Result<Request<Incoming>, Box<Response<ProxyBody>>> {
    let uri = request.uri();
    if request.method() == Method::CONNECT || uri.scheme().is_some() {
        return Err(Box::new(bad_request(
            "only origin-form requests are served",
        )));
    }
    if request.headers().contains_key(header::UPGRADE) {
        return Err(Box::new(bad_request("protocol upgrades are not served")));
    }
    let route = route_of(uri.path());
    if route.grpc() {
        return Err(Box::new(upgrade_required()));
    }
    if route != Route::Unknown {
        // A request of a known route has a path, so the target is always made.
        let made = absolute(request.uri(), authority_of(request.headers()));
        made.into_iter().for_each(|uri| *request.uri_mut() = uri);
    }
    drop_connection_level(request.headers_mut());
    Ok(request)
}

/// The `Host` of a request, or [`NO_HOST`] if it names none or one that is not an authority.
fn authority_of(headers: &HeaderMap) -> Authority {
    headers
        .get(header::HOST)
        .and_then(|host| Authority::try_from(host.as_bytes()).ok())
        .unwrap_or_else(|| Authority::from_static(NO_HOST))
}

/// `uri` (an origin-form target) made absolute: `http://authority/path?query`. `None` for a
/// target with no path.
fn absolute(uri: &Uri, authority: Authority) -> Option<Uri> {
    let mut parts = Parts::default();
    parts.scheme = Some(Scheme::HTTP);
    parts.authority = Some(authority);
    parts.path_and_query = uri.path_and_query().cloned();
    Uri::from_parts(parts).ok()
}

fn drop_connection_level(headers: &mut HeaderMap) {
    for name in CONNECTION_LEVEL.iter().chain([&header::HOST]) {
        headers.remove(name);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_host_header_is_the_authority_or_a_fixed_name() {
        let mut headers = HeaderMap::new();
        assert_eq!(authority_of(&headers).as_str(), NO_HOST);
        headers.insert(header::HOST, "frontend.test:8443".parse().expect("value"));
        assert_eq!(authority_of(&headers).as_str(), "frontend.test:8443");
        headers.insert(header::HOST, "not an authority".parse().expect("value"));
        assert_eq!(authority_of(&headers).as_str(), NO_HOST);
    }

    #[test]
    fn a_target_becomes_absolute_and_a_target_without_a_path_does_not() {
        let host = Authority::from_static("frontend.test");
        let made = absolute(&Uri::from_static("/v1/status?x=1"), host.clone());
        assert_eq!(
            made.expect("absolute").to_string(),
            "http://frontend.test/v1/status?x=1"
        );
        assert!(absolute(&Uri::from_static("other.test:443"), host).is_none());
    }

    #[test]
    fn connection_level_headers_do_not_cross_to_an_h2_hop() {
        let mut headers = HeaderMap::new();
        for name in [
            "connection",
            "keep-alive",
            "proxy-connection",
            "transfer-encoding",
            "te",
            "host",
        ] {
            headers.insert(HeaderName::from_static(name), "x".parse().expect("value"));
        }
        headers.insert("x-keep", "1".parse().expect("value"));
        drop_connection_level(&mut headers);
        assert_eq!(headers.len(), 1);
        assert!(headers.contains_key("x-keep"));
    }

    #[test]
    fn the_builder_takes_the_documented_limits() {
        // `max_buf_size` panics below its floor of 8192; the head limit is above it.
        let _ = builder(&Limits::default());
    }
}
