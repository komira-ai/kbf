//! How a REAPI call's end is counted and logged ([`ClosesLayer`]): each cause from the
//! status its response carries, or from its connection and the drain when it is dropped
//! before one. The routes are a fake service whose response body the test feeds frame by
//! frame; the connection is a [`Liveness`] the test marks; the clock is paused and moved
//! only by the test and by the fake routes' [`ROUTES_TAKE`], so a logged duration is the one
//! the test made.

use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use bytes::Bytes;
use futures::FutureExt as _;
use futures::channel::mpsc;
use http_body::Frame;
use http_body_util::{BodyExt as _, StreamBody};
use kbf_front::CallMetadata;
use kbf_tls::{ConnInfo, Liveness};
use tonic::codegen::{Service, http};
use tonic::transport::server::TcpConnectInfo;
use tower_layer::Layer as _;

use super::{Cause, CloseMetrics, ClosesLayer, MESSAGE};
use crate::drain::Drain;
use crate::health::Readiness;
use crate::proxy_auth::ClientAddr;
use crate::test_log::{Capture, capture};

const EXECUTE: &str = "/build.bazel.remote.execution.v2.Execution/Execute";
const READ: &str = "/google.bytestream.ByteStream/Read";
const GET_RESULT: &str = "/build.bazel.remote.execution.v2.ActionCache/GetActionResult";

/// The peer every test connection has.
const PEER: &str = "192.0.2.7:40312";

type Item = Result<Frame<Bytes>, io::Error>;
type Body = StreamBody<mpsc::UnboundedReceiver<Item>>;

/// What the fake routes answer a call with.
enum Answer {
    /// A response with these headers and the body the test feeds through the sender.
    Body(http::HeaderMap, Body),
    /// The service's error.
    Fail,
    /// No answer, ever: a call still being prepared.
    Never,
}

/// How long the fake routes take to answer, on the paused clock: so the logged duration of
/// every call they answer counts their time from the call's open, and a test left on the
/// real clock panics (`advance` needs a paused clock) instead of reading the wall clock.
const ROUTES_TAKE: Duration = Duration::from_millis(1);

/// Routes that answer each call with the next [`Answer`], [`ROUTES_TAKE`] after it came (an
/// error too); a call given [`Answer::Never`], or none, is never answered.
struct Routes(Option<Answer>);

impl Service<http::Request<()>> for Routes {
    type Response = http::Response<Body>;
    type Error = io::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, io::Error>> + Send>>;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, _: http::Request<()>) -> Self::Future {
        match self.0.take() {
            Some(Answer::Body(headers, body)) => {
                let mut response = http::Response::new(body);
                *response.headers_mut() = headers;
                Box::pin(async {
                    tokio::time::advance(ROUTES_TAKE).await;
                    Ok(response)
                })
            }
            Some(Answer::Fail) => Box::pin(async {
                tokio::time::advance(ROUTES_TAKE).await;
                Err(io::Error::other("routes failed"))
            }),
            Some(Answer::Never) | None => Box::pin(std::future::pending()),
        }
    }
}

/// The counts, the drain the layer reads, and the layer over them.
struct Rig {
    metrics: Arc<CloseMetrics>,
    drain: Arc<Drain>,
    layer: ClosesLayer,
}

impl Rig {
    fn new() -> Self {
        let (closer, _closing) = kbf_front::closing();
        let (drain, _goaway) = Drain::new(closer, Arc::new(Readiness::default()));
        let drain = Arc::new(drain);
        let metrics = Arc::new(CloseMetrics::default());
        let layer = ClosesLayer::new(Arc::clone(&metrics), Arc::clone(&drain));
        Self {
            metrics,
            drain,
            layer,
        }
    }

    /// Calls `request` on routes that give `answer`: the response, or the service's error.
    async fn call(
        &self,
        answer: Answer,
        request: http::Request<()>,
    ) -> Result<http::Response<super::ClosedBody<Body>>, io::Error> {
        self.layer.layer(Routes(Some(answer))).call(request).await
    }

    /// The calls of `method` counted under each cause, in [`Cause::ALL`]'s order.
    fn counts(&self, method: &str) -> [u64; 7] {
        Cause::ALL.map(|cause| self.metrics.of(method, cause))
    }
}

/// The counts with one call under `cause`.
fn one(cause: Cause) -> [u64; 7] {
    Cause::ALL.map(|c| u64::from(c == cause))
}

/// A call to `path` over a connection from [`PEER`] whose flag is `live`.
fn request(path: &str, live: &Liveness) -> http::Request<()> {
    let mut request = http::Request::builder()
        .uri(path)
        .body(())
        .expect("a request");
    request.extensions_mut().insert(ConnInfo {
        tcp: TcpConnectInfo {
            local_addr: None,
            remote_addr: Some(PEER.parse::<SocketAddr>().expect("an address")),
        },
        certs: None,
        live: live.clone(),
    });
    request
}

/// A body the test feeds, and its sender.
fn fed() -> (mpsc::UnboundedSender<Item>, Body) {
    let (send, frames) = mpsc::unbounded();
    (send, StreamBody::new(frames))
}

/// Headers holding gRPC status `code`.
fn status(code: u8) -> http::HeaderMap {
    let mut headers = http::HeaderMap::new();
    headers.insert("grpc-status", code.to_string().parse().expect("a value"));
    headers
}

/// Feeds a message and then trailers with status `code` into `send`, closing it.
fn message_then_status(send: &mpsc::UnboundedSender<Item>, code: u8) {
    let message = Frame::data(Bytes::from_static(b"an operation"));
    send.unbounded_send(Ok(message)).expect("a message");
    send.unbounded_send(Ok(Frame::trailers(status(code))))
        .expect("trailers");
    send.close_channel();
}

/// Reads `response`'s body to its end, as the transport does, and drops it.
async fn read_out(response: http::Response<super::ClosedBody<Body>>) {
    let mut body = response.into_body();
    while let Some(frame) = body.frame().await {
        if frame.is_err() {
            break;
        }
    }
}

/// The lines `log` holds that carry the close message, each without its timestamp.
fn close_lines(log: &Capture) -> Vec<String> {
    let text = log.text();
    text.lines()
        .filter(|line| line.contains(MESSAGE))
        .map(|line| {
            line.split_once("Z ")
                .map_or(line, |(_, rest)| rest.trim_start())
        })
        .map(str::to_owned)
        .collect()
}

/// The line logged for a call: the level, the message, then the fields as they are written.
fn line(level: &str, fields: &str) -> String {
    format!("{level} kbf_server::closes: {MESSAGE} {fields}")
}

/// Catches a stream that ends with its OK trailers counted as anything but `ok`, or logged:
/// a normal end is neither abnormal nor lost.
#[tokio::test(start_paused = true)]
async fn a_stream_that_ends_ok_counts_ok_and_logs_nothing() {
    let log = capture();
    let rig = Rig::new();
    let (send, body) = fed();
    let response = rig
        .call(
            Answer::Body(http::HeaderMap::new(), body),
            request(EXECUTE, &Liveness::default()),
        )
        .await
        .expect("a response");
    message_then_status(&send, 0);
    read_out(response).await;

    assert_eq!(rig.counts("Execute"), one(Cause::Ok));
    assert_eq!(close_lines(&log), Vec::<String>::new());
}

/// Catches a call answered with a status of its own request (a cache miss's NOT_FOUND,
/// sent in the headers with nothing else) counted as a server error, a drop, or logged:
/// every action-cache miss would then be a line. Also a trailers-only response the
/// transport drops without reading its empty body, which must still count its status.
#[tokio::test(start_paused = true)]
async fn a_status_the_request_called_for_counts_client_error_and_logs_nothing() {
    let log = capture();
    let rig = Rig::new();
    let (send, body) = fed();
    send.close_channel();
    let response = rig
        .call(
            Answer::Body(status(5), body),
            request(GET_RESULT, &Liveness::default()),
        )
        .await
        .expect("a response");
    drop(response);

    assert_eq!(rig.counts("GetActionResult"), one(Cause::ClientError));
    assert_eq!(close_lines(&log), Vec::<String>::new());
}

/// Catches a server's error status (INTERNAL in the trailers of a stream) not counted as
/// `server_error`, or not logged with its status, method, peer and the call's ids.
#[tokio::test(start_paused = true)]
async fn a_server_error_status_counts_and_logs_one_line() {
    let log = capture();
    let rig = Rig::new();
    let (send, body) = fed();
    let mut call = request(EXECUTE, &Liveness::default());
    let metadata = CallMetadata {
        invocation_id: kbf_front::InvocationId::parse("inv-1"),
        action_id: kbf_front::Text {
            value: "act-9".to_owned(),
            truncated: false,
        },
        ..CallMetadata::default()
    };
    call.extensions_mut().insert(Arc::new(metadata));
    call.headers_mut()
        .insert("x-request-id", "envoy-req-3".parse().expect("a value"));
    let response = rig
        .call(Answer::Body(http::HeaderMap::new(), body), call)
        .await
        .expect("a response");
    message_then_status(&send, 13);
    read_out(response).await;

    assert_eq!(rig.counts("Execute"), one(Cause::ServerError));
    assert_eq!(
        close_lines(&log),
        [line(
            "WARN",
            "method=\"Execute\" cause=\"server_error\" status=\"Internal\" peer=\"192.0.2.7:40312\" invocation=\"inv-1\" action=\"act-9\" request_id=\"envoy-req-3\" duration_ms=1"
        )]
    );
}

/// Catches a response that ends with no status (a body that ends bare, a body that fails,
/// the routes' own error) counted as anything but `server_error`: none is an answer.
#[tokio::test(start_paused = true)]
async fn an_end_with_no_status_counts_server_error() {
    let rig = Rig::new();
    let live = Liveness::default();

    let (send, body) = fed();
    send.close_channel();
    let bare = rig
        .call(
            Answer::Body(http::HeaderMap::new(), body),
            request(READ, &live),
        )
        .await
        .expect("a response");
    read_out(bare).await;
    assert_eq!(rig.counts("Read"), one(Cause::ServerError), "a bare end");

    let (send, body) = fed();
    send.unbounded_send(Err(io::Error::other("the body failed")))
        .expect("an error");
    let failed = rig
        .call(
            Answer::Body(http::HeaderMap::new(), body),
            request(READ, &live),
        )
        .await
        .expect("a response");
    read_out(failed).await;
    assert_eq!(rig.counts("Read")[2], 2, "a failed body");

    let refused = rig.call(Answer::Fail, request(READ, &live)).await;
    assert!(refused.is_err(), "the routes' error goes through");
    assert_eq!(rig.counts("Read")[2], 3, "the routes' error");
    assert_eq!(
        rig.counts("Read").iter().sum::<u64>(),
        3,
        "{:?}",
        rig.counts("Read")
    );
}

/// Catches a stream its client resets, on a connection that stays, counted as anything but
/// `client_cancel`, or logged at WARN, or with a duration other than the time it was open
/// (the paused clock moved 90 s after the routes' [`ROUTES_TAKE`]), or with the
/// connection's peer instead of the client the proxy names.
#[tokio::test(start_paused = true)]
async fn a_reset_on_a_connection_that_stays_counts_client_cancel() {
    let log = capture();
    let rig = Rig::new();
    let (send, body) = fed();
    let mut call = request(EXECUTE, &Liveness::default());
    call.extensions_mut()
        .insert(ClientAddr("198.51.100.4".parse().expect("an address")));
    let response = rig
        .call(Answer::Body(http::HeaderMap::new(), body), call)
        .await
        .expect("a response");
    let mut body = response.into_body();
    assert!(body.frame().now_or_never().is_none(), "nothing to send yet");
    let message = Frame::data(Bytes::from_static(b"queued"));
    send.unbounded_send(Ok(message)).expect("a message");
    assert!(body.frame().await.is_some(), "the first operation");
    tokio::time::advance(Duration::from_secs(90)).await;
    drop(body);

    assert_eq!(rig.counts("Execute"), one(Cause::ClientCancel));
    assert_eq!(
        close_lines(&log),
        [line(
            "INFO",
            "method=\"Execute\" cause=\"client_cancel\" status=\"-\" peer=\"198.51.100.4\" invocation=\"-\" action=\"\" request_id=\"-\" duration_ms=90001"
        )]
    );
}

/// Catches a call dropped before its routes answered (a client that resets a Write still
/// being read) not counted at all: the record is the call's, not only its body's.
#[tokio::test(start_paused = true)]
async fn a_call_dropped_before_its_answer_is_counted() {
    let rig = Rig::new();
    let live = Liveness::default();
    let mut service = rig.layer.layer(Routes(Some(Answer::Never)));
    let pending = service.call(request("/google.bytestream.ByteStream/Write", &live));
    assert!(pending.now_or_never().is_none(), "no answer yet");

    assert_eq!(rig.counts("Write"), one(Cause::ClientCancel));
}

/// Catches a stream whose connection is lost counted as the client's cancel, as a
/// keepalive close or not at all; and a call with no connection to ask (no `ConnInfo`)
/// taken for a cancel. Logged at WARN.
#[tokio::test(start_paused = true)]
async fn a_lost_connection_counts_connection_lost() {
    let log = capture();
    let rig = Rig::new();
    let live = Liveness::default();
    let (_send, body) = fed();
    let response = rig
        .call(
            Answer::Body(http::HeaderMap::new(), body),
            request(READ, &live),
        )
        .await
        .expect("a response");
    live.mark_gone();
    drop(response);
    assert_eq!(rig.counts("Read"), one(Cause::ConnectionLost));

    let bare = http::Request::builder()
        .uri(READ)
        .body(())
        .expect("a request");
    let (_send, body) = fed();
    let response = rig
        .call(Answer::Body(http::HeaderMap::new(), body), bare)
        .await
        .expect("a response");
    drop(response);
    assert_eq!(rig.counts("Read")[4], 2, "no connection to ask");
    assert_eq!(
        close_lines(&log),
        [
            line(
                "WARN",
                "method=\"Read\" cause=\"connection_lost\" status=\"-\" peer=\"192.0.2.7:40312\" invocation=\"-\" action=\"\" request_id=\"-\" duration_ms=1"
            ),
            line(
                "WARN",
                "method=\"Read\" cause=\"connection_lost\" status=\"-\" peer=\"-\" invocation=\"-\" action=\"\" request_id=\"-\" duration_ms=1"
            ),
        ]
    );
}

/// Catches a stream on a connection the keepalive watch closed counted as a lost connection
/// or a drain: the watch's close is the server's own, and it wins over a drain that closed
/// at the same time.
#[tokio::test(start_paused = true)]
async fn a_keepalive_close_counts_keepalive() {
    let log = capture();
    let rig = Rig::new();
    let live = Liveness::default();
    let (_send, body) = fed();
    let response = rig
        .call(
            Answer::Body(http::HeaderMap::new(), body),
            request(EXECUTE, &live),
        )
        .await
        .expect("a response");
    live.mark_closed_quiet();
    live.mark_gone();
    rig.drain.close();
    drop(response);

    assert_eq!(rig.counts("Execute"), one(Cause::Keepalive));
    assert_eq!(
        close_lines(&log),
        [line(
            "WARN",
            "method=\"Execute\" cause=\"keepalive\" status=\"-\" peer=\"192.0.2.7:40312\" invocation=\"-\" action=\"\" request_id=\"-\" duration_ms=1"
        )]
    );
}

/// Catches the drain's ends counted as server errors or cancels: an UNAVAILABLE while the
/// drain refuses new calls, and a stream dropped after the drain closed. An UNAVAILABLE
/// with no drain is a server error.
#[tokio::test(start_paused = true)]
async fn the_drains_ends_count_drain() {
    let rig = Rig::new();
    let live = Liveness::default();

    let (send, body) = fed();
    send.close_channel();
    let response = rig
        .call(Answer::Body(status(14), body), request(EXECUTE, &live))
        .await
        .expect("a response");
    drop(response);
    assert_eq!(rig.counts("Execute"), one(Cause::ServerError), "no drain");

    rig.drain.begin();
    let (send, body) = fed();
    send.close_channel();
    let refused = rig
        .call(Answer::Body(status(14), body), request(EXECUTE, &live))
        .await
        .expect("a response");
    drop(refused);
    assert_eq!(rig.counts("Execute")[6], 1, "refused while draining");

    let (_send, body) = fed();
    let open = rig
        .call(
            Answer::Body(http::HeaderMap::new(), body),
            request(EXECUTE, &live),
        )
        .await
        .expect("a response");
    drop(open);
    assert_eq!(
        rig.counts("Execute")[3],
        1,
        "a reset while draining, before the close, is the client's"
    );

    rig.drain.close();
    let (_send, body) = fed();
    let cut = rig
        .call(
            Answer::Body(http::HeaderMap::new(), body),
            request(EXECUTE, &live),
        )
        .await
        .expect("a response");
    drop(cut);
    assert_eq!(rig.counts("Execute"), [0, 0, 1, 1, 0, 0, 2]);
}

/// Catches a call to a path the table does not name counted under another method or not at
/// all, and a request id that is too long or not printable logged as it came.
#[tokio::test(start_paused = true)]
async fn an_unknown_path_counts_as_other_and_a_bad_request_id_is_not_logged() {
    let log = capture();
    let rig = Rig::new();
    let mut call = request("/some.Service/Method", &Liveness::default());
    call.headers_mut()
        .insert("x-request-id", "two words".parse().expect("a value"));
    let (_send, body) = fed();
    drop(
        rig.call(Answer::Body(http::HeaderMap::new(), body), call)
            .await
            .expect("a response"),
    );
    let mut long = request("/some.Service/Method", &Liveness::default());
    long.headers_mut()
        .insert("x-request-id", "r".repeat(129).parse().expect("a value"));
    let (_send, body) = fed();
    drop(
        rig.call(Answer::Body(http::HeaderMap::new(), body), long)
            .await
            .expect("a response"),
    );

    assert_eq!(rig.counts("other")[3], 2);
    assert_eq!(rig.counts("Execute"), [0; 7]);
    let cancelled = line(
        "INFO",
        "method=\"other\" cause=\"client_cancel\" status=\"-\" peer=\"192.0.2.7:40312\" invocation=\"-\" action=\"\" request_id=\"-\" duration_ms=1",
    );
    assert_eq!(close_lines(&log), [cancelled.clone(), cancelled]);
}

/// Catches a family that leaves out a series (one never counted would read as gone), a
/// count under the wrong method or cause in the text, and the help and type lines missing.
#[test]
fn every_series_is_rendered() {
    let metrics = CloseMetrics::default();
    metrics.add(super::row_of(EXECUTE), Cause::Keepalive);
    metrics.add(super::row_of(EXECUTE), Cause::Keepalive);
    metrics.add(super::row_of("/x/y"), Cause::Drain);
    let mut text = String::new();
    metrics.render(&mut text);

    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 2 + 15 * 7, "{text}");
    assert_eq!(
        lines[0],
        "# HELP kbf_reapi_calls_closed_total REAPI calls the REAPI listener ended, by method and by how each ended."
    );
    assert_eq!(lines[1], "# TYPE kbf_reapi_calls_closed_total counter");
    assert_eq!(
        lines[2],
        "kbf_reapi_calls_closed_total{method=\"Execute\",cause=\"ok\"} 0"
    );
    assert!(
        lines.contains(&"kbf_reapi_calls_closed_total{method=\"Execute\",cause=\"keepalive\"} 2")
    );
    assert!(lines.contains(&"kbf_reapi_calls_closed_total{method=\"other\",cause=\"drain\"} 1"));
    assert!(lines.contains(
        &"kbf_reapi_calls_closed_total{method=\"CancelOperation\",cause=\"connection_lost\"} 0"
    ));
    let nonzero = lines
        .iter()
        .filter(|l| !l.starts_with('#') && !l.ends_with(" 0"));
    assert_eq!(nonzero.count(), 2, "{text}");
}

/// Catches a status code read into the wrong cause: every code of gRPC, OK first, by
/// whether the request called for it or the server failed it, and UNAVAILABLE by whether
/// the drain has begun.
#[test]
fn every_status_code_has_its_cause() {
    use tonic::Code;
    let client = [
        Code::InvalidArgument,
        Code::NotFound,
        Code::AlreadyExists,
        Code::PermissionDenied,
        Code::FailedPrecondition,
        Code::OutOfRange,
        Code::Unimplemented,
        Code::Unauthenticated,
    ];
    let server = [
        Code::Cancelled,
        Code::Unknown,
        Code::DeadlineExceeded,
        Code::ResourceExhausted,
        Code::Aborted,
        Code::Internal,
        Code::Unavailable,
        Code::DataLoss,
    ];
    assert_eq!(Cause::of_status(Code::Ok, false), Cause::Ok);
    assert_eq!(Cause::of_status(Code::Ok, true), Cause::Ok);
    for code in client {
        assert_eq!(
            Cause::of_status(code, false),
            Cause::ClientError,
            "{code:?}"
        );
        assert_eq!(Cause::of_status(code, true), Cause::ClientError, "{code:?}");
    }
    for code in server {
        assert_eq!(
            Cause::of_status(code, false),
            Cause::ServerError,
            "{code:?}"
        );
    }
    assert_eq!(Cause::of_status(Code::Unavailable, true), Cause::Drain);
    assert_eq!(Cause::of_status(Code::Internal, true), Cause::ServerError);
}

/// Catches a retire's refusal (the drain refusing new Execute calls UNAVAILABLE before, or
/// without, its GOAWAY) counted as a server error: it is the drain's.
#[tokio::test(start_paused = true)]
async fn a_refusal_before_the_drain_begins_counts_drain() {
    let rig = Rig::new();
    rig.drain.refuse();
    assert!(!rig.drain.is_draining(), "refusing, not yet begun");
    let (send, body) = fed();
    send.close_channel();
    let refused = rig
        .call(
            Answer::Body(status(14), body),
            request(EXECUTE, &Liveness::default()),
        )
        .await
        .expect("a response");
    drop(refused);

    assert_eq!(rig.counts("Execute"), one(Cause::Drain));
}

/// A call to `GET_RESULT` with `grpc-timeout` set to `timeout`, whose routes never answer, dropped once
/// the paused clock has moved `after`: the counts of `GetActionResult` and the lines logged.
async fn dropped_after(timeout: &str, after: Duration) -> ([u64; 7], Vec<String>) {
    let log = capture();
    let rig = Rig::new();
    let mut call = request(GET_RESULT, &Liveness::default());
    call.headers_mut()
        .insert("grpc-timeout", timeout.parse().expect("a value"));
    let mut pending = Box::pin(rig.call(Answer::Never, call));
    assert!((&mut pending).now_or_never().is_none(), "no answer");
    tokio::time::advance(after).await;
    drop(pending);
    (rig.counts("GetActionResult"), close_lines(&log))
}

/// Catches a call the transport's timeout ended (it drops the call and answers
/// DEADLINE_EXCEEDED itself, outside the layer) counted as the client's cancel: dropped at
/// its `grpc-timeout`, it is a `server_error` with that status; a second before, a cancel.
#[tokio::test(start_paused = true)]
async fn a_call_dropped_at_its_grpc_timeout_counts_server_error() {
    let (counts, lines) = dropped_after("5S", Duration::from_secs(5)).await;
    assert_eq!(counts, one(Cause::ServerError));
    assert_eq!(
        lines,
        [line(
            "WARN",
            "method=\"GetActionResult\" cause=\"server_error\" status=\"DeadlineExceeded\" peer=\"192.0.2.7:40312\" invocation=\"-\" action=\"\" request_id=\"-\" duration_ms=5000"
        )]
    );

    let (counts, _) = dropped_after("5S", Duration::from_secs(4)).await;
    assert_eq!(counts, one(Cause::ClientCancel), "inside its timeout");
    let (counts, _) = dropped_after("5X", Duration::from_secs(9)).await;
    assert_eq!(
        counts,
        one(Cause::ClientCancel),
        "a timeout that does not parse"
    );
}

/// Catches a `grpc-timeout` read in the wrong unit, or a malformed one taken: each unit of
/// the gRPC spec, and the values the spec refuses (no digits, nine digits, no unit, a sign).
#[test]
fn a_grpc_timeout_is_read_in_its_unit() {
    use super::grpc_timeout;
    assert_eq!(grpc_timeout(b"2H"), Some(Duration::from_secs(7200)));
    assert_eq!(grpc_timeout(b"3M"), Some(Duration::from_secs(180)));
    assert_eq!(grpc_timeout(b"4S"), Some(Duration::from_secs(4)));
    assert_eq!(grpc_timeout(b"5m"), Some(Duration::from_millis(5)));
    assert_eq!(grpc_timeout(b"6u"), Some(Duration::from_micros(6)));
    assert_eq!(
        grpc_timeout(b"99999999n"),
        Some(Duration::from_nanos(99_999_999))
    );
    for refused in [&b"S"[..], b"123456789S", b"12", b"", b"-1S", b"1x"] {
        assert_eq!(
            grpc_timeout(refused),
            None,
            "{:?}",
            String::from_utf8_lossy(refused)
        );
    }
}
