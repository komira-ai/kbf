//! Fixtures of the unit tests of the server: a server over in-memory tiers, and requests
//! signed for it.

use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use async_trait::async_trait;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use bytes::Bytes;
use http_body_util::BodyExt;
use hyper::body::{Body, Frame};
use hyper::header::HeaderMap;
use hyper::{Request, StatusCode};
use md5::Md5;
use ring::digest::{SHA1_FOR_LEGACY_USE_ONLY, digest as sha1_of};
use roxmltree::Document;
use sha2::{Digest, Sha256};
use tokio::time::{Sleep, sleep};

use crate::{
    Backend, ByteRange, Error, FaultyBackend, Listed, MemoryBackend, Meta, Proxy, ProxyConfig,
    TestClock,
};

use super::auth::{sign, sign_as};
use super::request::BoxError;
use super::{Server, ServerConfig};

pub(super) const KEY_ID: &str = "AKIDEXAMPLE";

pub(super) const SECRET: &str = "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY";

pub(super) const BUCKET: &str = "tier-bucket";

/// The time of the clock of the rig, which is 2027-01-15 08:00:00 UTC.
pub(super) const NOW: u64 = 1_800_000_000_000;

/// The headers in which a request may carry a checksum of its body.
pub(super) const CHECKSUMS: [&str; 5] = [
    "content-md5",
    "x-amz-checksum-crc32",
    "x-amz-checksum-crc32c",
    "x-amz-checksum-sha1",
    "x-amz-checksum-sha256",
];

/// The value of the checksum header `name` for `body`, worked out apart from the server.
pub(super) fn checksum(name: &str, body: &[u8]) -> String {
    let hash: Vec<u8> = match name {
        "content-md5" => Md5::digest(body).to_vec(),
        "x-amz-checksum-crc32" => crc32fast::hash(body).to_be_bytes().to_vec(),
        "x-amz-checksum-crc32c" => crc32c::crc32c(body).to_be_bytes().to_vec(),
        "x-amz-checksum-sha1" => sha1_of(&SHA1_FOR_LEGACY_USE_ONLY, body).as_ref().to_vec(),
        _ => Sha256::digest(body).to_vec(),
    };
    STANDARD.encode(hash)
}

/// The text of every element called `name` in `xml`, in document order.
pub(super) fn texts(xml: &str, name: &str) -> Vec<String> {
    let doc = Document::parse(xml).expect("well-formed XML");
    doc.descendants()
        .filter(|node| node.tag_name().name() == name)
        .map(|node| node.text().unwrap_or_default().to_owned())
        .collect()
}

/// What an answer was: its status, headers and body text.
pub(super) struct Reply {
    pub(super) status: StatusCode,
    pub(super) headers: HeaderMap,
    pub(super) body: String,
}

impl Reply {
    /// The value of a header, or the empty text.
    pub(super) fn header(&self, name: &str) -> &str {
        self.headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
    }

    /// The code of the error document in the body.
    pub(super) fn code(&self) -> String {
        texts(&self.body, "Code").concat()
    }
}

/// A server over a proxy whose tiers are in memory, each behind a fault injector, with a
/// clock set by hand.
pub(super) struct Rig {
    pub(super) hot_mem: Arc<MemoryBackend>,
    pub(super) cold_mem: Arc<MemoryBackend>,
    pub(super) hot: Arc<FaultyBackend>,
    pub(super) cold: Arc<FaultyBackend>,
    pub(super) config: ServerConfig,
    pub(super) server: Arc<Server>,
}

impl Rig {
    /// A rig whose server serves the proxy `p1` with a hot tier of `budget` bytes.
    pub(super) async fn new(budget: u64) -> Self {
        Self::with(budget, |_| {}).await
    }

    /// The same, with the settings of the server changed by `change`.
    pub(super) async fn with(budget: u64, change: impl FnOnce(&mut ServerConfig)) -> Self {
        let clock = Arc::new(TestClock::new());
        clock.set(NOW);
        let hot_mem = Arc::new(MemoryBackend::with_clock(Arc::clone(&clock)));
        let cold_mem = Arc::new(MemoryBackend::with_clock(Arc::clone(&clock)));
        let hot = Arc::new(FaultyBackend::new(hot_mem.clone()));
        let cold = Arc::new(FaultyBackend::new(cold_mem.clone()));
        let mut proxy_config = ProxyConfig::new("p1", budget, clock.clone());
        proxy_config.linger = Duration::from_millis(1);
        proxy_config.usage_refresh = Duration::ZERO;
        let proxy = Proxy::open(hot.clone(), cold.clone(), proxy_config).await;
        let backend: Arc<dyn Backend> = Arc::new(proxy.expect("a proxy"));
        let mut config = ServerConfig::new(BUCKET, KEY_ID, SECRET, clock);
        change(&mut config);
        let server = Server::new(backend, config.clone());
        Self {
            hot_mem,
            cold_mem,
            hot,
            cold,
            config,
            server,
        }
    }

    /// Sends a request signed for the server and reads the answer.
    pub(super) async fn send(
        &self,
        method: &str,
        uri: &str,
        extra: &[(&str, &str)],
        body: &[u8],
    ) -> Reply {
        let request = sign(&self.config, method, uri, extra, body);
        self.send_raw(request).await
    }

    /// Sends a request signed with the key `(id, secret)`, whichever key that is, and reads the
    /// answer.
    pub(super) async fn send_as(
        &self,
        key: (&str, &str),
        method: &str,
        uri: &str,
        extra: &[(&str, &str)],
        body: &[u8],
    ) -> Reply {
        let request = sign_as(&self.config, key, method, uri, extra, body);
        self.send_raw(request).await
    }

    /// Sends a request signed for the server whose body is the one `played` plays, in place of
    /// the bytes `signed`, which are what the signature covers.
    pub(super) async fn send_played(
        &self,
        method: &str,
        uri: &str,
        extra: &[(&str, &str)],
        signed: &[u8],
        played: Scripted,
    ) -> Reply {
        let (parts, _) = sign(&self.config, method, uri, extra, signed).into_parts();
        self.send_raw(Request::from_parts(parts, played)).await
    }

    /// Sends a request as it is and reads the answer.
    pub(super) async fn send_raw<B>(&self, request: Request<B>) -> Reply
    where
        B: Body + Send,
        B::Data: Send,
        B::Error: Into<BoxError>,
    {
        let response = self.server.handle(request).await;
        let (parts, body) = response.into_parts();
        let bytes = body.collect().await.expect("a whole body").to_bytes();
        Reply {
            status: parts.status,
            headers: parts.headers,
            body: String::from_utf8_lossy(&bytes).into_owned(),
        }
    }

    /// Sends a request for an object of the bucket (`key` may carry a query).
    pub(super) async fn call(&self, method: &str, key: &str, body: &[u8]) -> Reply {
        let uri = format!("/{BUCKET}/{key}");
        self.send(method, &uri, &[], body).await
    }

    /// Puts an object through the server, create-only.
    pub(super) async fn put(&self, key: &str, body: &[u8]) -> Reply {
        let uri = format!("/{BUCKET}/{key}");
        self.send("PUT", &uri, &[("if-none-match", "*")], body)
            .await
    }

    /// Gets an object through the server, with a `Range` header if there is one.
    pub(super) async fn get(&self, key: &str, range: Option<&str>) -> Reply {
        let uri = format!("/{BUCKET}/{key}");
        let extra: Vec<(&str, &str)> = range.map(|r| ("range", r)).into_iter().collect();
        self.send("GET", &uri, &extra, b"").await
    }

    /// A listing of the bucket (`query` is what follows `list-type=2&`).
    pub(super) async fn list(&self, query: &str) -> Reply {
        let uri = format!("/{BUCKET}?list-type=2&{query}");
        self.send("GET", &uri, &[], b"").await
    }
}

/// One step of the body that a [`Scripted`] plays.
pub(super) enum Step {
    /// A piece of the body.
    Data(&'static [u8]),
    /// Nothing happens for this long (tokio time).
    Wait(Duration),
    /// A frame of trailers, which carries no byte of the body.
    Trailers,
    /// The body fails.
    Fail,
}

/// A request body that plays steps, so that a test says when its pieces arrive.
pub(super) struct Scripted {
    steps: VecDeque<Step>,
    waiting: Option<Pin<Box<Sleep>>>,
}

impl Scripted {
    pub(super) fn new(steps: Vec<Step>) -> Self {
        Self {
            steps: steps.into(),
            waiting: None,
        }
    }
}

impl Body for Scripted {
    type Data = Bytes;
    type Error = std::io::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, std::io::Error>>> {
        let this = self.get_mut();
        loop {
            if let Some(timer) = this.waiting.as_mut() {
                if timer.as_mut().poll(cx).is_pending() {
                    return Poll::Pending;
                }
                this.waiting = None;
            }
            let Some(step) = this.steps.pop_front() else {
                return Poll::Ready(None);
            };
            match step {
                Step::Data(bytes) => {
                    return Poll::Ready(Some(Ok(Frame::data(Bytes::from_static(bytes)))));
                }
                Step::Wait(time) => this.waiting = Some(Box::pin(sleep(time))),
                Step::Trailers => {
                    return Poll::Ready(Some(Ok(Frame::trailers(HeaderMap::new()))));
                }
                Step::Fail => return Poll::Ready(Some(Err(std::io::Error::other("cut")))),
            }
        }
    }
}

/// A backend that does what the trait requires of it and no multipart upload: it holds nothing.
pub(super) struct Plain;

#[async_trait]
impl Backend for Plain {
    async fn put(&self, key: &str, _body: Bytes) -> Result<(), Error> {
        Err(Error::Full(key.to_owned()))
    }

    async fn put_if_match(&self, key: &str, _body: Bytes, _etag: &str) -> Result<String, Error> {
        Err(Error::NotFound(key.to_owned()))
    }

    async fn get(&self, key: &str, _range: Option<ByteRange>) -> Result<Bytes, Error> {
        Err(Error::NotFound(key.to_owned()))
    }

    async fn head(&self, key: &str) -> Result<Meta, Error> {
        Err(Error::NotFound(key.to_owned()))
    }

    async fn delete(&self, _key: &str) -> Result<(), Error> {
        Ok(())
    }

    async fn list(&self, _prefix: &str) -> Result<Vec<Listed>, Error> {
        Ok(Vec::new())
    }
}
