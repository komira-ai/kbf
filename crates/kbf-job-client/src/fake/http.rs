//! The fake's connections: each accepted one (over TLS when the fake has it) is served by
//! hyper's HTTP/2 server, through a gate in front of the generated service that checks the
//! bearer token and applies the [`Fault`] queued for the call.

use std::collections::VecDeque;
use std::convert::Infallible;
use std::future::Future;
use std::pin::{Pin, pin};
use std::sync::Arc;
use std::task::{Context, Poll};

use bytes::{BufMut as _, Bytes, BytesMut};
use http::header::{AUTHORIZATION, CONTENT_TYPE};
use http::{HeaderMap, HeaderValue};
use http_body::Frame;
use http_body_util::BodyExt as _;
use hyper::body::Incoming;
use hyper::server::conn::http2;
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::service::TowerToHyperService;
use js_proto::v1::job_service_server::JobServiceServer;
use js_proto::v1::{
    CancelJobRequest, GetJobRequest, GetLogsRequest, GetResultRequest, ListJobsRequest,
    SubmitJobRequest,
};
use prost::Message as _;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpListener;
use tokio::sync::Notify;
use tokio::task::JoinSet;
use tokio_rustls::TlsAcceptor;
use tonic::body::Body;
use tonic::{Code, Status};

use super::service::Service;
use super::{Call, Fault, Shared, TOKEN};

/// A connection, over TLS or not.
trait Conn: AsyncRead + AsyncWrite + Send + Unpin {}

impl<T: AsyncRead + AsyncWrite + Send + Unpin> Conn for T {}

/// Accepts connections until the fake drops, which aborts this task and with it every
/// connection it serves, or until accepting fails.
pub(super) async fn accept_loop(
    listener: TcpListener,
    acceptor: Option<TlsAcceptor>,
    shared: Arc<Shared>,
) {
    let mut connections = JoinSet::new();
    // A failed accept ends the fake's serving; there is no one to tell but the test, which
    // then sees its calls refused.
    let _ = accept_all(&listener, acceptor, &shared, &mut connections).await;
}

/// Accepts and serves connections, each on a task of `connections`, until accepting fails.
/// A connection that ended stays in the set until the fake drops; a fake lives for one
/// test.
async fn accept_all(
    listener: &TcpListener,
    acceptor: Option<TlsAcceptor>,
    shared: &Arc<Shared>,
    connections: &mut JoinSet<()>,
) -> std::io::Result<()> {
    loop {
        let (tcp, _) = listener.accept().await?;
        // As the client does (`dial.rs`): without it the small frame that ends an answer
        // (its trailers) waits for the client's delayed acknowledgement, tens of milliseconds
        // of real time, which a test on tokio's paused clock does not wait for.
        let _ = tcp.set_nodelay(true);
        let acceptor = acceptor.clone();
        let shared = Arc::clone(shared);
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
        inner: JobServiceServer::new(Service {
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
    inner: JobServiceServer<Service>,
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
            let bearer = format!("Bearer {TOKEN}");
            let presented = request.headers().get(AUTHORIZATION);
            if presented.is_none_or(|value| value.as_bytes() != bearer.as_bytes()) {
                let refused = Status::unauthenticated("the bearer token is missing or wrong");
                return Ok(refused.into_http());
            }
            let path = request.uri().path();
            let method = path.rsplit('/').next().unwrap_or_default().to_owned();
            let mut applied = |request: http::Request<Incoming>| {
                tonic::codegen::Service::call(&mut inner, request)
            };
            match shared.fault_for(&method) {
                None => applied(request).await,
                Some(Fault::Hang) => {
                    record_unapplied(&shared, &method, request).await;
                    shared.hung.notify_one();
                    std::future::pending().await
                }
                Some(Fault::Status { code, message }) => {
                    record_unapplied(&shared, &method, request).await;
                    Ok(Status::new(code, message).into_http())
                }
                Some(Fault::LoseAnswer) => {
                    let _answer = applied(request).await;
                    lose.notify_one();
                    std::future::pending().await
                }
                Some(Fault::Answer(message)) => {
                    let _answer = applied(request).await;
                    Ok(raw(frame(&message), Code::Ok))
                }
                Some(Fault::Raw { body, code }) => {
                    let _answer = applied(request).await;
                    Ok(raw(body, code))
                }
            }
        })
    }
}

/// Reads the request of a call a fault answers without applying it, and logs it as the
/// service would have; a body that is not one message of `method` is not logged.
async fn record_unapplied(shared: &Shared, method: &str, request: http::Request<Incoming>) {
    let body = request.into_body().collect().await;
    let body = body.map(|whole| whole.to_bytes()).unwrap_or_default();
    let message = body.get(5..).unwrap_or_default();
    let call = decode_call(method, message);
    call.into_iter().for_each(|call| shared.record(call));
}

/// The call `method` with `message` as its request, decoded.
fn decode_call(method: &str, message: &[u8]) -> Option<Call> {
    let call = match method {
        "SubmitJob" => Call::SubmitJob(Box::new(SubmitJobRequest::decode(message).ok()?)),
        "GetJob" => Call::GetJob(GetJobRequest::decode(message).ok()?),
        "ListJobs" => Call::ListJobs(ListJobsRequest::decode(message).ok()?),
        "CancelJob" => Call::CancelJob(CancelJobRequest::decode(message).ok()?),
        "GetResult" => Call::GetResult(GetResultRequest::decode(message).ok()?),
        "GetLogs" => Call::GetLogs(GetLogsRequest::decode(message).ok()?),
        _ => return None,
    };
    Some(call)
}

/// `message` as gRPC frames one message: a flag byte (not compressed), the length in four
/// bytes, the bytes.
fn frame(message: &[u8]) -> Bytes {
    let mut frame = BytesMut::with_capacity(5 + message.len());
    frame.put_u8(0);
    frame.put_u32(message.len() as u32);
    frame.put_slice(message);
    frame.freeze()
}

/// An answer whose body is `body` as it is, then the trailers with status `code`.
fn raw(body: Bytes, code: Code) -> http::Response<Body> {
    let mut trailers = HeaderMap::new();
    trailers.insert("grpc-status", HeaderValue::from(i32::from(code)));
    let frames = Frames(VecDeque::from([
        Frame::data(body),
        Frame::trailers(trailers),
    ]));
    let mut response = http::Response::new(Body::new(frames));
    let grpc = HeaderValue::from_static("application/grpc");
    response.headers_mut().insert(CONTENT_TYPE, grpc);
    response
}

/// A body made of the frames given, in order.
struct Frames(VecDeque<Frame<Bytes>>);

impl http_body::Body for Frames {
    type Data = Bytes;
    type Error = Infallible;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        _: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Infallible>>> {
        Poll::Ready(self.0.pop_front().map(Ok))
    }
}
