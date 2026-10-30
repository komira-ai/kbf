//! The fake's HTTP: the accept loop, the router, and the faults.

use std::convert::Infallible;
use std::sync::Arc;

use bytes::Bytes;
use http_body_util::{BodyExt as _, Full};
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper::{Method, Request, Response};
use hyper_util::rt::TokioIo;
use serde::de::DeserializeOwned;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpListener;

use super::state::{Answer, Refusal, State};
use super::{Fault, Shared, TOKEN, locked};
use crate::ids::LeaseId;
use crate::wire::{RenewRequest, RunSpec};

pub(super) async fn accept_loop(
    listener: TcpListener,
    acceptor: Option<tokio_rustls::TlsAcceptor>,
    shared: Arc<Shared>,
) {
    loop {
        let (stream, _) = listener
            .accept()
            .await
            .expect("the fake lease server accepts on its loopback port");
        let shared = Arc::clone(&shared);
        match acceptor.clone() {
            Some(acceptor) => {
                tokio::spawn(async move {
                    // A client that fails the handshake gets nothing; it is a test of the
                    // client's trust, not of the server's answers.
                    if let Ok(tls) = acceptor.accept(stream).await {
                        connection(tls, shared).await;
                    }
                });
            }
            None => {
                tokio::spawn(connection(stream, shared));
            }
        }
    }
}

async fn connection<S>(stream: S, shared: Arc<Shared>)
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let service = service_fn(move |request| handle(Arc::clone(&shared), request));
    // A connection that ends early is the client's doing or a fault; there is no one to tell.
    let _ = hyper::server::conn::http1::Builder::new()
        .serve_connection(TokioIo::new(stream), service)
        .await;
}

type Reply = Response<Full<Bytes>>;

fn reply(status: u16, body: impl Into<Bytes>) -> Reply {
    let mut response = Response::new(Full::new(body.into()));
    *response.status_mut() = hyper::StatusCode::from_u16(status).unwrap_or_default();
    response
}

/// An error that is the connection's, not a status: hyper closes the connection on it.
#[derive(Debug, thiserror::Error)]
#[error("fault injected")]
struct Injected;

async fn handle(
    shared: Arc<Shared>,
    request: Request<Incoming>,
) -> Result<Reply, Box<dyn std::error::Error + Send + Sync>> {
    let (head, body) = request.into_parts();
    let body = body
        .collect()
        .await
        .map(http_body_util::Collected::to_bytes);
    let line = format!("{} {}", head.method, head.uri.path());
    locked(&shared.requests).push(line.clone());
    let fault = {
        let mut faults = locked(&shared.faults);
        let at = faults.iter().position(|(route, _)| line.starts_with(route));
        at.and_then(|at| faults.remove(at)).map(|(_, fault)| fault)
    };
    match fault {
        Some(Fault::Close) => return Err(Box::new(Injected)),
        Some(Fault::Hang) => match std::future::pending::<Infallible>().await {},
        Some(Fault::Reply { status, body }) => return Ok(reply(status, body)),
        Some(Fault::ShortBody) => {
            let mut short = reply(200, "{}");
            short
                .headers_mut()
                .insert(hyper::header::CONTENT_LENGTH, 100.into());
            return Ok(short);
        }
        None => {}
    }
    let authorized = head
        .headers
        .get(hyper::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .is_some_and(|token| token == TOKEN);
    if !authorized {
        return Ok(reply(401, Bytes::new()));
    }
    let body = body.unwrap_or_default();
    let answer = {
        let mut state = locked(&shared.state);
        route(&mut state, &head.method, head.uri.path(), &body)
    };
    Ok(match answer {
        Ok((status, body)) => reply(status, body),
        Err(refusal) => reply(refusal.status, refusal.body()),
    })
}

fn parse<T: DeserializeOwned>(body: &[u8]) -> Result<T, Refusal> {
    serde_json::from_slice(body).map_err(|error| Refusal::bad_body(&error))
}

fn lease(text: &str) -> Result<LeaseId, Refusal> {
    text.parse().map_err(|_| Refusal::unknown_lease())
}

/// The route table: the real server's, by method and path.
fn route(state: &mut State, method: &Method, path: &str, body: &[u8]) -> Answer {
    let segments: Vec<&str> = path.trim_start_matches('/').split('/').collect();
    match (method.as_str(), segments.as_slice()) {
        ("POST", ["v1", "leases"]) => state.acquire(parse(body)?),
        ("GET", ["v1", "leases"]) => state.leases(),
        ("GET", ["v1", "hosts"]) => state.hosts(),
        ("POST", ["v1", "leases", id, "renew"]) => {
            // The id is read before the body, as the server's extractors do.
            let id = lease(id)?;
            let request = match body.is_empty() {
                true => RenewRequest::default(),
                false => parse(body)?,
            };
            state.renew(id, &request)
        }
        ("DELETE", ["v1", "leases", id]) => state.release(lease(id)?),
        ("POST", ["v1", "leases", id, "runs"]) => {
            let id = lease(id)?;
            let spec: RunSpec = parse(body)?;
            state.queue_run(id, spec)
        }
        ("GET", ["v1", "leases", id, "runs"]) => state.runs(lease(id)?),
        ("GET", ["v1", "leases", id, "runs", run]) => state.run(lease(id)?, run),
        ("DELETE", ["v1", "leases", id, "runs", run]) => state.stop_run(lease(id)?, run),
        (
            _,
            ["v1", "leases"]
            | ["v1", "hosts"]
            | ["v1", "leases", _]
            | ["v1", "leases", _, "renew" | "runs"]
            | ["v1", "leases", _, "runs", _],
        ) => Err(Refusal::method_not_allowed()),
        _ => Err(Refusal::not_found()),
    }
}
