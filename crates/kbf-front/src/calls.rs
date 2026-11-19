//! The REAPI calls a front is answering, counted from each call's arrival until its
//! response body has ended or been dropped ([`ExecuteMetrics::open_calls`],
//! `kbf_front_open_calls`).
//!
//! A drain waits for this count, not only for the open Execute streams: a ByteStream write
//! or read, a `BatchUpdateBlobs`, a `GetActionResult` or an Execute still being prepared is
//! answered before the process goes, instead of being cut when it exits. A unary call counts
//! until its one message is sent; a streaming call until its last message, its status or its
//! caller leaving.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use tonic::codegen::{Service, http};
use tower_layer::Layer;

use crate::metrics::ExecuteMetrics;

/// Counts every call of the service it wraps in [`ExecuteMetrics::open_calls`].
#[derive(Clone, Debug)]
pub struct CallsLayer {
    metrics: Arc<ExecuteMetrics>,
}

impl CallsLayer {
    /// Counts in `metrics`.
    #[must_use]
    pub fn new(metrics: Arc<ExecuteMetrics>) -> Self {
        Self { metrics }
    }
}

impl<S> Layer<S> for CallsLayer {
    type Service = Counted<S>;

    fn layer(&self, inner: S) -> Self::Service {
        Counted {
            inner,
            metrics: Arc::clone(&self.metrics),
        }
    }
}

/// The service [`CallsLayer`] wraps around a front's routes.
#[derive(Clone, Debug)]
pub struct Counted<S> {
    inner: S,
    metrics: Arc<ExecuteMetrics>,
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
        let answer = self.inner.call(request);
        Box::pin(async move {
            let response = answer.await?;
            Ok(response.map(|inner| CountedBody { inner, _open: open }))
        })
    }
}

/// A response body that keeps its call in the count until it ends or is dropped.
#[derive(Debug)]
pub struct CountedBody<B> {
    inner: B,
    _open: Open,
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
        Pin::new(&mut self.inner).poll_frame(cx)
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> http_body::SizeHint {
        self.inner.size_hint()
    }
}

#[cfg(test)]
#[path = "calls_tests.rs"]
mod tests;
