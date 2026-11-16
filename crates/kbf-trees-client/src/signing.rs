//! The layer between tonic's client and its channel that signs each call: it reads the whole
//! encoded body (one message of a unary call, under the client's cap), hashes it, sets
//! `x-amz-date`, `x-amz-content-sha256` and `authorization`, and sends the same bytes on. A
//! call cannot be signed before its body is known, since the signature covers the body's
//! hash; tonic's interceptors see the message before it is encoded, so the signing is here.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use http::{HeaderName, HeaderValue, Request, Response};
use http_body_util::{BodyExt as _, Full};
use tonic::body::Body;
use tonic::codegen::Service;

use crate::config::{Clock, Key};
use crate::sign::sign;

/// The error of a layer that wraps a channel's.
pub(crate) type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// What signs: the key, its region, the authority the calls name, and the clock.
pub(crate) struct Signer {
    pub(crate) key: Key,
    pub(crate) region: String,
    pub(crate) authority: String,
    pub(crate) clock: Clock,
}

/// A service that signs each request and hands it to `inner`.
#[derive(Clone)]
pub(crate) struct Signing<S> {
    pub(crate) inner: S,
    pub(crate) signer: Arc<Signer>,
}

impl<S, B> Service<Request<Body>> for Signing<S>
where
    S: Service<Request<Body>, Response = Response<B>> + Clone + Send + 'static,
    S::Future: Send,
    S::Error: Into<BoxError>,
{
    type Response = Response<B>;
    type Error = BoxError;
    type Future = Pin<Box<dyn Future<Output = Result<Response<B>, BoxError>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), BoxError>> {
        self.inner.poll_ready(cx).map_err(Into::into)
    }

    fn call(&mut self, request: Request<Body>) -> Self::Future {
        // The service that was made ready goes with this call; a clone stays for the next.
        let clone = self.inner.clone();
        let mut inner = std::mem::replace(&mut self.inner, clone);
        let signer = Arc::clone(&self.signer);
        Box::pin(async move {
            let (mut parts, body) = request.into_parts();
            let bytes = body.collect().await?.to_bytes();
            let key = (signer.key.id(), signer.key.secret());
            let at = (signer.clock)();
            let path = parts.uri.path();
            let head = sign(key, &signer.region, &signer.authority, path, &bytes, at);
            let set = [
                ("x-amz-date", head.stamp),
                ("x-amz-content-sha256", head.payload),
                ("authorization", head.authorization),
            ];
            // Each value is printable ASCII (the key's id and the region are checked when the
            // configuration is made), so none is dropped here.
            parts
                .headers
                .extend(set.into_iter().filter_map(|(name, value)| {
                    let value = HeaderValue::try_from(value).ok()?;
                    Some((HeaderName::from_static(name), value))
                }));
            let request = Request::from_parts(parts, Body::new(Full::new(bytes)));
            inner.call(request).await.map_err(Into::into)
        })
    }
}
