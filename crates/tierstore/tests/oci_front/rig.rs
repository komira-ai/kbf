//! A front on a loopback port over a proxy with in-memory tiers, and a client that trusts it.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use kbf_testkit::dir::TestDir;
use sha2::{Digest as _, Sha256};
use tierstore::server::oci::OciError;
use tierstore::server::oci::{OciFront, OciLimits, OciListener, OciMetrics, OciPrefix, acceptor};
use tierstore::{Backend, MemoryBackend, Proxy, ProxyConfig, SystemClock};
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio_rustls::{TlsAcceptor, TlsConnector};

use crate::client::{Conn, Reply, connector};
use crate::pki::{ca, ip, leaf};

pub const PREFIX: &str = "oci/";

pub fn hex_of(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

pub struct Rig {
    pub addr: SocketAddr,
    pub cold: Arc<MemoryBackend>,
    pub proxy: Arc<Proxy>,
    pub metrics: Arc<OciMetrics>,
    pub connector: TlsConnector,
    /// `None` for a plain front.
    tls: Option<TlsAcceptor>,
    plain: bool,
    front: Arc<OciFront>,
    limits: OciLimits,
    stop: watch::Sender<bool>,
    served: JoinHandle<usize>,
    _dir: TestDir,
}

impl Rig {
    pub async fn start() -> Self {
        Self::with(|_| {}).await
    }

    /// A front that serves plain HTTP on 127.0.0.1.
    pub async fn start_plain() -> Self {
        Self::plain_with(|_| {}).await
    }

    /// A plain front whose limits are the defaults of the tests, changed by `change`.
    pub async fn plain_with(change: impl FnOnce(&mut OciLimits)) -> Self {
        let listen = "127.0.0.1:0".parse().expect("an address");
        Self::build(true, listen, change).await
    }

    /// A front whose limits are the defaults of the tests, changed by `change`.
    pub async fn with(change: impl FnOnce(&mut OciLimits)) -> Self {
        let listen = "127.0.0.1:0".parse().expect("an address");
        Self::build(false, listen, change).await
    }

    /// A plain front at `listen`, which must be loopback.
    pub async fn plain_at(listen: SocketAddr) -> Self {
        Self::build(true, listen, |_| {}).await
    }

    async fn build(plain: bool, listen: SocketAddr, change: impl FnOnce(&mut OciLimits)) -> Self {
        let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "oci-rig");
        let ca = ca();
        let files = leaf(dir.path(), "front", &ca, vec![ip("127.0.0.1")]);
        let tls = (!plain).then(|| acceptor(&files.cert, &files.key, listen).expect("an acceptor"));
        let hot = Arc::new(MemoryBackend::new());
        let cold = Arc::new(MemoryBackend::new());
        let mut config = ProxyConfig::new("oci-rig", 1 << 30, Arc::new(SystemClock));
        config.linger = Duration::from_millis(1);
        config.usage_refresh = Duration::ZERO;
        let opened = Proxy::open(hot, cold.clone(), config).await;
        let proxy = Arc::new(opened.expect("a proxy"));
        let metrics = Arc::new(OciMetrics::default());
        let prefix = OciPrefix::new(PREFIX).expect("a prefix");
        let front = Arc::new(OciFront::new(proxy.clone(), prefix, metrics.clone()));
        let mut limits = OciLimits {
            max_connections: 16,
            handshake_timeout: Duration::from_secs(30),
            header_timeout: Duration::from_secs(30),
            write_idle: Duration::from_secs(30),
            lifetime: Duration::from_secs(3600),
            drain_deadline: Duration::from_secs(30),
        };
        change(&mut limits);
        let bound = match &tls {
            Some(tls) => OciListener::bind(listen, tls.clone(), front.clone(), limits).await,
            None => OciListener::bind_plain(listen, front.clone(), limits).await,
        };
        let listener = bound.expect("bound");
        let addr = listener.local_addr().expect("an address");
        let (stop, stopped) = watch::channel(false);
        let served = tokio::spawn(listener.serve(stopped));
        Self {
            addr,
            cold,
            proxy,
            metrics,
            connector: connector(&ca.pem),
            tls,
            plain,
            front,
            limits,
            stop,
            served,
            _dir: dir,
        }
    }

    /// Stores `content` where its digest names it, through the proxy, and gives `sha256:<hex>`.
    pub async fn seed(&self, content: &[u8]) -> String {
        let hex = hex_of(content);
        self.put_under(&hex, content).await;
        format!("sha256:{hex}")
    }

    /// Stores `content` where `hex` names it, whatever it hashes to.
    pub async fn put_under(&self, hex: &str, content: &[u8]) {
        let key = format!("{PREFIX}blobs/sha256/{hex}");
        let put = self.proxy.put(&key, Bytes::copy_from_slice(content)).await;
        put.expect("put");
    }

    pub async fn conn(&self) -> Conn {
        let opened = if self.plain {
            Conn::open_plain(self.addr).await
        } else {
            Conn::open(self.addr, &self.connector).await
        };
        opened.expect("a connection")
    }

    /// One request on a connection of its own.
    pub async fn ask(&self, method: &str, path: &str, headers: &[(&str, &str)]) -> Option<Reply> {
        self.conn().await.request(method, path, headers).await
    }

    pub async fn get(&self, path: &str) -> Reply {
        self.ask("GET", path, &[]).await.expect("an answer")
    }

    /// Stops the listener, and gives the number of connections it cut.
    pub async fn stop(self) -> usize {
        self.stop.send(true).expect("a listener");
        self.served.await.expect("the listener ended")
    }
}

impl Rig {
    /// Binds the address this front listens on again, which is in use.
    pub async fn bind_again(&self) -> Result<OciListener, OciError> {
        let tls = self.tls.clone().expect("a front with TLS");
        let front = self.front.clone();
        OciListener::bind(self.addr, tls, front, self.limits).await
    }
}
