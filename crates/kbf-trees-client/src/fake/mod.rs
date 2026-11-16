//! A tierstore trees interface in the process, for tests of the code above
//! [`Client`](crate::Client).
//!
//! [`FakeTreesServer`] listens on a loopback port and serves the generated `TreesService` of
//! `tierstore-trees-api` with the node's rules (tierstore's `node-local-trees.md` and its
//! `server/trees` front):
//!
//! - **Every call is signed.** A call whose `Authorization` is missing or malformed, names
//!   another key, region or service, does not sign `host`, `x-amz-content-sha256` and
//!   `x-amz-date`, is more than 15 minutes from the fake's clock
//!   ([`set_clock`](FakeTreesServer::set_clock)), has a signature that does not match, or
//!   carries a body whose SHA-256 is not the signed one, is `UNAUTHENTICATED`
//!   (`unauthenticated: <why>`, in the fake's words: the node words its reasons otherwise, so
//!   code above this crate matches the reason and the code, never the text) and is not
//!   logged. The key is [`KEY_ID`] and [`SECRET`] in
//!   [`REGION`]; every head the fake received is kept ([`heads`](FakeTreesServer::heads)).
//! - **A create is idempotent by the tree and its locations.** The same tree at the same
//!   locations answers the placement made first, `created: false`, with its lease renewed;
//!   another tree at a location of a placement is `location taken`. A placement made with
//!   `prefetch` `wait` (or unspecified) is `READY`, `background` `FILLING`, `none` `LAZY`.
//! - **The request alone**: an empty, `.` or `..` name, two entries at one path, a digest
//!   that is not 32 bytes, or a mode above 07777 is refused by name (`INVALID_ARGUMENT`).
//! - **A child tree entry** names a tree the fake has (made by an earlier create, and not
//!   [`forget_tree`](FakeTreesServer::forget_tree)-ed), else `unknown child tree`.
//! - **The tree's digest is the fake's**: a hash of the entries' paths, kinds, modes,
//!   `sha256`s, sizes, targets and child digests (never where a file's bytes come from),
//!   salted, so that it is no digest kbf could compute and a test can make the fake another
//!   node's builder ([`set_digest_salt`](FakeTreesServer::set_digest_salt)). A request that
//!   carries a digest of its own other than the fake's is `tree digest mismatch`.
//! - **Sources the test marks** ([`mark_source`](FakeTreesServer::mark_source), by object
//!   key: `from.key`, or `cas/l/<sha256 in hex>` for a whole object) are read only by a
//!   create that fills, after its locations and the cache are checked: a `wait` create fails
//!   whole with `source missing` naming the key, or `short` or `mismatch` naming the entry; a
//!   `background` create keeps its placement `FAILED`, each bad entry in its `failures`; a
//!   `none` create reads no source.
//! - **The cache**: a `wait` create of more file bytes than
//!   [`set_pin_limit`](FakeTreesServer::set_pin_limit) is `tree too large for the cache`
//!   (`RESOURCE_EXHAUSTED`); the same tree with `background` is placed.
//! - **Placements** are read and released by id; an id the fake does not have is `unknown
//!   placement`. A read with `list` answers one page of a directory's children.
//!
//! A [`Fault`] breaks the next call in a way a node or the network can.
//! [`calls`](FakeTreesServer::calls) is every request the fake read past the signature check,
//! decoded.

mod http;
mod service;
mod state;

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use tierstore_trees_api::v1::{CreateTreeRequest, DeleteTreeRequest, GetPlacementRequest};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tonic::Code;

pub use state::Source;
use state::State;

use crate::config::{Clock, Config, Key};
use crate::error::ConfigError;
use crate::types::TreeDigest;

/// The id of the one key the fake knows, the full key of its node.
pub const KEY_ID: &str = "AKIDFAKETREES";

/// The secret of [`KEY_ID`].
pub const SECRET: &str = "fake-trees-secret-0123456789abcdef";

/// The region of the fake's signatures.
pub const REGION: &str = "fake-region";

/// How the next call goes wrong.
#[derive(Debug, Clone)]
pub enum Fault {
    /// The call is applied, then the connection is closed before the answer: the node has
    /// acted and the client has no answer.
    LoseAnswer,
    /// The call is read and logged, never answered, and not applied.
    /// [`FakeTreesServer::hung`] tells when a call has taken it.
    Hang,
    /// The call is read and logged, answered with this status, and not applied.
    Status {
        /// The status's code.
        code: Code,
        /// The status's message.
        message: String,
    },
    /// The call is applied and answered OK with these bytes as its message.
    Answer(Bytes),
}

/// One call the fake read, with its request as decoded.
#[derive(Debug, Clone, PartialEq)]
pub enum Call {
    /// `CreateTree`.
    CreateTree(Box<CreateTreeRequest>),
    /// `GetPlacement`.
    GetPlacement(GetPlacementRequest),
    /// `DeleteTree`.
    DeleteTree(DeleteTreeRequest),
}

struct Shared {
    state: Mutex<State>,
    /// Each fault, with the method it breaks (empty: any).
    faults: Mutex<VecDeque<(String, Fault)>>,
    calls: Mutex<Vec<Call>>,
    heads: Mutex<Vec<::http::HeaderMap>>,
    clock: Mutex<Clock>,
    /// Told each time a call takes a [`Fault::Hang`].
    hung: tokio::sync::Notify,
}

impl Shared {
    fn state(&self) -> MutexGuard<'_, State> {
        locked(&self.state)
    }

    fn record(&self, call: Call) {
        locked(&self.calls).push(call);
    }

    fn record_head(&self, head: ::http::HeaderMap) {
        locked(&self.heads).push(head);
    }

    /// The fake's time, which the leases are reckoned from too.
    fn now(&self) -> SystemTime {
        let now = (*locked(&self.clock))();
        let since = now.duration_since(UNIX_EPOCH).unwrap_or_default();
        let ms = u64::try_from(since.as_millis()).unwrap_or(u64::MAX);
        self.state().set_now_ms(ms);
        now
    }

    /// The first fault queued for `method` or for any method, taken off the queue.
    fn fault_for(&self, method: &str) -> Option<Fault> {
        let mut faults = locked(&self.faults);
        let at = faults
            .iter()
            .position(|(on, _)| on.is_empty() || on == method)?;
        faults.remove(at).map(|(_, fault)| fault)
    }
}

fn locked<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A trees interface on a loopback port, until it drops.
pub struct FakeTreesServer {
    shared: Arc<Shared>,
    addr: SocketAddr,
    tls: bool,
    task: JoinHandle<std::io::Result<()>>,
}

impl Drop for FakeTreesServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl FakeTreesServer {
    /// Serves over plain HTTP/2 on a port of `127.0.0.1` the system picks.
    ///
    /// # Errors
    /// No loopback port can be bound.
    pub async fn start() -> std::io::Result<Self> {
        Self::serve(None).await
    }

    /// Serves over TLS under `config` on a port of `127.0.0.1`; [`node`](Self::node) names
    /// the host `localhost`, so the certificate must carry that name. The fake offers `h2`
    /// as the TLS protocol, whatever `config` says.
    ///
    /// # Errors
    /// No loopback port can be bound.
    pub async fn start_tls(config: Arc<rustls::ServerConfig>) -> std::io::Result<Self> {
        let mut config = (*config).clone();
        config.alpn_protocols = vec![b"h2".to_vec()];
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
        Self::serve(Some(acceptor)).await
    }

    async fn serve(acceptor: Option<tokio_rustls::TlsAcceptor>) -> std::io::Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let shared = Arc::new(Shared {
            state: Mutex::new(State::new()),
            faults: Mutex::new(VecDeque::new()),
            calls: Mutex::new(Vec::new()),
            heads: Mutex::new(Vec::new()),
            clock: Mutex::new(SystemTime::now),
            hung: tokio::sync::Notify::new(),
        });
        let tls = acceptor.is_some();
        let task = tokio::spawn(http::accept_loop(listener, acceptor, Arc::clone(&shared)));
        Ok(Self {
            shared,
            addr,
            tls,
            task,
        })
    }

    /// The address clients dial: `127.0.0.1:<port>`, or `localhost:<port>` over TLS.
    #[must_use]
    pub fn node(&self) -> String {
        match self.tls {
            true => format!("localhost:{}", self.addr.port()),
            false => self.addr.to_string(),
        }
    }

    /// The config of a client of this fake, with its key and region and without a CA.
    ///
    /// # Errors
    /// None in practice: the address and the key are the fake's own.
    pub fn config(&self) -> Result<Config, ConfigError> {
        Config::new(self.node().parse()?, Key::new(KEY_ID, SECRET)?, REGION)
    }

    // ---- the node's settings

    /// The clock the fake judges a signature's time by, and reckons leases from; the system's
    /// until this is called.
    pub fn set_clock(&self, clock: Clock) {
        *locked(&self.shared.clock) = clock;
    }

    /// Makes the fake a node of another digest builder: its digests of the same entries
    /// change.
    pub fn set_digest_salt(&self, salt: &[u8]) {
        self.shared.state().set_salt(salt);
    }

    /// The most file bytes a `wait` create may pin; no limit until this is called.
    pub fn set_pin_limit(&self, bytes: u64) {
        self.shared.state().set_pin_limit(bytes);
    }

    /// Marks the object `key` (or clears its mark, with `None`).
    pub fn mark_source(&self, key: &str, source: Option<Source>) {
        self.shared.state().mark(key, source);
    }

    /// Forgets the tree `tree`, as a node does once nothing holds it. False when the fake did
    /// not have it.
    pub fn forget_tree(&self, tree: TreeDigest) -> bool {
        self.shared.state().forget_tree(tree)
    }

    // ---- what the test looks at

    /// Whether the fake has the tree `tree`.
    #[must_use]
    pub fn has_tree(&self, tree: TreeDigest) -> bool {
        self.shared.state().has_tree(tree)
    }

    /// The ids of the live placements, in id order.
    #[must_use]
    pub fn placements(&self) -> Vec<String> {
        self.shared.state().placements()
    }

    /// Every call the fake read past the signature check, in arrival order, with its request
    /// as decoded: those it handled and those a [`Fault`] answered without applying them.
    #[must_use]
    pub fn calls(&self) -> Vec<Call> {
        locked(&self.shared.calls).clone()
    }

    /// The headers of every call the fake received, signed or not, in arrival order.
    #[must_use]
    pub fn heads(&self) -> Vec<::http::HeaderMap> {
        locked(&self.shared.heads).clone()
    }

    // ---- faults

    /// Breaks the next call that arrives (after the faults already queued).
    pub fn inject(&self, fault: Fault) {
        self.inject_on("", fault);
    }

    /// Breaks the next call of `method` (such as `CreateTree`); other calls are served as
    /// usual.
    pub fn inject_on(&self, method: &str, fault: Fault) {
        locked(&self.shared.faults).push_back((method.to_owned(), fault));
    }

    /// Waits until a call has taken a [`Fault::Hang`] (at once if one has since the last
    /// wait): the request has reached the fake and will never be answered.
    pub async fn hung(&self) {
        self.shared.hung.notified().await;
    }
}
