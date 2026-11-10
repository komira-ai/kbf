//! The health listener: what each request is answered, the loopback-only address, and the
//! listener serving over a socket.

use std::io::{Read, Write};
use std::time::Duration;

use http_body_util::BodyExt;
use hyper::header::{ALLOW, CONTENT_TYPE};
use hyper::{Method, StatusCode};

use crate::health::{HEALTH_PATH, answer, loopback, serve};

async fn body_of(response: hyper::Response<http_body_util::Full<bytes::Bytes>>) -> String {
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("a body")
        .to_bytes();
    String::from_utf8(bytes.to_vec()).expect("text")
}

/// Catches: a health route that answers anything but `GET` or `HEAD` on `/healthz` with 200, or
/// a path near it (a trailing slash, a prefix) as healthy.
#[tokio::test]
async fn healthz_is_200_ok_for_get_and_head_and_nothing_else_is() {
    for method in [Method::GET, Method::HEAD] {
        let response = answer(&method, HEALTH_PATH);
        assert_eq!(response.status(), StatusCode::OK, "{method}");
        assert_eq!(response.headers()[CONTENT_TYPE], "text/plain");
        assert!(response.headers().get(ALLOW).is_none());
        assert_eq!(body_of(response).await, "ok\n");
    }
    for method in [Method::POST, Method::PUT, Method::DELETE] {
        let response = answer(&method, HEALTH_PATH);
        assert_eq!(
            response.status(),
            StatusCode::METHOD_NOT_ALLOWED,
            "{method}"
        );
        assert_eq!(response.headers()[ALLOW], "GET, HEAD");
        assert_eq!(body_of(response).await, "method not allowed\n");
    }
    for path in ["/", "/healthz/", "/healthzz", "/readyz", "/v1/status"] {
        let response = answer(&Method::GET, path);
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "{path}");
        assert_eq!(body_of(response).await, "not found\n");
    }
}

/// Catches: a health listener allowed on an address others reach (it has no TLS and no
/// token): every loopback address is accepted, the unspecified address and any other is
/// refused by its whole message, and a text that is no address is refused naming it.
#[test]
fn only_a_loopback_address_is_accepted() {
    // Any address of 127/8, not only the usual one.
    let other_loopback = std::net::SocketAddr::from(([127, 1, 2, 3], 18846)).to_string();
    for text in ["127.0.0.1:0", other_loopback.as_str(), "[::1]:0"] {
        assert_eq!(
            loopback(text),
            Ok(text.parse().expect("an address")),
            "{text}"
        );
    }
    for text in ["0.0.0.0:18846", "192.0.2.1:1", "[::]:1", "[2001:db8::1]:1"] {
        assert_eq!(
            loopback(text),
            Err(format!(
                "{text} is not a loopback address: the health listener has no TLS and no \
                 token, so it binds a loopback address only"
            )),
        );
    }
    assert_eq!(
        loopback("localhost:1"),
        Err("\"localhost:1\": invalid socket address syntax".to_owned())
    );
}

/// The raw answer of the listener at `addr` to `request`.
fn exchange(addr: std::net::SocketAddr, request: &str) -> String {
    let mut stream = std::net::TcpStream::connect(addr).expect("connected");
    stream.write_all(request.as_bytes()).expect("sent");
    let mut answer = String::new();
    stream.read_to_string(&mut answer).expect("read");
    answer
}

/// Catches: a listener that binds but does not speak HTTP/1.1, or that serves only its first
/// connection: two requests on two connections are each answered, a healthy one with 200 and
/// `ok`, an unknown path with 404.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_listener_answers_each_connection_over_http_1_1() {
    let tcp = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bound");
    let addr = tcp.local_addr().expect("an address");
    let server = tokio::spawn(serve(tcp, Duration::from_secs(10)));
    let answers = tokio::task::spawn_blocking(move || {
        let close = "Host: health\r\nConnection: close\r\n\r\n";
        (
            exchange(addr, &format!("GET /healthz HTTP/1.1\r\n{close}")),
            exchange(addr, &format!("GET /nope HTTP/1.1\r\n{close}")),
        )
    })
    .await
    .expect("answered");
    assert!(
        answers.0.starts_with("HTTP/1.1 200 OK\r\n"),
        "{}",
        answers.0
    );
    assert!(answers.0.ends_with("\r\n\r\nok\n"), "{}", answers.0);
    assert!(
        answers.1.starts_with("HTTP/1.1 404 Not Found\r\n"),
        "{}",
        answers.1
    );
    server.abort();
}
