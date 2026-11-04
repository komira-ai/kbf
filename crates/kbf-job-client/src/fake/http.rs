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
use hyper::body::Incoming;
use hyper::server::conn::http2;
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::service::TowerToHyperService;
use js_proto::v1::job_service_server::JobServiceServer;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpListener;
use tokio::sync::Notify;
use tokio::task::JoinSet;
use tokio_rustls::TlsAcceptor;
use tonic::Status;
use tonic::body::Body;

use super::service::Service;
use super::{Fault, Shared, TOKEN};

/// A connection, over TLS or not.
trait Conn: AsyncRead + AsyncWrite + Send + Unpin {}

impl<T: AsyncRead + AsyncWrite + Send + Unpin> Conn for T {}

/// Accepts connections until the fake drops, which aborts this task and with it every
/// connection in `connections`, or until accepting fails. A connection that ended stays in
/// the set until then; a fake lives for one test.
pub(super) async fn accept_loop(
    listener: TcpListener,
    acceptor: Option<TlsAcceptor>,
    shared: Arc<Shared>,
) {
    let mut connections = JoinSet::new();
    while let Ok((tcp, _)) = listener.accept().await {
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
            match shared.fault_for(&method) {
                None => tonic::codegen::Service::call(&mut inner, request).await,
                Some(Fault::Hang) => std::future::pending().await,
                Some(Fault::Status { code, message }) => Ok(Status::new(code, message).into_http()),
                Some(Fault::LoseAnswer) => {
                    let _applied = tonic::codegen::Service::call(&mut inner, request).await;
                    lose.notify_one();
                    std::future::pending().await
                }
                Some(Fault::Answer(message)) => {
                    let _applied = tonic::codegen::Service::call(&mut inner, request).await;
                    Ok(answer(&message))
                }
            }
        })
    }
}

/// An OK answer whose one message is `message`, as gRPC frames it: a flag byte (not
/// compressed), the length in four bytes, the bytes; then the trailers with status 0.
fn answer(message: &[u8]) -> http::Response<Body> {
    let mut frame = BytesMut::with_capacity(5 + message.len());
    frame.put_u8(0);
    frame.put_u32(message.len() as u32);
    frame.put_slice(message);
    let mut trailers = HeaderMap::new();
    trailers.insert("grpc-status", HeaderValue::from_static("0"));
    let frames = Frames(VecDeque::from([
        Frame::data(frame.freeze()),
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
