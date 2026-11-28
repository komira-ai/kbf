//! The REAPI calls a front is answering, counted from each call's arrival until its
//! response body has ended or been dropped ([`ExecuteMetrics::open_calls`],
//! `kbf_front_open_calls`), and what the server's close ([`crate::Closer::close`]) does to the
//! ones it finds.
//!
//! A drain waits for this count, not only for the open Execute streams: a ByteStream write
//! or read, a `BatchUpdateBlobs`, a `GetActionResult` or an Execute still being prepared is
//! answered before the process goes, instead of being cut when it exits. A unary call counts
//! until its one message is sent; a streaming call until its last message, its status or its
//! caller leaving.
//!
//! The close is the operator's hard stop (a second stop signal, `--shutdown-timeout-secs`
//! running out). A call it finds is answered UNAVAILABLE, which a client makes again, and not
//! left to the exit, whose closed connection a client takes as a cancelled call:
//!
//! - a call whose handler has not answered is answered UNAVAILABLE at once, in headers alone;
//! - a call whose answer is being sent, or is made after the close, ends UNAVAILABLE in place
//!   of its next frame;
//! - an Execute or WaitExecution stream is left to the `Execution` service, which ends it
//!   UNAVAILABLE with its operation's name ([`crate::Closing`]).
//!
//! An answer whose last frame was taken before the close is not changed: it is the
//! transport's to write.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use futures::future::BoxFuture;
use tonic::codegen::http::header::CONTENT_TYPE;
use tonic::codegen::http::{HeaderMap, HeaderValue};
use tonic::codegen::{Service, http};
use tower_layer::Layer;

use crate::execution::Closing;
use crate::metrics::ExecuteMetrics;

/// The `grpc-message` of a call the close ends.
pub(crate) const CLOSED_MESSAGE: &str =
    "the server is shutting down; this call was not finished, make it again";

/// The path prefix of the `Execution` service's calls, whose streams end themselves at the
/// close.
const EXECUTION: &str = "/build.bazel.remote.execution.v2.Execution/";

/// The gRPC status of a call the close ends, as headers: UNAVAILABLE and [`CLOSED_MESSAGE`].
fn closed_status(headers: &mut HeaderMap) {
    headers.insert("grpc-status", HeaderValue::from_static("14"));
    headers.insert("grpc-message", HeaderValue::from_static(CLOSED_MESSAGE));
}

/// Counts every call of the service it wraps in [`ExecuteMetrics::open_calls`], and ends the
/// ones `closing`'s close finds (see the module).
#[derive(Clone, Debug)]
pub struct CallsLayer {
    metrics: Arc<ExecuteMetrics>,
    closing: Closing,
}

impl CallsLayer {
    /// Counts in `metrics`, and ends the calls open when `closing`'s closer closes.
    #[must_use]
    pub fn new(metrics: Arc<ExecuteMetrics>, closing: Closing) -> Self {
        Self { metrics, closing }
    }
}

impl<S> Layer<S> for CallsLayer {
    type Service = Counted<S>;

    fn layer(&self, inner: S) -> Self::Service {
        Counted {
            inner,
            metrics: Arc::clone(&self.metrics),
            closing: self.closing.clone(),
        }
    }
}

/// The service [`CallsLayer`] wraps around a front's routes.
#[derive(Clone, Debug)]
pub struct Counted<S> {
    inner: S,
    metrics: Arc<ExecuteMetrics>,
    closing: Closing,
}

/// One call in [`ExecuteMetrics::open_calls`] until it is dropped.
#[derive(Debug)]
struct Open(Arc<ExecuteMetrics>);

impl Open {
    fn new(metrics: &Arc<ExecuteMetrics>) -> Self {
        metrics.open_calls.add_one();
        Self(Arc::clone(metrics))
    }
}

impl Drop for Open {
    fn drop(&mut self) {
        self.0.open_calls.sub_one();
    }
}

/// Completes when `closing`'s closer closes; never, if it is dropped first.
fn closed(closing: &Closing) -> BoxFuture<'static, ()> {
    let mut closing = closing.clone();
    Box::pin(async move { closing.closed().await })
}

impl<S, B, R> Service<http::Request<B>> for Counted<S>
where
    S: Service<http::Request<B>, Response = http::Response<R>>,
    S::Future: Send + 'static,
{
    type Response = http::Response<CountedBody<R>>;
    type Error = S::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, request: http::Request<B>) -> Self::Future {
        // Counted before the inner service sees the call, so a call being authorized or
        // prepared is in the count too; a call whose handler fails lowers it with its error.
        let open = Open::new(&self.metrics);
        // An Execute or WaitExecution stream is ended by its own service at the close.
        let ends_itself = request.uri().path().starts_with(EXECUTION);
        let cut = (!ends_itself).then(|| closed(&self.closing));
        let refused = closed(&self.closing);
        let answer = self.inner.call(request);
        Box::pin(async move {
            tokio::select! {
                // A handler that has answered keeps its answer's head; its body is the
                // close's to end (`CountedBody`).
                biased;
                answered = answer => {
                    let response = answered?;
                    Ok(response.map(|inner| CountedBody::new(inner, open, cut)))
                }
                () = refused => Ok(CountedBody::closed_answer(open)),
            }
        })
    }
}

/// A response body that keeps its call in the count until it ends or is dropped, and that
/// the close ends UNAVAILABLE (see the module).
pub struct CountedBody<B> {
    /// The handler's body; `None` for the answer of a call the close found unanswered,
    /// whose status is in its headers.
    inner: Option<B>,
    /// Completes at the close, for a body the close ends; `None` for one it leaves alone.
    cut: Option<BoxFuture<'static, ()>>,
    /// The body has given its last frame (its trailers, or its end): nothing follows.
    done: bool,
    _open: Open,
}

impl<B> CountedBody<B> {
    fn new(inner: B, open: Open, cut: Option<BoxFuture<'static, ()>>) -> Self {
        Self {
            inner: Some(inner),
            cut,
            done: false,
            _open: open,
        }
    }

    /// The answer of a call the close found unanswered: UNAVAILABLE, in headers alone.
    fn closed_answer(open: Open) -> http::Response<Self> {
        let mut response = http::Response::new(Self {
            inner: None,
            cut: None,
            done: true,
            _open: open,
        });
        let headers = response.headers_mut();
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/grpc"));
        closed_status(headers);
        response
    }
}

impl<B> std::fmt::Debug for CountedBody<B> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CountedBody")
            .field("answered", &self.inner.is_some())
            .field("ends_at_the_close", &self.cut.is_some())
            .field("done", &self.done)
            .finish_non_exhaustive()
    }
}

impl<B> http_body::Body for CountedBody<B>
where
    B: http_body::Body + Unpin,
{
    type Data = B::Data;
    type Error = B::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<http_body::Frame<Self::Data>, Self::Error>>> {
        let this = &mut *self;
        // An answer the close gave in headers alone has no body and is done from the start.
        let (false, Some(inner)) = (this.done, this.inner.as_mut()) else {
            return Poll::Ready(None);
        };
        // Looked at before the inner body, so a body that always has a frame ready (a long
        // read) still ends at the close; it also wakes this body when the close comes.
        if let Some(cut) = this.cut.as_mut()
            && cut.as_mut().poll(cx).is_ready()
        {
            this.done = true;
            let mut trailers = HeaderMap::new();
            closed_status(&mut trailers);
            return Poll::Ready(Some(Ok(http_body::Frame::trailers(trailers))));
        }
        let polled = Pin::new(inner).poll_frame(cx);
        if let Poll::Ready(frame) = &polled {
            this.done = match frame {
                None => true,
                Some(Ok(frame)) => frame.is_trailers(),
                Some(Err(_)) => true,
            };
        }
        polled
    }

    fn is_end_stream(&self) -> bool {
        self.done || self.inner.as_ref().is_none_or(B::is_end_stream)
    }

    fn size_hint(&self) -> http_body::SizeHint {
        match &self.inner {
            Some(inner) if !self.done => inner.size_hint(),
            _ => http_body::SizeHint::with_exact(0),
        }
    }
}

#[cfg(test)]
#[path = "calls_tests.rs"]
mod tests;
