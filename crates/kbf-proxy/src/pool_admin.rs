//! The admin API of a pool-mode proxy: plain HTTP/1.1 on `admin_listen` (a loopback address,
//! which the config enforces), and on no other socket. It asks for no credential: whoever can
//! reach the loopback address is the host's own. A request a web page could have sent (it has
//! an `Origin` header, or a `Host` that is not a loopback IP literal) is a 403.
//!
//! - `GET /v1/proxy/upstreams`: every member as `{"name", "state", "ready", "open_streams"}`,
//!   `state` being `up` or `draining` and `open_streams` the streams open on the member by
//!   listener (`reapi`, `worker`, `operator`, `enroll`).
//! - `POST /v1/proxy/upstreams/{name}:drain` and `...:undrain`: the member takes no new stream
//!   (drain) or takes them again (undrain). The streams it has go on to their end. Both are
//!   idempotent and answer the same list as the `GET`; a name that is no member is a 404 and
//!   changes nothing.
//!
//! Draining one member is how a frontend leaves the pool for an update: wait for its
//! `open_streams` to reach 0, restart it, wait until it is ready, and undrain it.

use std::convert::Infallible;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use bytes::Bytes;
use http::{HeaderMap, HeaderValue, Method, Request, Response, StatusCode, header};
use http_body_util::Full;
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;

use crate::config::Limits;
use crate::pool::BoxIo;
use crate::serve::{ACCEPT_BACKOFF, Listen};
use crate::upstreams::Upstreams;

/// The path of the list, and of every member under it.
const PREFIX: &str = "/v1/proxy/upstreams";

/// What the admin API answers: a status and a JSON body, and for a 405 the methods it takes.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Reply {
    status: StatusCode,
    allow: Option<&'static str>,
    body: String,
}

impl Reply {
    fn json(status: StatusCode, body: String) -> Self {
        Self {
            status,
            allow: None,
            body,
        }
    }

    fn error(status: StatusCode, message: &str) -> Self {
        Self::json(status, serde_json::json!({ "error": message }).to_string())
    }

    fn not_found(message: &str) -> Self {
        Self::error(StatusCode::NOT_FOUND, message)
    }

    fn not_allowed(allow: &'static str) -> Self {
        Self {
            allow: Some(allow),
            ..Self::error(StatusCode::METHOD_NOT_ALLOWED, "method not allowed")
        }
    }

    fn into_response(self) -> Response<Full<Bytes>> {
        let mut response = Response::new(Full::new(Bytes::from(self.body)));
        *response.status_mut() = self.status;
        let headers = response.headers_mut();
        let json = HeaderValue::from_static("application/json");
        headers.insert(header::CONTENT_TYPE, json);
        if let Some(allow) = self.allow {
            headers.insert(header::ALLOW, HeaderValue::from_static(allow));
        }
        response
    }
}

/// Every member, as the list the API answers with.
fn list(members: &Upstreams) -> Reply {
    let body = serde_json::to_string(&members.snapshot()).unwrap_or_default();
    Reply::json(StatusCode::OK, body)
}

/// The answer to `method` on `path`.
pub(crate) fn answer(members: &Upstreams, method: &Method, path: &str) -> Reply {
    let Some(rest) = path.strip_prefix(PREFIX) else {
        return Reply::not_found("no such path");
    };
    if rest.is_empty() {
        return match *method {
            Method::GET => list(members),
            _ => Reply::not_allowed("GET"),
        };
    }
    let call = rest.strip_prefix('/').and_then(|t| t.rsplit_once(':'));
    let (name, draining) = match call {
        Some((name, "drain")) => (name, true),
        Some((name, "undrain")) => (name, false),
        _ => return Reply::not_found("no such path"),
    };
    if *method != Method::POST {
        return Reply::not_allowed("POST");
    }
    if !members.set_draining(name, draining) {
        return Reply::not_found("no such upstream");
    }
    list(members)
}

/// Serves the API on connections from `listener`, until the task is dropped.
pub(crate) async fn serve<L: Listen>(mut listener: L, members: Arc<Upstreams>, limits: Limits) {
    let http = crate::h1::builder(&limits);
    loop {
        match listener.accept().await {
            Ok((io, _)) => {
                tokio::spawn(connection(http.clone(), io, Arc::clone(&members)));
            }
            // Out of descriptors, say: back off instead of spinning on the error.
            Err(_) => tokio::time::sleep(ACCEPT_BACKOFF).await,
        }
    }
}

/// The 403 for a request a web page could have sent to the loopback socket: one with an
/// `Origin` header (a browser sets it on every cross-site `POST`), or whose `Host` is not an IP
/// literal on loopback (a DNS name that was made to point here). A client that was given
/// `admin_listen` sends neither.
fn browser_refusal(headers: &HeaderMap) -> Option<Reply> {
    let host = headers.get(header::HOST).and_then(|h| h.to_str().ok());
    let from_a_page = headers.contains_key(header::ORIGIN);
    if from_a_page | !host.is_some_and(is_loopback_host) {
        return Some(Reply::error(StatusCode::FORBIDDEN, "not for a web page"));
    }
    None
}

/// Whether a `Host` value is a loopback IP literal, with a port or without one.
fn is_loopback_host(host: &str) -> bool {
    let literal = host.parse::<SocketAddr>().map(|socket| socket.ip());
    let ip = literal.or_else(|_| host.parse::<IpAddr>());
    ip.is_ok_and(|ip| ip.is_loopback())
}

/// The reply to a request: the 403 of [`browser_refusal`], or the [`answer`].
fn respond(members: &Upstreams, method: &Method, path: &str, headers: &HeaderMap) -> Reply {
    match browser_refusal(headers) {
        Some(refusal) => refusal,
        None => answer(members, method, path),
    }
}

/// One connection, to its end.
async fn connection(http: http1::Builder, io: BoxIo, members: Arc<Upstreams>) {
    let service = service_fn(move |request: Request<Incoming>| {
        let reply = respond(
            &members,
            request.method(),
            request.uri().path(),
            request.headers(),
        );
        async move { Ok::<_, Infallible>(reply.into_response()) }
    });
    let _ = http.serve_connection(TokioIo::new(io), service).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Frontend;

    fn pool() -> Arc<Upstreams> {
        let frontend = |name: &str, base_port| Frontend {
            name: name.to_owned(),
            base_port,
        };
        Upstreams::new(&[frontend("a", 18_000), frontend("b", 18_010)])
    }

    /// The members of `reply`'s list as (name, state).
    fn states(reply: &Reply) -> Vec<(String, String)> {
        let list: serde_json::Value = serde_json::from_str(&reply.body).expect("a JSON list");
        let member = |m: &serde_json::Value| (m["name"].to_string(), m["state"].to_string());
        let items = list.as_array().expect("an array");
        items.iter().map(member).collect()
    }

    fn pair(name: &str, state: &str) -> (String, String) {
        (format!("\"{name}\""), format!("\"{state}\""))
    }

    /// Asserts that the list answers `GET` with every member's name, state, readiness and open
    /// streams by listener, as JSON.
    #[test]
    fn the_list_shows_each_member() {
        let members = pool();
        members.set_ready(1, true);
        let reply = answer(&members, &Method::GET, PREFIX);
        assert_eq!(reply.status, StatusCode::OK);
        let list: serde_json::Value = serde_json::from_str(&reply.body).expect("JSON");
        assert_eq!(list[0]["name"], "a");
        assert_eq!(list[0]["ready"], false);
        assert_eq!(list[1]["ready"], true);
        assert_eq!(list[1]["state"], "up");
        assert_eq!(list[1]["open_streams"]["operator"], 0);
    }

    /// Asserts that `:drain` makes the member draining and answers the list, that doing it
    /// again changes nothing, and that `:undrain` puts it back, also twice over.
    #[test]
    fn drain_and_undrain_are_idempotent_and_answer_the_list() {
        let members = pool();
        let drain = format!("{PREFIX}/a:drain");
        let undrain = format!("{PREFIX}/a:undrain");
        for _ in 0..2 {
            let reply = answer(&members, &Method::POST, &drain);
            assert_eq!(reply.status, StatusCode::OK);
            let want = [pair("a", "draining"), pair("b", "up")];
            assert_eq!(states(&reply), want);
        }
        for _ in 0..2 {
            let reply = answer(&members, &Method::POST, &undrain);
            assert_eq!(reply.status, StatusCode::OK);
            let want = [pair("a", "up"), pair("b", "up")];
            assert_eq!(states(&reply), want);
        }
    }

    /// Asserts that a name that is no member is a 404 for both verbs and drains nothing.
    #[test]
    fn an_unknown_member_is_a_404_and_changes_nothing() {
        let members = pool();
        for verb in ["drain", "undrain"] {
            let reply = answer(&members, &Method::POST, &format!("{PREFIX}/c:{verb}"));
            assert_eq!(reply.status, StatusCode::NOT_FOUND);
            assert!(reply.body.contains("no such upstream"), "{}", reply.body);
        }
        let list = answer(&members, &Method::GET, PREFIX);
        assert_eq!(states(&list), [pair("a", "up"), pair("b", "up")]);
    }

    /// Asserts that a path that is not the list or a member's `:drain` or `:undrain` is a 404,
    /// look-alikes included.
    #[test]
    fn other_paths_are_a_404() {
        let members = pool();
        for path in [
            "/",
            "/v1/proxy",
            "/v1/proxy/upstreamsx",
            "/v1/proxy/upstreams/",
            "/v1/proxy/upstreams/a",
            "/v1/proxy/upstreams/a:explode",
            "/v1/proxy/upstreams/a:drain/extra",
            "/v1/proxy/upstreams:drain",
        ] {
            let reply = answer(&members, &Method::POST, path);
            assert_eq!(reply.status, StatusCode::NOT_FOUND, "{path}");
        }
        assert_eq!(states(&answer(&members, &Method::GET, PREFIX)).len(), 2);
    }

    /// Asserts that the wrong method on a known path is a 405 that names the method it takes,
    /// and that it drains nothing.
    #[test]
    fn the_wrong_method_is_a_405_naming_the_right_one() {
        let members = pool();
        let list = answer(&members, &Method::POST, PREFIX);
        assert_eq!(
            (list.status, list.allow),
            (StatusCode::METHOD_NOT_ALLOWED, Some("GET"))
        );
        let drain = format!("{PREFIX}/a:drain");
        for method in [Method::GET, Method::PUT, Method::DELETE] {
            let reply = answer(&members, &method, &drain);
            assert_eq!(reply.status, StatusCode::METHOD_NOT_ALLOWED, "{method}");
            assert_eq!(reply.allow, Some("POST"));
        }
        let all_up = [pair("a", "up"), pair("b", "up")];
        assert_eq!(states(&answer(&members, &Method::GET, PREFIX)), all_up);
    }

    /// The headers of a request with `host` and, if given, `origin`.
    fn headers(host: Option<&'static str>, origin: Option<&'static str>) -> HeaderMap {
        let mut headers = HeaderMap::new();
        if let Some(host) = host {
            headers.insert(header::HOST, HeaderValue::from_static(host));
        }
        if let Some(origin) = origin {
            headers.insert(header::ORIGIN, HeaderValue::from_static(origin));
        }
        headers
    }

    /// Asserts that a request whose `Host` is a loopback IP literal, with a port or without,
    /// in either family, and that has no `Origin` is answered, and drains.
    #[test]
    fn a_request_to_a_loopback_literal_is_answered() {
        let members = pool();
        let drain = format!("{PREFIX}/a:drain");
        for host in ["127.0.0.1:18090", "127.0.0.1", "[::1]:18090"] {
            let sent = headers(Some(host), None);
            let reply = respond(&members, &Method::POST, &drain, &sent);
            assert_eq!(reply.status, StatusCode::OK, "{host}");
        }
    }

    /// Asserts that a request with an `Origin` header is a 403 whatever its `Host`, and that
    /// it drains nothing.
    #[test]
    fn a_request_with_an_origin_is_a_403_and_changes_nothing() {
        let members = pool();
        let drain = format!("{PREFIX}/a:drain");
        let sent = headers(Some("127.0.0.1:18090"), Some("http://example.test"));
        let reply = respond(&members, &Method::POST, &drain, &sent);
        assert_eq!(reply.status, StatusCode::FORBIDDEN);
        let list = answer(&members, &Method::GET, PREFIX);
        assert_eq!(states(&list), [pair("a", "up"), pair("b", "up")]);
    }

    /// Asserts that a request whose `Host` is a name, a non-loopback address, or not given is
    /// a 403 (a DNS name pointed at the loopback address reaches it this way), and that it
    /// drains nothing.
    #[test]
    fn a_request_to_a_name_or_no_host_is_a_403_and_changes_nothing() {
        let members = pool();
        let drain = format!("{PREFIX}/a:drain");
        for host in ["localhost:18090", "rebind.test", "192.0.2.1"] {
            let sent = headers(Some(host), None);
            let reply = respond(&members, &Method::POST, &drain, &sent);
            assert_eq!(reply.status, StatusCode::FORBIDDEN, "{host}");
        }
        let reply = respond(&members, &Method::POST, &drain, &headers(None, None));
        assert_eq!(reply.status, StatusCode::FORBIDDEN, "no Host");
        let list = answer(&members, &Method::GET, PREFIX);
        assert_eq!(states(&list), [pair("a", "up"), pair("b", "up")]);
    }

    /// Asserts that a reply becomes a JSON response with its status, and an `Allow` header
    /// only when it has methods to name.
    #[test]
    fn a_reply_becomes_a_json_response() {
        let ok = Reply::json(StatusCode::OK, "[]".to_owned()).into_response();
        assert_eq!(ok.status(), StatusCode::OK);
        assert_eq!(ok.headers()[header::CONTENT_TYPE], "application/json");
        assert!(ok.headers().get(header::ALLOW).is_none());
        let no = Reply::not_allowed("GET").into_response();
        assert_eq!(no.status(), StatusCode::METHOD_NOT_ALLOWED);
        assert_eq!(no.headers()[header::ALLOW], "GET");
    }
}
