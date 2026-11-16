//! The fake's connections: each accepted one (over TLS when the fake has it) is served by
//! hyper's HTTP/2 server, through a gate in front of the generated service that checks each
//! call's signature as tierstore's front does and applies the [`Fault`] queued for the call.

use std::convert::Infallible;
use std::future::Future;
use std::pin::{Pin, pin};
use std::sync::Arc;
use std::task::{Context, Poll};

use bytes::Bytes;
use http::HeaderMap;
use http_body_util::{BodyExt as _, Full};
use hyper::body::Incoming;
use hyper::server::conn::http2;
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::service::TowerToHyperService;
use prost::Message as _;
use tierstore_trees_api::v1::trees_service_server::TreesServiceServer;
use tierstore_trees_api::v1::{CreateTreeRequest, DeleteTreeRequest, GetPlacementRequest};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpListener;
use tokio::sync::Notify;
use tokio::task::JoinSet;
use tokio_rustls::TlsAcceptor;
use tonic::Status;
use tonic::body::Body;

use super::service::Service;
use super::state::refused;
use super::{Call, Fault, KEY_ID, REGION, SECRET, Shared};
use crate::error::Reason;
use crate::sign::{SERVICE, amz_date, canonical_request, payload_hash, signature};

/// How far a call's time may be from the fake's, as tierstore allows: 15 minutes.
const SKEW: std::time::Duration = std::time::Duration::from_secs(15 * 60);

/// A connection, over TLS or not.
trait Conn: AsyncRead + AsyncWrite + Send + Unpin {}

impl<T: AsyncRead + AsyncWrite + Send + Unpin> Conn for T {}

/// Accepts connections, each served on a task of its own, until the fake drops, which
/// aborts this task and with it every connection it serves, or until accepting fails, which
/// ends the fake's serving: the test then sees its calls fail to connect.
pub(super) async fn accept_loop(
    listener: TcpListener,
    acceptor: Option<TlsAcceptor>,
    shared: Arc<Shared>,
) -> std::io::Result<()> {
    let mut connections = JoinSet::new();
    loop {
        let (tcp, _) = listener.accept().await?;
        // As the client does: without it the small frame that ends an answer waits for the
        // client's delayed acknowledgement.
        let _ = tcp.set_nodelay(true);
        let acceptor = acceptor.clone();
        let shared = Arc::clone(&shared);
        connections.spawn(async move {
            let io: Box<dyn Conn> = match acceptor {
                // A client that does not trust the certificate ends the handshake.
                Some(acceptor) => match acceptor.accept(tcp).await {
                    Ok(tls) => Box::new(tls),
                    Err(_) => return,
                },
                None => Box::new(tcp),
            };
            serve(io, shared).await;
        });
    }
}

/// Serves one connection until it closes, or until a [`Fault::LoseAnswer`] drops it.
async fn serve(io: Box<dyn Conn>, shared: Arc<Shared>) {
    let lose = Arc::new(Notify::new());
    let gate = Gate {
        inner: TreesServiceServer::new(Service {
            shared: Arc::clone(&shared),
        }),
        shared,
        lose: Arc::clone(&lose),
    };
    let connection = http2::Builder::new(TokioExecutor::new())
        .serve_connection(TokioIo::new(io), TowerToHyperService::new(gate));
    let mut connection = pin!(connection);
    let mut lost = pin!(lose.notified());
    // Whichever ends first; dropping the connection closes its socket.
    std::future::poll_fn(|cx| {
        if lost.as_mut().poll(cx).is_ready() {
            return Poll::Ready(());
        }
        connection.as_mut().poll(cx).map(|_| ())
    })
    .await;
}

/// What stands in front of the generated service on one connection.
#[derive(Clone)]
struct Gate {
    inner: TreesServiceServer<Service>,
    shared: Arc<Shared>,
    /// Told when a [`Fault::LoseAnswer`] wants the connection dropped.
    lose: Arc<Notify>,
}

impl tonic::codegen::Service<http::Request<Incoming>> for Gate {
    type Response = http::Response<Body>;
    type Error = Infallible;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Infallible>> + Send>>;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Infallible>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, request: http::Request<Incoming>) -> Self::Future {
        let mut inner = self.inner.clone();
        let shared = Arc::clone(&self.shared);
        let lose = Arc::clone(&self.lose);
        Box::pin(async move {
            let (parts, body) = request.into_parts();
            shared.record_head(parts.headers.clone());
            // A body that cannot be read is empty here, and its hash is then not the signed one.
            let body = body.collect().await.map(|whole| whole.to_bytes());
            let body = body.unwrap_or_default();
            if let Err(why) = verify(&parts, &body, shared.now()) {
                return Ok(refused(Reason::Unauthenticated, Some(why)).into_http());
            }
            let method = parts.uri.path().rsplit('/').next().unwrap_or_default();
            let method = method.to_owned();
            let call = decode_call(&method, body.get(5..).unwrap_or_default());
            let request = http::Request::from_parts(parts, Full::new(body));
            let mut applied = |request| tonic::codegen::Service::call(&mut inner, request);
            match shared.fault_for(&method) {
                None => applied(request).await,
                Some(Fault::Hang) => {
                    call.into_iter().for_each(|call| shared.record(call));
                    shared.hung.notify_one();
                    std::future::pending().await
                }
                Some(Fault::Status { code, message }) => {
                    call.into_iter().for_each(|call| shared.record(call));
                    Ok(Status::new(code, message).into_http())
                }
                Some(Fault::LoseAnswer) => {
                    let _answer = applied(request).await;
                    lose.notify_one();
                    std::future::pending().await
                }
                Some(Fault::Answer(message)) => {
                    let _answer = applied(request).await;
                    Ok(answer_of(&message))
                }
            }
        })
    }
}

/// An answer of status OK whose one message is `message`, as it is.
fn answer_of(message: &[u8]) -> http::Response<Body> {
    let mut trailers = HeaderMap::new();
    trailers.insert("grpc-status", http::HeaderValue::from_static("0"));
    let body = Full::new(frame(message)).with_trailers(async { Some(Ok(trailers)) });
    let mut response = http::Response::new(Body::new(body));
    let grpc = http::HeaderValue::from_static("application/grpc");
    response
        .headers_mut()
        .insert(http::header::CONTENT_TYPE, grpc);
    response
}

/// The call `method` with `message` as its request, decoded; `None` for a method the
/// service does not have or a message that is not its request.
fn decode_call(method: &str, message: &[u8]) -> Option<Call> {
    let call = match method {
        "CreateTree" => Call::CreateTree(Box::new(CreateTreeRequest::decode(message).ok()?)),
        "GetPlacement" => Call::GetPlacement(GetPlacementRequest::decode(message).ok()?),
        "DeleteTree" => Call::DeleteTree(DeleteTreeRequest::decode(message).ok()?),
        _ => return None,
    };
    Some(call)
}

/// `message` as gRPC frames one message: a flag byte (not compressed), the length in four
/// bytes, the bytes.
fn frame(message: &[u8]) -> Bytes {
    let len = u32::try_from(message.len()).unwrap_or(u32::MAX);
    let mut framed = Vec::with_capacity(5 + message.len());
    framed.push(0);
    framed.extend_from_slice(&len.to_be_bytes());
    framed.extend_from_slice(message);
    Bytes::from(framed)
}

/// The value of header `name`, if it is text.
fn header<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|value| value.to_str().ok())
}

/// The parts of an `Authorization` header: the key id, the date and region of its scope,
/// its service, the signed headers and the signature.
struct Authorization<'a> {
    id: &'a str,
    date: &'a str,
    region: &'a str,
    service: &'a str,
    signed: Vec<&'a str>,
    signature: &'a str,
}

impl<'a> Authorization<'a> {
    /// `AWS4-HMAC-SHA256 Credential=<id>/<date>/<region>/<service>/aws4_request,
    /// SignedHeaders=<a;b>, Signature=<hex>`.
    fn parse(text: &'a str) -> Option<Self> {
        let rest = text.strip_prefix("AWS4-HMAC-SHA256 ")?;
        let mut fields = rest.split(", ");
        let credential = fields.next()?.strip_prefix("Credential=")?;
        let signed = fields.next()?.strip_prefix("SignedHeaders=")?;
        let signature = fields.next()?.strip_prefix("Signature=")?;
        let mut scope = credential.split('/');
        let (id, date, region, service) =
            (scope.next()?, scope.next()?, scope.next()?, scope.next()?);
        if scope.next() != Some("aws4_request") || fields.next().is_some() {
            return None;
        }
        Some(Self {
            id,
            date,
            region,
            service,
            signed: signed.split(';').collect(),
            signature,
        })
    }
}

/// Whether the call's head is signed with the fake's key, in its region, at a time within
/// [`SKEW`] of `now`, over the body it carries; why not, in the fake's words
/// (the node words its reasons otherwise).
fn verify(
    parts: &http::request::Parts,
    body: &[u8],
    now: std::time::SystemTime,
) -> Result<(), &'static str> {
    let text = header(&parts.headers, "authorization").ok_or("no Authorization header")?;
    let auth = Authorization::parse(text).ok_or("a malformed Authorization header")?;
    if auth.id != KEY_ID || auth.region != REGION || auth.service != SERVICE {
        return Err("an unknown key");
    }
    let required = ["host", "x-amz-content-sha256", "x-amz-date"];
    if !required.iter().all(|name| auth.signed.contains(name)) {
        return Err("the signature does not cover host, x-amz-content-sha256 and x-amz-date");
    }
    let stamp = header(&parts.headers, "x-amz-date").ok_or("no x-amz-date")?;
    // The stamp is fixed width, so text order is time order.
    let (early, late) = (amz_date(now - SKEW), amz_date(now + SKEW));
    if stamp.len() != early.len() || stamp < early.as_str() || stamp > late.as_str() {
        return Err("the time is too far from the node's");
    }
    if stamp.get(..8) != Some(auth.date) {
        return Err("the scope's date is not the time's");
    }
    let payload =
        header(&parts.headers, "x-amz-content-sha256").ok_or("no x-amz-content-sha256")?;
    let authority = parts.uri.authority().map(|a| a.as_str());
    let host = header(&parts.headers, "host")
        .or(authority)
        .unwrap_or_default();
    let mut headers = Vec::new();
    for name in &auth.signed {
        let value = match *name {
            "host" => host,
            other => header(&parts.headers, other).ok_or("a signed header is missing")?,
        };
        headers.push((*name, value));
    }
    let canonical = canonical_request(parts.method.as_str(), parts.uri.path(), &headers, payload);
    if signature(SECRET, (REGION, SERVICE), stamp, &canonical) != auth.signature {
        return Err("the signature does not match");
    }
    if payload != payload_hash(body) {
        return Err("the body is not the one the signature covers");
    }
    Ok(())
}

#[cfg(test)]
#[path = "http_tests.rs"]
mod tests;
