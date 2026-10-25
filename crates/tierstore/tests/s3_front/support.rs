//! A server on a loopback port over in-memory tiers, and the clients that reach it.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use kbf_objstore::Capabilities;
use kbf_objstore::s3::{Credentials, S3Config, S3Store};
use tierstore::server::{Drained, Server, ServerConfig};
use tierstore::{Clock, FaultyBackend, MemoryBackend, Proxy, ProxyConfig, SystemClock, TestClock};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

use crate::signer::{FIXED_STAMP, Request, sign};

pub const KEY_ID: &str = "AKIDTIERSTORE";

pub const SECRET: &str = "tierstore-test-secret";

pub const BUCKET: &str = "tier-bucket";

/// The time, in milliseconds, of a server whose clock is set by hand: `signer::FIXED_STAMP`.
pub const FIXED_MILLIS: u64 = 1_800_000_000_000;

/// A running server and its tiers.
pub struct Harness {
    pub addr: SocketAddr,
    pub server: Arc<Server>,
    pub hot_mem: Arc<MemoryBackend>,
    pub cold_mem: Arc<MemoryBackend>,
    pub hot: Arc<FaultyBackend>,
    pub cold: Arc<FaultyBackend>,
    /// The task that serves; it ends with the drain.
    pub served: JoinHandle<Drained>,
}

impl Harness {
    /// A server that tells the time by the clock of the machine, as a client of kbf does.
    pub async fn start(change: impl FnOnce(&mut ServerConfig)) -> Self {
        Self::start_with(Arc::new(SystemClock), change).await
    }

    /// A server whose clock stands at `FIXED_MILLIS`, so that a request can be signed for it
    /// without reading the time.
    pub async fn start_fixed(change: impl FnOnce(&mut ServerConfig)) -> Self {
        let clock = Arc::new(TestClock::new());
        clock.set(FIXED_MILLIS);
        Self::start_with(clock, change).await
    }

    async fn start_with(clock: Arc<dyn Clock>, change: impl FnOnce(&mut ServerConfig)) -> Self {
        let hot_mem = Arc::new(MemoryBackend::new());
        let cold_mem = Arc::new(MemoryBackend::new());
        let hot = Arc::new(FaultyBackend::new(hot_mem.clone()));
        let cold = Arc::new(FaultyBackend::new(cold_mem.clone()));
        let mut proxy_config = ProxyConfig::new("p1", 1 << 30, Arc::new(SystemClock));
        proxy_config.linger = Duration::from_millis(1);
        proxy_config.usage_refresh = Duration::ZERO;
        let opened = Proxy::open(hot.clone(), cold.clone(), proxy_config).await;
        let mut config = ServerConfig::new(BUCKET, KEY_ID, SECRET, clock);
        config.drain_lb_wait = Duration::from_millis(100);
        config.drain_deadline = Duration::from_secs(10);
        change(&mut config);
        let server = Server::new(Arc::new(opened.expect("a proxy")), config);
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bound");
        let addr = listener.local_addr().expect("an address");
        let serving = Arc::clone(&server);
        let served = tokio::spawn(async move { serving.serve(listener).await });
        Self {
            addr,
            server,
            hot_mem,
            cold_mem,
            hot,
            cold,
            served,
        }
    }

    /// The S3 client of kbf, with the key of the server.
    pub fn store(&self, capabilities: Capabilities) -> S3Store {
        self.store_with(KEY_ID, SECRET, capabilities)
    }

    /// The S3 client of kbf, with this key.
    pub fn store_with(&self, key_id: &str, secret: &str, capabilities: Capabilities) -> S3Store {
        let config = S3Config {
            endpoint: format!("http://{}", self.addr),
            region: "us-east-1".to_owned(),
            bucket: BUCKET.to_owned(),
            credentials: Credentials::new(key_id, secret),
            capabilities,
            connect_timeout: Duration::from_secs(5),
            request_timeout: Duration::from_secs(30),
            min_body_rate: None,
        };
        S3Store::new(config).expect("a store")
    }

    /// The headers that sign a request for the server at the fixed time.
    pub fn signed(
        &self,
        method: &str,
        path: &str,
        query: &[(&str, &str)],
        headers: &[(&str, &str)],
        body: &[u8],
    ) -> Vec<(String, String)> {
        self.signed_at(FIXED_STAMP, method, path, query, headers, body)
    }

    /// The headers that sign a request for the server at the fixed time with the key
    /// `(id, secret)`, whichever key that is.
    pub fn signed_as(
        &self,
        key: (&str, &str),
        method: &str,
        path: &str,
        query: &[(&str, &str)],
        headers: &[(&str, &str)],
        body: &[u8],
    ) -> Vec<(String, String)> {
        let host = self.addr.to_string();
        let request = Request {
            method,
            host: &host,
            path,
            query,
            headers,
            body,
            stamp: FIXED_STAMP,
        };
        sign(&request, key.0, key.1)
    }

    /// The headers that sign a request made at the time `stamp` names.
    pub fn signed_at(
        &self,
        stamp: &str,
        method: &str,
        path: &str,
        query: &[(&str, &str)],
        headers: &[(&str, &str)],
        body: &[u8],
    ) -> Vec<(String, String)> {
        let host = self.addr.to_string();
        let request = Request {
            method,
            host: &host,
            path,
            query,
            headers,
            body,
            stamp,
        };
        sign(&request, KEY_ID, SECRET)
    }
}
