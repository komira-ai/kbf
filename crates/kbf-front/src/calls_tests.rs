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
    let metrics = Arc::new(ExecuteMetrics::default());
    (CallsLayer::new(Arc::clone(&metrics)).layer(inner), metrics)
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
