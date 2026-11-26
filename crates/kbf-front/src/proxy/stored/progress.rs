//! When a byte of a call's request body last arrived, for a call that tonic decodes.
//!
//! tonic hands a handler whole messages only: a message of a ByteStream `Write` resolves when
//! its last byte is in, however long the bytes before it took to come. An idle timeout
//! counted from message to message would cut an upload whose one message is slow but still
//! moving. [`Watched`] wraps a service so that every request body it is handed records the
//! time each frame that carries a byte arrives ([`Progress`], in the request's extensions),
//! and the handler counts its idle timeout from that.

use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{Context, Poll};

use bytes::Bytes;
use http_body::{Body as _, Frame};
use tokio::time::Instant;
use tonic::Status;
use tonic::body::Body;
use tonic::codegen::{Service, http};
use tonic::server::NamedService;

/// When a frame of a call's request body that carried a byte last arrived, or when the call
/// began if none has. The body records it ([`Watching`]); the handler reads it.
#[derive(Clone, Debug)]
pub(super) struct Progress(Arc<Mutex<Instant>>);

impl Progress {
    /// Progress that starts now.
    pub(super) fn from_now() -> Self {
        Self(Arc::new(Mutex::new(Instant::now())))
    }

    /// When a byte last arrived (or the call began).
    pub(super) fn last(&self) -> Instant {
        *self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// A byte arrived now.
    fn moved(&self) {
        *self.0.lock().unwrap_or_else(PoisonError::into_inner) = Instant::now();
    }
}

/// The service `S` with the body of every request it is handed watched: the request carries
/// its [`Progress`] in its extensions.
#[derive(Clone)]
pub(super) struct Watched<S>(pub S);

impl<S: NamedService> NamedService for Watched<S> {
    const NAME: &'static str = S::NAME;
}

impl<S> Service<http::Request<Body>> for Watched<S>
where
    S: Service<http::Request<Body>>,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = S::Future;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.0.poll_ready(cx)
    }

    fn call(&mut self, request: http::Request<Body>) -> Self::Future {
        let (mut parts, body) = request.into_parts();
        let progress = Progress::from_now();
        parts.extensions.insert(progress.clone());
        let body = Body::new(Watching { body, progress });
        self.0.call(http::Request::from_parts(parts, body))
    }
}

/// A request body that records its [`Progress`]: a data frame with at least one byte moves
/// it; an empty data frame, trailers, the end and an error do not.
struct Watching {
    body: Body,
    progress: Progress,
}

impl http_body::Body for Watching {
    type Data = Bytes;
    type Error = Status;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Status>>> {
        let polled = Pin::new(&mut self.body).poll_frame(cx);
        if let Poll::Ready(Some(Ok(frame))) = &polled
            && frame.data_ref().is_some_and(|data| !data.is_empty())
        {
            self.progress.moved();
        }
        polled
    }

    fn is_end_stream(&self) -> bool {
        self.body.is_end_stream()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::convert::Infallible;
    use std::future::{Ready, poll_fn, ready};
    use std::time::Duration;

    use super::*;

    /// A body of these frames and errors, in order, then its end.
    struct Frames(VecDeque<Result<Frame<Bytes>, Status>>);

    impl http_body::Body for Frames {
        type Data = Bytes;
        type Error = Status;

        fn poll_frame(
            mut self: Pin<&mut Self>,
            _: &mut Context<'_>,
        ) -> Poll<Option<Result<Frame<Bytes>, Status>>> {
            Poll::Ready(self.0.pop_front())
        }
    }

    /// A service that hands back the request it is given.
    #[derive(Clone)]
    struct Echo;

    impl NamedService for Echo {
        const NAME: &'static str = "echo.Echo";
    }

    impl Service<http::Request<Body>> for Echo {
        type Response = http::Request<Body>;
        type Error = Infallible;
        type Future = Ready<Result<http::Request<Body>, Infallible>>;

        fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Infallible>> {
            Poll::Ready(Ok(()))
        }

        fn call(&mut self, request: http::Request<Body>) -> Self::Future {
            ready(Ok(request))
        }
    }

    /// The request `Watched` hands on for a body of `frames`.
    async fn watched(frames: Vec<Result<Frame<Bytes>, Status>>) -> http::Request<Body> {
        let mut service = Watched(Echo);
        poll_fn(|cx| service.poll_ready(cx)).await.expect("ready");
        let body = Body::new(Frames(frames.into()));
        service
            .call(http::Request::new(body))
            .await
            .expect("handed on")
    }

    async fn next(body: &mut Body) -> Option<Result<Frame<Bytes>, Status>> {
        poll_fn(|cx| http_body::Body::poll_frame(Pin::new(&mut *body), cx)).await
    }

    const STEP: Duration = Duration::from_secs(5);

    /// Asserts that the wrapped service keeps the inner one's name, and that the request it hands
    /// on carries progress that starts when the call began.
    #[tokio::test(start_paused = true)]
    async fn the_request_handed_on_carries_progress_from_the_start_of_the_call() {
        assert_eq!(<Watched<Echo> as NamedService>::NAME, "echo.Echo");
        let began = Instant::now();

        let request = watched(Vec::new()).await;

        let progress = request.extensions().get::<Progress>().expect("progress");
        assert_eq!(progress.last(), began);
    }

    /// Asserts that a data frame with a byte moves the progress to the time it was read, and
    /// that the frames are handed on unchanged and in order.
    #[tokio::test(start_paused = true)]
    async fn a_frame_that_carries_a_byte_moves_the_progress() {
        let request = watched(vec![
            Ok(Frame::data(Bytes::from_static(b"a"))),
            Ok(Frame::data(Bytes::from_static(b"bc"))),
        ])
        .await;
        let progress = request
            .extensions()
            .get::<Progress>()
            .expect("progress")
            .clone();
        let mut body = request.into_body();

        tokio::time::advance(STEP).await;
        let first = next(&mut body).await.expect("a frame").expect("no error");
        assert_eq!(first.into_data().expect("data"), Bytes::from_static(b"a"));
        assert_eq!(progress.last(), Instant::now());
        tokio::time::advance(STEP).await;
        let second = next(&mut body).await.expect("a frame").expect("no error");
        assert_eq!(second.into_data().expect("data"), Bytes::from_static(b"bc"));

        assert_eq!(progress.last(), Instant::now());
        assert!(next(&mut body).await.is_none());
    }

    /// Asserts that an empty data frame, trailers, an error and the end of the body are not
    /// progress: the time stays when the call began.
    #[tokio::test(start_paused = true)]
    async fn frames_without_a_byte_and_the_end_are_not_progress() {
        let began = Instant::now();
        let request = watched(vec![
            Ok(Frame::data(Bytes::new())),
            Ok(Frame::trailers(http::HeaderMap::new())),
            Err(Status::internal("broken")),
        ])
        .await;
        let progress = request
            .extensions()
            .get::<Progress>()
            .expect("progress")
            .clone();
        let mut body = request.into_body();

        tokio::time::advance(STEP).await;
        let empty = next(&mut body).await.expect("a frame").expect("no error");
        assert_eq!(empty.into_data().expect("data"), Bytes::new());
        tokio::time::advance(STEP).await;
        let trailers = next(&mut body).await.expect("a frame").expect("no error");
        assert!(trailers.is_trailers());
        tokio::time::advance(STEP).await;
        let broken = next(&mut body)
            .await
            .expect("a frame")
            .expect_err("the error");
        assert_eq!(broken.message(), "broken");
        tokio::time::advance(STEP).await;
        assert!(next(&mut body).await.is_none());

        assert_eq!(progress.last(), began);
    }
}
