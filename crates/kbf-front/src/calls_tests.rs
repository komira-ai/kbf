//! The count of open calls: from a call's arrival until its body has ended or been dropped,
//! whether the call is answered, fails or is abandoned.

use std::future::{Ready, ready};

use http_body::Body as _;

use super::*;

/// A service that answers every call with `body`, or fails it.
#[derive(Clone)]
struct Answer(Result<&'static str, &'static str>);

impl Service<http::Request<()>> for Answer {
    type Response = http::Response<String>;
    type Error = &'static str;
    type Future = Ready<Result<Self::Response, Self::Error>>;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, _: http::Request<()>) -> Self::Future {
        ready(self.0.map(|body| http::Response::new(body.to_owned())))
    }
}

/// A service whose answer never comes, as a call still being prepared.
#[derive(Clone)]
struct Never;

impl Service<http::Request<()>> for Never {
    type Response = http::Response<String>;
    type Error = &'static str;
    type Future = std::future::Pending<Result<Self::Response, Self::Error>>;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Err("not ready"))
    }

    fn call(&mut self, _: http::Request<()>) -> Self::Future {
        std::future::pending()
    }
}

fn counted<S>(inner: S) -> (Counted<S>, Arc<ExecuteMetrics>) {
    let (service, metrics, _closer) = closable(inner);
    (service, metrics)
}

/// [`counted`], and the closer whose close ends the calls the service has open.
fn closable<S>(inner: S) -> (Counted<S>, Arc<ExecuteMetrics>, crate::Closer) {
    let metrics = Arc::new(ExecuteMetrics::default());
    let (closer, closing) = crate::closing();
    let service = CallsLayer::new(Arc::clone(&metrics), closing).layer(inner);
    (service, metrics, closer)
}

/// Catches a count that ends when the handler returns rather than when the answer has
/// been sent: the call counts from its arrival, through its handler, while its body is
/// read, and stops counting only when the body is dropped. The body's frames, end and size
/// are the inner body's.
#[tokio::test]
async fn a_call_counts_from_its_arrival_until_its_body_is_dropped() {
    let (mut service, metrics) = counted(Answer(Ok("the blob")));
    assert_eq!(metrics.open_calls.get(), 0);

    let answer = service.call(http::Request::new(()));
    assert_eq!(metrics.open_calls.get(), 1, "a call not yet answered");
    let response = answer.await.expect("answered");
    assert_eq!(
        metrics.open_calls.get(),
        1,
        "an answer whose body is unsent"
    );

    let mut body = response.into_body();
    assert!(!body.is_end_stream());
    assert_eq!(body.size_hint().exact(), Some(8));
    let frame = std::future::poll_fn(|cx| Pin::new(&mut body).poll_frame(cx))
        .await
        .expect("a frame")
        .expect("data");
    assert_eq!(frame.into_data().expect("data"), "the blob");
    assert!(body.is_end_stream());
    assert_eq!(
        metrics.open_calls.get(),
        1,
        "a body sent and not yet let go"
    );
    drop(body);
    assert_eq!(metrics.open_calls.get(), 0);
}

/// Catches a call that fails in its handler and is never let out of the count, which would
/// keep a drain waiting forever.
#[tokio::test]
async fn a_failed_call_leaves_the_count() {
    let (mut service, metrics) = counted(Answer(Err("refused")));
    let answer = service.call(http::Request::new(()));
    assert_eq!(metrics.open_calls.get(), 1);
    assert_eq!(answer.await.expect_err("failed"), "refused");
    assert_eq!(metrics.open_calls.get(), 0);
}

/// Catches a call abandoned before its answer (its caller gone, its connection cut) that
/// stays in the count. Readiness is the inner service's.
#[test]
fn an_abandoned_call_leaves_the_count() {
    let (mut service, metrics) = counted(Never);
    let waker = futures::task::noop_waker();
    let mut cx = Context::from_waker(&waker);
    assert_eq!(service.poll_ready(&mut cx), Poll::Ready(Err("not ready")));
    let answer = service.call(http::Request::new(()));
    let second = service.clone().call(http::Request::new(()));
    assert_eq!(metrics.open_calls.get(), 2);
    drop(answer);
    assert_eq!(metrics.open_calls.get(), 1);
    drop(second);
    assert_eq!(metrics.open_calls.get(), 0);
}

/// One step of a [`Script`]ed body.
#[derive(Clone, Copy, Debug)]
enum Step {
    /// A data frame, then the next step.
    Data(&'static str),
    /// A data frame, and this step again: a body that always has a frame ready.
    Again(&'static str),
    /// Trailers with an OK status, then the next step.
    Trailers,
    /// The end of the body.
    End,
    /// Nothing yet, for ever: a frame that has not been made.
    Pending,
    /// The body fails.
    Fail,
}

/// A response body that gives the frames its steps say, in order.
#[derive(Clone, Debug)]
struct Script(Vec<Step>);

impl http_body::Body for Script {
    type Data = bytes::Bytes;
    type Error = &'static str;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        _: &mut Context<'_>,
    ) -> Poll<Option<Result<http_body::Frame<Self::Data>, Self::Error>>> {
        let step = self.0.first().copied().unwrap_or(Step::End);
        if matches!(step, Step::Data(_) | Step::Trailers | Step::Fail) {
            self.0.remove(0);
        }
        match step {
            Step::Data(data) | Step::Again(data) => Poll::Ready(Some(Ok(http_body::Frame::data(
                bytes::Bytes::from_static(data.as_bytes()),
            )))),
            Step::Trailers => {
                let mut trailers = HeaderMap::new();
                trailers.insert("grpc-status", HeaderValue::from_static("0"));
                Poll::Ready(Some(Ok(http_body::Frame::trailers(trailers))))
            }
            Step::End => Poll::Ready(None),
            Step::Pending => Poll::Pending,
            Step::Fail => Poll::Ready(Some(Err("the body failed"))),
        }
    }
}

/// A service that answers every call with `body` once `answers` says so: a handler still at
/// work until then.
#[derive(Clone)]
struct Scripted {
    body: Script,
    answers: bool,
}

impl Service<http::Request<()>> for Scripted {
    type Response = http::Response<Script>;
    type Error = &'static str;
    type Future = futures::future::BoxFuture<'static, Result<Self::Response, Self::Error>>;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, _: http::Request<()>) -> Self::Future {
        let (body, answers) = (self.body.clone(), self.answers);
        Box::pin(async move {
            if !answers {
                std::future::pending::<()>().await;
            }
            Ok(http::Response::new(body))
        })
    }
}

/// A call of the method at `path`.
fn call_of(path: &str) -> http::Request<()> {
    let mut request = http::Request::new(());
    *request.uri_mut() = path.parse().expect("a path");
    request
}

/// The ByteStream read the tests make, a call the close ends.
const READ: &str = "/google.bytestream.ByteStream/Read";

/// What one poll of a counted body gives.
type Polled = Poll<Option<Result<http_body::Frame<bytes::Bytes>, BoxError>>>;

/// Polls `body` once, with a waker that does nothing.
fn poll_once(body: &mut CountedBody) -> Polled {
    let waker = futures::task::noop_waker();
    Pin::new(body).poll_frame(&mut Context::from_waker(&waker))
}

/// Asserts that `headers` say UNAVAILABLE, with the close's message.
fn says_unavailable(headers: &HeaderMap, what: &str) {
    assert_eq!(
        headers.get("grpc-status").map(HeaderValue::as_bytes),
        Some(&b"14"[..]),
        "{what}"
    );
    assert_eq!(
        headers.get("grpc-message").map(HeaderValue::as_bytes),
        Some(CLOSED_MESSAGE.as_bytes()),
        "{what}"
    );
}

/// Catches a close that leaves a call whose handler is still at work to the exit (its client
/// sees its connection close, a cancelled call it does not make again): the call is answered
/// at the close, UNAVAILABLE in headers alone (a gRPC content type, an empty body that has
/// ended), and stays in the count until that answer is let go. A call that arrives after the
/// close is answered the same way.
#[tokio::test]
async fn a_call_unanswered_at_the_close_is_answered_unavailable() {
    let (mut service, metrics, closer) = closable(Scripted {
        body: Script(vec![Step::End]),
        answers: false,
    });
    let mut answer = service.call(call_of(READ));
    assert!(
        futures::FutureExt::now_or_never(&mut answer).is_none(),
        "answered before the close"
    );
    closer.close();
    let response = answer.await.expect("answered");
    assert_eq!(
        response
            .headers()
            .get(CONTENT_TYPE)
            .map(HeaderValue::as_bytes),
        Some(&b"application/grpc"[..])
    );
    says_unavailable(response.headers(), "the answer at the close");
    let mut body = response.into_body();
    assert!(http_body::Body::is_end_stream(&body));
    assert_eq!(http_body::Body::size_hint(&body).exact(), Some(0));
    assert!(matches!(poll_once(&mut body), Poll::Ready(None)));
    assert_eq!(
        metrics.open_calls.get(),
        1,
        "let go before the answer is sent"
    );
    drop(body);
    assert_eq!(metrics.open_calls.get(), 0);

    let late = service.call(call_of(READ)).await.expect("answered");
    says_unavailable(late.headers(), "a call after the close");
}

/// Catches a call made after the close that is sent as if nothing had happened: its handler's
/// answer, ready at once, keeps its own head, and its body ends UNAVAILABLE at its first frame,
/// as every unfinished call does at the close.
#[tokio::test]
async fn an_answer_made_after_the_close_ends_unavailable_at_its_first_frame() {
    let (mut service, _metrics, closer) = closable(Scripted {
        body: Script(vec![Step::Data("the blob"), Step::Trailers]),
        answers: true,
    });
    closer.close();
    let response = service.call(call_of(READ)).await.expect("answered");
    assert!(response.headers().get("grpc-status").is_none());
    let mut body = response.into_body();
    let Poll::Ready(Some(Ok(frame))) = poll_once(&mut body) else {
        panic!("no frame");
    };
    says_unavailable(&frame.into_trailers().expect("trailers"), "after the close");
    assert!(matches!(poll_once(&mut body), Poll::Ready(None)));
}

/// Catches a close that leaves a body being sent to run on until the exit cuts it: a read
/// that has sent a frame and is making the next ends at the close with UNAVAILABLE trailers
/// and nothing after them; and one that always has a frame ready ends at the close too, in
/// place of its next frame.
#[tokio::test]
async fn an_answer_being_sent_at_the_close_ends_unavailable() {
    for (what, steps) in [
        (
            "a body making its next frame",
            vec![Step::Data("first"), Step::Pending],
        ),
        ("a body that always has a frame", vec![Step::Again("more")]),
    ] {
        let (mut service, metrics, closer) = closable(Scripted {
            body: Script(steps),
            answers: true,
        });
        let mut body = service
            .call(call_of(READ))
            .await
            .expect("answered")
            .into_body();
        let Poll::Ready(Some(Ok(first))) = poll_once(&mut body) else {
            panic!("{what}: no first frame");
        };
        assert!(first.is_data(), "{what}");
        assert!(!http_body::Body::is_end_stream(&body), "{what}");
        closer.close();
        let Poll::Ready(Some(Ok(last))) = poll_once(&mut body) else {
            panic!("{what}: no frame at the close");
        };
        let trailers = last.into_trailers().expect("trailers at the close");
        says_unavailable(&trailers, what);
        assert!(matches!(poll_once(&mut body), Poll::Ready(None)), "{what}");
        assert!(http_body::Body::is_end_stream(&body), "{what}");
        assert_eq!(http_body::Body::size_hint(&body).exact(), Some(0), "{what}");
        assert_eq!(metrics.open_calls.get(), 1, "{what}");
        drop(body);
        assert_eq!(metrics.open_calls.get(), 0, "{what}");
    }
}

/// Catches a close that gives a second status after one the body gave: trailers already
/// taken are the call's last frame, and the close adds nothing after them.
#[tokio::test]
async fn trailers_given_before_the_close_are_the_last_frame() {
    let (mut service, _metrics, closer) = closable(Scripted {
        body: Script(vec![Step::Data("the blob"), Step::Trailers, Step::Pending]),
        answers: true,
    });
    let mut body = service
        .call(call_of(READ))
        .await
        .expect("answered")
        .into_body();
    assert!(matches!(poll_once(&mut body), Poll::Ready(Some(Ok(f))) if f.is_data()));
    let Poll::Ready(Some(Ok(trailers))) = poll_once(&mut body) else {
        panic!("no trailers");
    };
    let trailers = trailers.into_trailers().expect("trailers");
    assert_eq!(
        trailers.get("grpc-status").map(HeaderValue::as_bytes),
        Some(&b"0"[..])
    );
    closer.close();
    assert!(matches!(poll_once(&mut body), Poll::Ready(None)));
    assert!(http_body::Body::is_end_stream(&body));
}

/// Catches a close that ends an Execute or WaitExecution stream itself, in place of the
/// `Execution` service that ends it with its operation's name: the body of an Execute is left
/// as it is at the close (it is still making its next frame). Its handler, still at work at
/// the close, is answered UNAVAILABLE like any other call's.
#[tokio::test]
async fn an_execute_stream_is_left_to_its_service_at_the_close() {
    let execute = "/build.bazel.remote.execution.v2.Execution/Execute";
    let (mut service, _metrics, closer) = closable(Scripted {
        body: Script(vec![Step::Data("queued"), Step::Pending]),
        answers: true,
    });
    let mut body = service
        .call(call_of(execute))
        .await
        .expect("answered")
        .into_body();
    assert!(matches!(poll_once(&mut body), Poll::Ready(Some(Ok(f))) if f.is_data()));
    closer.close();
    assert!(
        poll_once(&mut body).is_pending(),
        "the close ended an Execute stream"
    );
    assert!(!http_body::Body::is_end_stream(&body));
    assert_eq!(
        format!("{body:?}"),
        "CountedBody { answered: true, ends_at_the_close: false, done: false, .. }"
    );

    let (mut preparing, _metrics, closer) = closable(Scripted {
        body: Script(vec![Step::End]),
        answers: false,
    });
    let answer = preparing.call(call_of(execute));
    closer.close();
    says_unavailable(
        answer.await.expect("answered").headers(),
        "an Execute being prepared",
    );
}

/// Catches a close that gives a status after the body failed (the transport has reset the
/// stream by then): a failure is the call's last frame, and nothing follows it.
#[tokio::test]
async fn a_failed_body_is_given_nothing_after_its_failure() {
    let (mut service, _metrics, closer) = closable(Scripted {
        body: Script(vec![Step::Fail, Step::Pending]),
        answers: true,
    });
    let mut body = service
        .call(call_of(READ))
        .await
        .expect("answered")
        .into_body();
    assert!(matches!(
        poll_once(&mut body),
        Poll::Ready(Some(Err(e))) if e.to_string() == "the body failed"
    ));
    closer.close();
    assert!(matches!(poll_once(&mut body), Poll::Ready(None)));
    assert!(http_body::Body::is_end_stream(&body));
}
