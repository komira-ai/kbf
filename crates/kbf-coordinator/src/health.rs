//! The health listener (`--health-listen`): plain HTTP on a loopback address, for a proxy or a
//! supervisor on the same host to check that the coordinator serves.
//!
//! `GET /healthz` (or `HEAD`) answers 200 `ok` for as long as the process serves its heartbeat
//! and control listeners: the health listener is bound with them and served beside them, and
//! nothing answers it before they are bound. It says nothing about the farm, the desired state
//! or the quiet start (`GET /v1/convergence` on the control listener says those). Any other
//! method on `/healthz` is 405, any other path 404. It has no TLS and no token, which is why its
//! address must be a loopback one ([`loopback`]).

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use http_body_util::Full;
use hyper::header::{ALLOW, CONTENT_TYPE, HeaderValue};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Response, StatusCode};
use hyper_util::rt::{TokioIo, TokioTimer};
use tokio::net::TcpListener;
use tokio::sync::Semaphore;

/// The one path the health listener serves.
pub const HEALTH_PATH: &str = "/healthz";

/// The most connections the health listener serves at once.
pub const MAX_HEALTH_CONNECTIONS: usize = 64;

/// The address of `--health-listen`: a socket address whose IP is a loopback one
/// (IPv4 `127.*`, or `::1`).
///
/// # Errors
/// The text is no socket address, or its IP is not a loopback one.
pub fn loopback(text: &str) -> Result<SocketAddr, String> {
    let addr: SocketAddr = text.parse().map_err(|why| format!("{text:?}: {why}"))?;
    if addr.ip().is_loopback() {
        Ok(addr)
    } else {
        Err(format!(
            "{addr} is not a loopback address: the health listener has no TLS and no token, so \
             it binds a loopback address only"
        ))
    }
}

/// The answer to `method` on `path`.
#[must_use]
pub fn answer(method: &Method, path: &str) -> Response<Full<Bytes>> {
    let (status, body) = if path != HEALTH_PATH {
        (StatusCode::NOT_FOUND, "not found\n")
    } else if method == Method::GET || method == Method::HEAD {
        (StatusCode::OK, "ok\n")
    } else {
        (StatusCode::METHOD_NOT_ALLOWED, "method not allowed\n")
    };
    let mut response = Response::new(Full::new(Bytes::from_static(body.as_bytes())));
    *response.status_mut() = status;
    let headers = response.headers_mut();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("text/plain"));
    if status == StatusCode::METHOD_NOT_ALLOWED {
        headers.insert(ALLOW, HeaderValue::from_static("GET, HEAD"));
    }
    response
}

/// Serves `tcp` for as long as the future is polled: at most [`MAX_HEALTH_CONNECTIONS`] at once,
/// each with `patience` for its request head. It never returns.
pub async fn serve(tcp: TcpListener, patience: Duration) -> Infallible {
    let slots = Arc::new(Semaphore::new(MAX_HEALTH_CONNECTIONS));
    loop {
        // A slot first, then the accept: with none free, clients queue in the backlog.
        let slot = Arc::clone(&slots).acquire_owned().await.ok();
        let Ok((stream, _)) = tcp.accept().await else {
            tokio::time::sleep(Duration::from_millis(100)).await;
            continue;
        };
        tokio::spawn(async move {
            let service = service_fn(
                |request: hyper::Request<hyper::body::Incoming>| async move {
                    Ok::<_, Infallible>(answer(request.method(), request.uri().path()))
                },
            );
            let _ = http1::Builder::new()
                .timer(TokioTimer::new())
                .header_read_timeout(patience)
                .serve_connection(TokioIo::new(stream), service)
                .await;
            drop(slot);
        });
    }
}
