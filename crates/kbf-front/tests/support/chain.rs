//! The chain rig: a few nodes, each a real `Cache` (a metadata log, a store and a tier
//! directory of its own) behind a real `StorageService` served over mutual TLS on loopback,
//! with a chain, a repair journal, a repair worker and a peer confirmer, as `kbf storage`
//! assembles them. A node can be killed (its process state is gone, its tier directory is
//! cut at the last sync, its metadata log and store survive) and started again, and a
//! `Router` reads and writes the cluster as a frontend does.
//!
//! The tests drive the drain by hand (`Cache::drain_held`) so that "acknowledged, not yet
//! on disk" is a state they hold, not one they race: the fs sync gate
//! ([`Counting::pause`]) parks the pool's writers in `sync`, the log's hold
//! ([`Log::hold`]) parks a `PutStaged`, and the pool's drain hook parks a writer after the
//! index took the blobs. Nothing here sleeps. Included with `#[path]` after `mod common;`,
//! the write-tier `support` module and `rig`.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use futures::future::BoxFuture;
use kbf_front::{
    Cache, CacheStorage, DrainHook, MetaLog, MetaLogError, SegmentsConfig, TierStartup,
};
use kbf_map::{Map, Member, MemberId, MemberState, Rule};
use kbf_meta::{Applied, Command, MetaState};
use kbf_objstore::{Capabilities, KeyPrefix};
use kbf_route::{MapSource as RouteMaps, MapView, Nodes, RefetchError, Router};
use kbf_storage::repair::{NoLedger, Options, PeerConfirmer, RepairConfig, RepairWorker, Repairs};
use kbf_storage::replicate::{Chain, ChainConfig, MapSource, Peers};
use kbf_storage::service::{Admission, Config, ROLE_URI_PREFIX, RemoteStorage, StorageService};
use kbf_storage::{Storage, StorageError};
use kbf_store::FaultFs;
use kbf_testkit::switch::Switch;
use kbf_tier::TierOptions;
use kbf_types::Digest;
use rcgen::{
    BasicConstraints, CertificateParams, CertifiedIssuer, DnType, ExtendedKeyUsagePurpose, IsCa,
    KeyPair, KeyUsagePurpose, SanType,
};
use tokio::sync::Notify;
use tonic::transport::server::TcpIncoming;
use tonic::transport::{
    Certificate, Channel, ClientTlsConfig, Endpoint, Identity, Server, ServerTlsConfig,
};

use crate::common::faulty::FaultyStore;
use crate::rig::{Counting, pool};
use crate::support::{Log, SEGMENT, SERVER, TestBlob};

const GIB: u64 = 1 << 30;

/// The cache type of a node: its metadata log can die with it.
pub type NodeCache = Cache<Mortal, FaultyStore>;

/// A metadata log that stops answering when its process dies: a commit parked at the log's
/// hold when the node is killed is dropped without being applied, as a proposal that never
/// reached the leader is; one already past the hold applies, as one that reached it does.
#[derive(Clone)]
pub struct Mortal {
    log: Log,
    dead: Arc<Death>,
}

struct Death {
    flag: AtomicBool,
    gone: Notify,
}

impl Mortal {
    fn new(log: Log) -> Self {
        Self {
            log,
            dead: Arc::new(Death {
                flag: AtomicBool::new(false),
                gone: Notify::new(),
            }),
        }
    }

    fn kill(&self) {
        self.dead.flag.store(true, Ordering::SeqCst);
        self.dead.gone.notify_waiters();
    }

    fn gone() -> MetaLogError {
        MetaLogError::Unavailable("the process died".to_owned())
    }
}

impl MetaLog for Mortal {
    async fn commit(&self, command: Command) -> Result<Applied, MetaLogError> {
        let gone = self.dead.gone.notified();
        tokio::pin!(gone);
        gone.as_mut().enable();
        if self.dead.flag.load(Ordering::SeqCst) {
            return Err(Self::gone());
        }
        // Biased: a proposal the process died holding is dropped even if the hold it is
        // parked at ends in the same instant.
        // The log asks the flag itself, after the hold and before it applies anything, so
        // a release that races the kill cannot get the proposal applied.
        let dead = Arc::clone(&self.dead);
        tokio::select! {
            biased;
            () = &mut gone => Err(Self::gone()),
            applied = self.log.commit_while(command, move || !dead.flag.load(Ordering::SeqCst)) => applied,
        }
    }

    async fn query<R, F>(&self, f: F) -> Result<R, MetaLogError>
    where
        F: FnOnce(&MetaState) -> R + Send,
        R: Send,
    {
        if self.dead.flag.load(Ordering::SeqCst) {
            return Err(Self::gone());
        }
        MetaLog::query(&self.log, f).await
    }
}

/// The farm's one CA, and the certificates of its nodes and of the frontend.
pub struct Pki {
    ca: CertifiedIssuer<'static, KeyPair>,
}

impl Pki {
    pub fn new() -> Self {
        let mut ca = CertificateParams::new(Vec::<String>::new()).expect("CA params");
        ca.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        ca.distinguished_name
            .push(DnType::CommonName, "kbf test CA");
        ca.key_usages = vec![
            KeyUsagePurpose::KeyCertSign,
            KeyUsagePurpose::DigitalSignature,
        ];
        let ca = CertifiedIssuer::self_signed(ca, KeyPair::generate().expect("key")).expect("CA");
        Self { ca }
    }

    /// What a holder of certificate `role` serves with and calls with (one certificate
    /// that carries serverAuth and clientAuth and names the role).
    fn identity(&self, role: &str) -> (ServerTlsConfig, ClientTlsConfig) {
        let mut params = CertificateParams::new(vec!["localhost".to_owned()]).expect("params");
        params.subject_alt_names.push(SanType::URI(
            format!("{ROLE_URI_PREFIX}{role}")
                .try_into()
                .expect("an IA5 name"),
        ));
        params.extended_key_usages = vec![
            ExtendedKeyUsagePurpose::ServerAuth,
            ExtendedKeyUsagePurpose::ClientAuth,
        ];
        let key = KeyPair::generate().expect("key");
        let cert = params.signed_by(&key, &self.ca).expect("sign");
        let identity = || Identity::from_pem(cert.pem(), key.serialize_pem());
        let server = ServerTlsConfig::new()
            .identity(identity())
            .client_ca_root(Certificate::from_pem(self.ca.pem()));
        let client = ClientTlsConfig::new()
            .ca_certificate(Certificate::from_pem(self.ca.pem()))
            .domain_name("localhost")
            .identity(identity());
        (server, client)
    }

    /// A channel to `addr` as a caller of `role`, connected when first used.
    pub fn channel(&self, addr: SocketAddr, role: &str) -> Channel {
        let (_, client) = self.identity(role);
        Endpoint::from_shared(format!("https://{addr}"))
            .expect("an endpoint")
            .tls_config(client)
            .expect("TLS")
            .connect_lazy()
    }
}

/// The member id and map a node holds.
pub struct NodeMap {
    me: MemberId,
    map: Mutex<Option<Arc<Map>>>,
}

impl MapSource for NodeMap {
    fn current(&self) -> Option<Arc<Map>> {
        self.map.lock().expect("lock").clone()
    }

    fn me(&self) -> Option<MemberId> {
        Some(self.me)
    }
}

/// A node's persistent parts: what a kill does not take.
pub struct Disk {
    pub log: Log,
    pub store: FaultyStore,
    /// The tier directory, behind the sync gate and counters.
    pub fs: Counting,
    /// The repair journal's directory.
    pub repair_fs: FaultFs,
}

/// A running node.
pub struct Live {
    pub cache: Arc<NodeCache>,
    pub storage: Arc<CacheStorage<Mortal, FaultyStore>>,
    pub switch: Switch,
    pub held: Arc<NodeMap>,
    pub chain: Arc<Chain>,
    pub repairs: Arc<Repairs>,
    pub worker: Arc<RepairWorker>,
    pub confirmer: Arc<PeerConfirmer>,
    pub startup: TierStartup,
    mortal: Mortal,
    server: tokio::task::JoinHandle<()>,
}

impl Live {
    /// The node's address as the map names it.
    pub fn addr(&self) -> SocketAddr {
        self.switch.addr
    }
}

impl Drop for Live {
    fn drop(&mut self) {
        self.server.abort();
    }
}

/// One node of the cluster.
pub struct Node {
    pub id: MemberId,
    pub disk: Disk,
    pub live: Option<Live>,
    boot: u64,
}

impl Node {
    pub fn up(&self) -> &Live {
        self.live.as_ref().expect("the node is running")
    }

    pub fn cache(&self) -> &Arc<NodeCache> {
        &self.up().cache
    }
}

/// What a cluster is started with.
#[derive(Clone)]
pub struct Setup {
    /// `--ram-ack-bytes` of every node.
    pub budget: u64,
    /// The replication factor of the map.
    pub r: u8,
    /// Called by every node's pool writer after a batch's `PutStaged` applied.
    pub hook: Option<DrainHook>,
    /// How long a barrier waits for held blobs (`--durable-wait`).
    pub durable_wait: Duration,
}

impl Setup {
    pub fn new(budget: u64) -> Self {
        Self {
            budget,
            r: 2,
            hook: None,
            durable_wait: Duration::from_secs(600),
        }
    }
}

/// The cluster: nodes numbered from 0, member ids their numbers plus 1.
pub struct Cluster {
    pub pki: Arc<Pki>,
    pub nodes: Vec<Node>,
    pub generation: u64,
    options: Setup,
    maps: Arc<Cell>,
}

impl Drop for Cluster {
    /// A test that fails with the syncs paused must not leave the writers stuck.
    fn drop(&mut self) {
        for node in &self.nodes {
            node.disk.fs.resume();
        }
    }
}

impl Cluster {
    pub async fn start(n: usize, options: Setup) -> Self {
        let pki = Arc::new(Pki::new());
        let mut nodes = Vec::new();
        for i in 0..n {
            nodes.push(Node {
                id: MemberId(i as u64 + 1),
                disk: Disk {
                    log: Log::new(),
                    store: FaultyStore::new(Capabilities::default()),
                    fs: Counting::new(kbf_store::FaultFs::new()),
                    repair_fs: FaultFs::new(),
                },
                live: None,
                boot: 0,
            });
        }
        let mut cluster = Self {
            pki,
            nodes,
            generation: 0,
            options,
            maps: Arc::new(Cell::default()),
        };
        for i in 0..n {
            cluster.boot(i).await;
        }
        cluster.republish();
        cluster
    }

    /// Starts node `i` over its disk as it is now.
    async fn boot(&mut self, i: usize) {
        let options = self.options.clone();
        let node = &mut self.nodes[i];
        node.boot += 1;
        let mortal = Mortal::new(node.disk.log.clone());
        let mut cache = Cache::open(
            mortal.clone(),
            node.disk.store.clone(),
            KeyPrefix::default(),
        )
        .await
        .expect("open")
        .with_segment_limit(SEGMENT)
        .with_ram_ack_bytes(options.budget)
        .with_durable_wait(options.durable_wait);
        if let Some(hook) = options.hook.clone() {
            cache = cache.with_drain_hook(hook);
        }
        let (cache, startup) = cache
            .with_write_pool(pool(1, 16))
            .attach_tier(node.disk.fs.clone(), TierOptions::new(GIB), SERVER, None)
            .await
            .expect("attach");
        // The start synced the `TIER` marker; the tests count the blobs' syncs.
        node.disk.fs.probe.syncs.store(0, Ordering::SeqCst);
        let cache = Arc::new(cache);

        let (server_tls, client_tls) = self.pki.identity("storage");
        let held = Arc::new(NodeMap {
            me: node.id,
            map: Mutex::new(None),
        });
        let repairs = Arc::new(
            Repairs::open(
                node.disk.repair_fs.clone(),
                Options::default(),
                Arc::new(NoLedger),
            )
            .expect("a journal"),
        );
        let chain = Arc::new(Chain::new(
            Arc::clone(&held) as Arc<dyn MapSource>,
            Peers::tls(client_tls),
            Arc::clone(&repairs),
            ChainConfig::default(),
        ));
        let storage = Arc::new(CacheStorage::new(
            Arc::clone(&cache),
            SegmentsConfig::DEFAULT,
        ));
        let dynamic: Arc<dyn Storage> = Arc::clone(&storage) as Arc<dyn Storage>;
        let worker = Arc::new(RepairWorker::new(
            Arc::clone(&dynamic),
            Arc::clone(&chain),
            Arc::clone(&repairs),
            RepairConfig::default(),
        ));
        let confirmer = worker.confirmer();
        let service = StorageService::new(
            dynamic,
            Admission::Certified,
            Config {
                write_bytes_in_flight: 64 << 20,
            },
        )
        .with_refetch_wait(Duration::from_secs(5))
        .with_chain(Arc::clone(&chain))
        .expect("a certified service")
        .with_confirmer(Arc::clone(&confirmer));
        let incoming = TcpIncoming::bind(SocketAddr::from(([127, 0, 0, 1], 0))).expect("bind");
        let target = incoming.local_addr().expect("address");
        let server = tokio::spawn(async move {
            let _ = Server::builder()
                .tls_config(server_tls)
                .expect("TLS config")
                .add_routes(service.routes())
                .serve_with_incoming(incoming)
                .await;
        });
        let switch = Switch::start(target).await;
        node.live = Some(Live {
            cache,
            storage,
            switch,
            held,
            chain,
            repairs,
            worker,
            confirmer,
            startup,
            mortal,
            server,
        });
    }

    /// The map of the cluster now: every running node at its address, active.
    fn map(&self) -> Arc<Map> {
        let members = self
            .nodes
            .iter()
            .map(|node| Member {
                id: node.id,
                addr: node
                    .live
                    .as_ref()
                    .map_or_else(|| "127.0.0.1:1".to_owned(), |live| live.addr().to_string()),
                zone: String::new(),
                weight: 1,
                state: MemberState::Active,
                boot: node.boot,
            })
            .collect();
        Arc::new(
            Map::new(
                self.generation,
                members,
                Rule::Rendezvous,
                self.options.r,
                1,
                Vec::new(),
            )
            .expect("a map"),
        )
    }

    /// A new map generation, learned by every running node and by the router's cell.
    pub fn republish(&mut self) {
        self.generation += 1;
        let map = self.map();
        for node in &self.nodes {
            if let Some(live) = &node.live {
                *live.held.map.lock().expect("lock") = Some(Arc::clone(&map));
            }
        }
        self.maps.set(MapView {
            map,
            previous: None,
            earlier: Vec::new(),
        });
    }

    /// Kills node `i`: its connections close, its process state is gone and its tier
    /// directory is what was synced. Whatever its writers were parked at is let go and
    /// fails.
    pub fn kill(&mut self, i: usize) {
        let node = &mut self.nodes[i];
        let live = node.live.take().expect("running");
        live.switch.cut();
        live.mortal.kill();
        // Every write after this instant fails, so a writer let go from the sync gate does
        // not reach the log.
        let crashed = node.disk.fs.inner.crash(0);
        node.disk.fs.probe.fail_syncs.store(true, Ordering::SeqCst);
        node.disk.fs.resume();
        node.disk.fs = Counting::new(crashed);
        node.disk.repair_fs = node.disk.repair_fs.crash(0);
        drop(live);
    }

    /// Starts node `i` again over what its disk kept, and tells everyone the new map.
    pub async fn restart(&mut self, i: usize) {
        self.boot(i).await;
        self.republish();
    }

    /// A router as a frontend holds one, over the cluster's maps.
    pub fn router(&self) -> Router {
        Router::new(
            Arc::clone(&self.maps) as Arc<dyn RouteMaps>,
            Arc::new(Dialer(Arc::clone(&self.pki))),
        )
    }

    /// A client of node `i` as the frontend, routed by the current generation.
    pub fn client(&self, i: usize) -> RemoteStorage {
        RemoteStorage::new(
            self.pki.channel(self.nodes[i].up().addr(), "frontend"),
            self.generation,
        )
    }

    /// The node that is `digest`'s primary under the current map.
    pub fn primary_of(&self, digest: &Digest) -> usize {
        self.rank_of(digest, 0)
    }

    /// The node that is `digest`'s owner of rank `rank` (0 is the primary).
    pub fn rank_of(&self, digest: &Digest, rank: usize) -> usize {
        let owners = self.map().write_owners(digest);
        self.nodes
            .iter()
            .position(|n| n.id == owners[rank])
            .expect("a node")
    }

    /// A blob of `len` bytes whose primary is node `primary`, `salt` making it another
    /// blob.
    pub fn blob(&self, primary: usize, len: usize, salt: u64) -> TestBlob {
        (0..10_000u64)
            .map(|round| TestBlob::new(salt * 10_000 + round, len))
            .find(|blob| self.primary_of(&blob.digest) == primary)
            .expect("a blob with that primary")
    }

    /// A blob whose primary is node `primary` and whose replica is node `replica`.
    pub fn pair_blob(&self, primary: usize, replica: usize, len: usize, salt: u64) -> TestBlob {
        (0..10_000u64)
            .map(|round| TestBlob::new(salt * 10_000 + round, len))
            .find(|blob| {
                self.primary_of(&blob.digest) == primary && self.rank_of(&blob.digest, 1) == replica
            })
            .expect("a blob with that placement")
    }

    /// Writes `blob` to node `to` as a frontend does: acknowledged when the node answers.
    pub async fn write(&self, to: usize, blob: &TestBlob) -> Result<Digest, StorageError> {
        self.client(to).put(blob.bytes.clone()).await
    }

    /// Whether node `i`'s index has `digest` (staged on its tier or in its store).
    pub fn indexed(&self, i: usize, digest: &Digest) -> bool {
        !matches!(
            self.nodes[i].disk.log.answer(digest),
            kbf_meta::BlobAnswer::Absent
        )
    }
}

/// The map the router routes by.
#[derive(Default)]
struct Cell(Mutex<Option<MapView>>);

impl Cell {
    fn set(&self, view: MapView) {
        *self.0.lock().expect("lock") = Some(view);
    }
}

impl RouteMaps for Cell {
    fn current(&self) -> Option<MapView> {
        self.0.lock().expect("lock").clone()
    }

    fn refetch(&self, at_least: u64) -> BoxFuture<'_, Result<MapView, RefetchError>> {
        Box::pin(async move {
            self.current()
                .filter(|view| view.map.generation() >= at_least)
                .ok_or(RefetchError {
                    at_least,
                    reason: "no newer map in the test".to_owned(),
                })
        })
    }
}

/// Dials a member as the frontend.
struct Dialer(Arc<Pki>);

impl Nodes for Dialer {
    fn node(&self, member: &Member, generation: u64) -> Result<Arc<dyn Storage>, StorageError> {
        let addr: SocketAddr = member.addr.parse().expect("an address");
        Ok(Arc::new(RemoteStorage::new(
            self.0.channel(addr, "frontend"),
            generation,
        )))
    }
}

/// Awaits `event`, failing the test instead of hanging if it never comes. The bound only
/// ends a hang; no test asserts how long an event takes.
pub async fn within<T>(what: &str, event: impl std::future::Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(20), event)
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {what}"))
}

/// Blocks (off the runtime) until `n` syncs are parked at node `i`'s sync gate.
pub async fn parked_in_sync(cluster: &Cluster, i: usize, n: usize) {
    let fs = cluster.nodes[i].disk.fs.clone();
    within(
        "writers to park in sync",
        tokio::task::spawn_blocking(move || fs.wait_waiting(n)),
    )
    .await
    .expect("join");
}

/// The value of the sample `name` (with its labels, if any) in node `i`'s scrape.
pub fn metric(cluster: &Cluster, i: usize, name: &str) -> f64 {
    let text = cluster.nodes[i].cache().render_metrics();
    let key = format!("{name} ");
    text.lines()
        .find_map(|l| l.strip_prefix(&key))
        .unwrap_or_else(|| panic!("no sample {name}"))
        .trim()
        .parse()
        .expect("a number")
}

/// The bytes of a blob node `i` serves, if it does.
pub async fn read_from(
    cluster: &Cluster,
    i: usize,
    digest: &Digest,
) -> Result<Bytes, StorageError> {
    cluster.client(i).read_blob(digest).await
}
