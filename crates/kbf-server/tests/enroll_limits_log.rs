//! The enroll guard's log line for a rate-limited call. Alone in its test binary, on a
//! current-thread runtime: a tracing callsite that another test's thread reached first, with
//! no subscriber installed, is cached as uninteresting, and a subscriber installed after
//! (`set_default` is per thread) would miss it. In the library's unit tests this failed one
//! run in ten.

use std::convert::Infallible;
use std::future::{Ready, ready};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use kbf_server::enroll::limits::{GuardLayer, Limits};
use tonic::codegen::{Service, http};
use tower_layer::Layer as _;

/// An inner service that always answers.
#[derive(Clone)]
struct Inner;

impl Service<http::Request<()>> for Inner {
    type Response = http::Response<String>;
    type Error = Infallible;
    type Future = Ready<Result<Self::Response, Infallible>>;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Infallible>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, _: http::Request<()>) -> Self::Future {
        ready(Ok(http::Response::builder()
            .header("grpc-status", "0")
            .body("answer".to_owned())
            .expect("response")))
    }
}

/// Log lines written by the test's subscriber.
#[derive(Clone, Default)]
struct Lines(Arc<Mutex<Vec<u8>>>);

impl Lines {
    fn text(&self) -> String {
        String::from_utf8(self.0.lock().expect("lock").clone()).expect("UTF-8")
    }
}

impl std::io::Write for Lines {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().expect("lock").extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// A refused call leaves one line saying why, so an operator who sees nodes failing to
/// enrol can tell a flood from a fault.
///
/// Catches: a refusal that is silent, or logged under another reason; an admitted call that
/// logs.
#[tokio::test(flavor = "current_thread")]
async fn a_rate_limited_call_is_logged_with_its_reason() {
    let lines = Lines::default();
    let writer = lines.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(move || writer.clone())
        .with_ansi(false)
        .finish();
    let _installed = tracing::subscriber::set_default(subscriber);
    let mut guard = GuardLayer::new(&Limits {
        per_minute: 1,
        burst: 1,
        in_flight: 8,
    })
    .layer(Inner);
    guard.call(http::Request::new(())).await.expect("answer");
    assert_eq!(lines.text(), "", "an admitted call logs nothing");
    guard.call(http::Request::new(())).await.expect("answer");
    let text = lines.text();
    assert!(
        text.contains("enrollment call refused") && text.contains("reason=\"rate_limited\""),
        "{text}"
    );
}
