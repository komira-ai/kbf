//! Fixtures of the unit tests of the server: a server over in-memory tiers, and requests
//! signed for it.

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::header::HeaderMap;
use hyper::{Request, StatusCode};
use roxmltree::Document;

use crate::{Backend, FaultyBackend, MemoryBackend, Proxy, ProxyConfig, TestClock};

use super::auth::sign;
use super::{Server, ServerConfig};

pub(super) const KEY_ID: &str = "AKIDEXAMPLE";

pub(super) const SECRET: &str = "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY";

pub(super) const BUCKET: &str = "tier-bucket";

/// The time of the clock of the rig, which is 2027-01-15 08:00:00 UTC.
pub(super) const NOW: u64 = 1_800_000_000_000;

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

    /// Sends a request as it is and reads the answer.
    pub(super) async fn send_raw(&self, request: Request<Full<Bytes>>) -> Reply {
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
        self.send("PUT", &uri, &[("if-none-match", "*")], body).await
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
