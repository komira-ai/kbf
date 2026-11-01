//! Fixtures: an object store and a pack index that fail when told to and record what they
//! are asked, a storage node, and the frontend in front of them with packs.

// Each test file uses some of the fixtures.
#![allow(dead_code)]

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering::SeqCst};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, SystemTime};

use bytes::{Bytes, BytesMut};
use futures::StreamExt as _;
use kbf_auth::Authorizers;
use kbf_front::{
    Cache, CacheStorage, Owner, Owners, PackSettings, Packs, Proxy, SegmentsConfig, VerifyingKeys,
};
use kbf_objstore::{
    BodyStream, ByteRange, Capabilities, KeyPrefix, ListPage, ListToken, MemoryStore, ObjectKey,
    ObjectStore, ObjectStoreError, PageSize,
};
use kbf_pack::{PackKeys, WriterKind};
use kbf_pack_index::memory::MemoryPackIndex;
use kbf_pack_index::{
    BlobDigest, Location, NewPack, PackIndex, PackIndexError, PackInfo, Recorded,
};
use kbf_proto::reapi::content_addressable_storage_client::ContentAddressableStorageClient;
use kbf_proto::reapi::{
    self, BatchReadBlobsRequest, BatchUpdateBlobsRequest, FindMissingBlobsRequest,
    batch_update_blobs_request,
};
use kbf_state::ManualClock;
use kbf_storage::Storage;
use kbf_storage::service::{Admission, Config, StorageService};
use kbf_types::Digest;
use tonic::service::Routes;
use tonic::transport::server::TcpIncoming;
use tonic::transport::{Channel, Endpoint, Server};

use crate::harness;

/// Counts an armed failure down; true while one is left.
fn fires(armed: &AtomicU32) -> bool {
    let down = |n: u32| n.checked_sub(1);
    armed.try_update(SeqCst, SeqCst, down).is_ok()
}

fn injected() -> ObjectStoreError {
    injected_with(503)
}

fn injected_with(status: u32) -> ObjectStoreError {
    let code = if status == 507 {
        "InsufficientStorage"
    } else {
        "SlowDown"
    };
    ObjectStoreError::Service {
        status: u16::try_from(status).expect("a status"),
        code: code.to_owned(),
        message: "injected".to_owned(),
    }
}

/// What a test runs inside a store call, given the key the call is for.
pub type Hook = Arc<dyn Fn(String) -> futures::future::BoxFuture<'static, ()> + Send + Sync>;

/// A memory object store that claims what it is told to, whose writes and range reads fail when
/// armed to, and whose range reads are logged.
pub struct Spy {
    inner: MemoryStore,
    claims: Capabilities,
    reject_puts: AtomicU32,
    reject_reads: AtomicU32,
    reject_lens: AtomicU32,
    /// The range reads so far, and the number of the one that fails (`u64::MAX`: none).
    reads: AtomicU64,
    fail_read_at: AtomicU64,
    shave_len: AtomicU64,
    reject_big_reads: AtomicU32,
    reject_large_lens: AtomicU32,
    overstated: Mutex<Vec<(String, u64)>>,
    hidden: Mutex<Vec<String>>,
    failing_len: Mutex<Vec<String>>,
    put_status: AtomicU32,
    stall_streams: AtomicBool,
    puts: AtomicU32,
    stream_puts: AtomicU32,
    largest_piece: AtomicU64,
    ranges: Mutex<Vec<(String, u64, u64)>>,
    watching: Mutex<Option<(Arc<VerifyingKeys>, Vec<String>)>>,
    on_len: Mutex<Option<Hook>>,
    on_delete: Mutex<Option<Hook>>,
    held_when_asked: Mutex<Vec<bool>>,
}

impl Spy {
    /// A store with create-only writes.
    pub fn new() -> Self {
        let create_only = Capabilities {
            conditional_put: true,
            object_lock: false,
        };
        Self::with(create_only, create_only)
    }

    /// A store that claims create-only writes and replaces an object it holds.
    pub fn replacing() -> Self {
        let claims = Capabilities {
            conditional_put: true,
            object_lock: false,
        };
        Self::with(claims, Capabilities::default())
    }

    /// A store that does not claim create-only writes.
    pub fn without_create_only() -> Self {
        Self::with(Capabilities::default(), Capabilities::default())
    }

    fn with(claims: Capabilities, behaves: Capabilities) -> Self {
        Self {
            inner: MemoryStore::new(behaves),
            claims,
            reject_puts: AtomicU32::new(0),
            reject_reads: AtomicU32::new(0),
            reject_lens: AtomicU32::new(0),
            reads: AtomicU64::new(0),
            fail_read_at: AtomicU64::new(u64::MAX),
            shave_len: AtomicU64::new(0),
            reject_big_reads: AtomicU32::new(0),
            reject_large_lens: AtomicU32::new(0),
            overstated: Mutex::new(Vec::new()),
            hidden: Mutex::new(Vec::new()),
            failing_len: Mutex::new(Vec::new()),
            put_status: AtomicU32::new(503),
            stall_streams: AtomicBool::new(false),
            puts: AtomicU32::new(0),
            stream_puts: AtomicU32::new(0),
            largest_piece: AtomicU64::new(0),
            ranges: Mutex::new(Vec::new()),
            watching: Mutex::new(None),
            on_len: Mutex::new(None),
            on_delete: Mutex::new(None),
            held_when_asked: Mutex::new(Vec::new()),
        }
    }

    /// From now on, each length asked of the store notes whether every one of `keys` is held by
    /// `registry` at that moment.
    pub fn watch_verifying(&self, registry: Arc<VerifyingKeys>, keys: Vec<String>) {
        *self.watching.lock().unwrap_or_else(PoisonError::into_inner) = Some((registry, keys));
    }

    /// Runs `hook` inside every length asked of the store, before it answers.
    pub fn on_len(&self, hook: Hook) {
        *self.on_len.lock().unwrap_or_else(PoisonError::into_inner) = Some(hook);
    }

    /// Runs `hook` inside every delete asked of the store, before it is made.
    pub fn on_delete(&self, hook: Hook) {
        *self
            .on_delete
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(hook);
    }

    /// What each length asked since `watch_verifying` found: whether all the keys were held.
    pub fn held_when_asked(&self) -> Vec<bool> {
        self.held_when_asked
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// The next `n` writes fail before they store anything.
    pub fn reject_puts(&self, n: u32) {
        self.reject_puts.store(n, SeqCst);
    }

    /// The next `n` range reads fail (after they are logged).
    pub fn reject_reads(&self, n: u32) {
        self.reject_reads.store(n, SeqCst);
    }

    /// The range read after the next `skip` ones fails, and only that one (the first read is
    /// number 0 of those after this call).
    pub fn reject_read_after(&self, skip: u64) {
        let now = self.reads.load(SeqCst);
        self.fail_read_at.store(now + skip, SeqCst);
    }

    /// The next `n` length questions fail.
    pub fn reject_lens(&self, n: u32) {
        self.reject_lens.store(n, SeqCst);
    }

    /// Every length answer is `by` bytes short of the object's: the store says an object is
    /// shorter than it is.
    pub fn understate_len(&self, by: u64) {
        self.shave_len.store(by, SeqCst);
    }

    /// The next `n` range reads of a MiB or more fail (after they are logged): a read of a large
    /// object's window, not of a footer or a record.
    pub fn reject_big_reads(&self, n: u32) {
        self.reject_big_reads.store(n, SeqCst);
    }

    /// The next `n` length questions about a large object (a key of slot `L<i>`) fail.
    pub fn reject_large_lens(&self, n: u32) {
        self.reject_large_lens.store(n, SeqCst);
    }

    /// The length answer for `key` is `by` bytes more than the object's: the store says an
    /// object is longer than it is.
    pub fn overstate_len_of(&self, key: &str, by: u64) {
        let mut overstated = self
            .overstated
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        overstated.push((key.to_owned(), by));
    }

    /// The length question about `key` is answered as if there were no object (a write of the key
    /// is still refused when it is held).
    pub fn hide_len_of(&self, key: &str) {
        let mut hidden = self.hidden.lock().unwrap_or_else(PoisonError::into_inner);
        hidden.push(key.to_owned());
    }

    /// The length question about `key` fails (every time it is asked).
    pub fn fail_len_of(&self, key: &str) {
        let mut failing = self
            .failing_len
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        failing.push(key.to_owned());
    }

    /// The next `n` writes fail with the HTTP `status` (503 and 507 are the answers of a store
    /// that is slow and one that is full).
    pub fn reject_puts_with(&self, n: u32, status: u32) {
        self.put_status.store(status, SeqCst);
        self.reject_puts.store(n, SeqCst);
    }

    /// A streamed write takes every byte of its body and then never finishes.
    pub fn stall_streams(&self) {
        self.stall_streams.store(true, SeqCst);
    }

    /// How many streamed writes were tried (not the writes the pack index makes).
    pub fn stream_puts(&self) -> u32 {
        self.stream_puts.load(SeqCst)
    }

    /// How many writes were tried.
    pub fn puts(&self) -> u32 {
        self.puts.load(SeqCst)
    }

    /// The most bytes any one piece of a streamed write's body has had so far: what a writer
    /// held at once to hand the store a piece.
    pub fn largest_piece(&self) -> u64 {
        self.largest_piece.load(SeqCst)
    }

    /// The bytes of the object under `key`, if the store holds one.
    pub async fn held(&self, key: &str) -> Option<Bytes> {
        let key = ObjectKey::new(key).expect("a key");
        let len = self.inner.len(&key).await.expect("asked")?;
        let range = ByteRange::new(0, len).expect("bytes");
        Some(self.inner.get_range(&key, range).await.expect("read"))
    }

    /// Every range read so far: the key, the offset and the length.
    pub fn ranges(&self) -> Vec<(String, u64, u64)> {
        self.ranges
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Stores `body` under `key` as another writer would.
    pub async fn seed(&self, key: &str, body: &[u8]) {
        let key = ObjectKey::new(key).expect("a key");
        let body = Bytes::copy_from_slice(body);
        self.inner.put_new(&key, body, None).await.expect("seeded");
    }

    /// The keys of the packs held.
    pub async fn keys(&self) -> Vec<String> {
        let prefix = KeyPrefix::new("cas/").expect("a prefix");
        let listing = self.list(&prefix, None, PageSize::MAX).await;
        let objects = listing.expect("listed").objects;
        objects.into_iter().map(|o| o.key.to_string()).collect()
    }
}

impl Default for Spy {
    fn default() -> Self {
        Self::new()
    }
}

impl ObjectStore for Spy {
    fn capabilities(&self) -> Capabilities {
        self.claims
    }

    async fn put_new(
        &self,
        key: &ObjectKey,
        body: Bytes,
        retain_until: Option<SystemTime>,
    ) -> Result<(), ObjectStoreError> {
        self.puts.fetch_add(1, SeqCst);
        if fires(&self.reject_puts) {
            return Err(injected());
        }
        self.inner.put_new(key, body, retain_until).await
    }

    async fn put_stream(
        &self,
        key: &ObjectKey,
        len: u64,
        mut body: BodyStream,
    ) -> Result<(), ObjectStoreError> {
        self.puts.fetch_add(1, SeqCst);
        self.stream_puts.fetch_add(1, SeqCst);
        if fires(&self.reject_puts) {
            return Err(injected_with(self.put_status.load(SeqCst)));
        }
        let mut out = BytesMut::new();
        while let Some(piece) = body.next().await {
            let piece = piece?;
            self.largest_piece.fetch_max(piece.len() as u64, SeqCst);
            if out.len() as u64 + piece.len() as u64 > len {
                return Err(ObjectStoreError::Body("more than its length".to_owned()));
            }
            out.extend_from_slice(&piece);
        }
        if (out.len() as u64) < len {
            return Err(ObjectStoreError::Body("shorter than its length".to_owned()));
        }
        if self.stall_streams.load(SeqCst) {
            std::future::pending::<()>().await;
        }
        self.inner.put_new(key, out.freeze(), None).await
    }

    async fn get_range(
        &self,
        key: &ObjectKey,
        range: ByteRange,
    ) -> Result<Bytes, ObjectStoreError> {
        let asked = (key.to_string(), range.offset(), range.size());
        self.ranges
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(asked);
        let number = self.reads.fetch_add(1, SeqCst);
        if fires(&self.reject_reads) || number == self.fail_read_at.load(SeqCst) {
            return Err(injected());
        }
        if range.size() >= 1 << 20 && fires(&self.reject_big_reads) {
            return Err(injected());
        }
        self.inner.get_range(key, range).await
    }

    async fn may_hold(&self, key: &ObjectKey) -> Result<bool, ObjectStoreError> {
        self.inner.may_hold(key).await
    }

    async fn len(&self, key: &ObjectKey) -> Result<Option<u64>, ObjectStoreError> {
        let hook = self
            .on_len
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        if let Some(hook) = hook {
            hook(key.to_string()).await;
        }
        let watching = self
            .watching
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        if let Some((registry, keys)) = watching {
            let all = keys.iter().all(|key| registry.is_held(key));
            self.held_when_asked
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(all);
        }
        if fires(&self.reject_lens) {
            return Err(injected());
        }
        let failing = {
            let failing = self
                .failing_len
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            failing.iter().any(|named| named == key.as_str())
        };
        if failing {
            return Err(injected());
        }
        let large = key.as_str().rsplit('-').next();
        if large.is_some_and(|slot| slot.starts_with('L')) && fires(&self.reject_large_lens) {
            return Err(injected());
        }
        let hidden = {
            let hidden = self.hidden.lock().unwrap_or_else(PoisonError::into_inner);
            hidden.iter().any(|named| named == key.as_str())
        };
        if hidden {
            return Ok(None);
        }
        let shave = self.shave_len.load(SeqCst);
        let len = self.inner.len(key).await?;
        let by: u64 = {
            let overstated = self
                .overstated
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            overstated
                .iter()
                .filter(|(named, _)| named == key.as_str())
                .map(|(_, by)| by)
                .sum()
        };
        Ok(len.map(|len| len.saturating_sub(shave) + by))
    }

    async fn delete(&self, key: &ObjectKey) -> Result<(), ObjectStoreError> {
        let hook = self
            .on_delete
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        if let Some(hook) = hook {
            hook(key.to_string()).await;
        }
        self.inner.delete(key).await
    }

    async fn list(
        &self,
        prefix: &KeyPrefix,
        after: Option<&ListToken>,
        max_keys: PageSize,
    ) -> Result<ListPage, ObjectStoreError> {
        self.inner.list(prefix, after, max_keys).await
    }
}

/// A memory pack index whose calls fail when armed to. It notes, at each record, whether the
/// object store holds the pack being recorded.
pub struct Index {
    inner: MemoryPackIndex,
    store: Arc<Spy>,
    fail_record: AtomicU32,
    fail_find: AtomicU32,
    fail_locate: AtomicU32,
    held: Mutex<Vec<bool>>,
    after_unreferenced: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
}

impl Index {
    pub fn over(store: Arc<Spy>) -> Self {
        Self {
            inner: MemoryPackIndex::new(Arc::new(ManualClock::default())),
            store,
            fail_record: AtomicU32::new(0),
            fail_find: AtomicU32::new(0),
            fail_locate: AtomicU32::new(0),
            held: Mutex::new(Vec::new()),
            after_unreferenced: Mutex::new(None),
        }
    }

    /// Runs `hook` after every answer to which keys are unreferenced, before it is returned.
    pub fn after_unreferenced(&self, hook: Arc<dyn Fn() + Send + Sync>) {
        *self
            .after_unreferenced
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(hook);
    }

    /// The next `n` records fail.
    pub fn fail_record(&self, n: u32) {
        self.fail_record.store(n, SeqCst);
    }

    /// The next `n` questions of which blobs are missing fail.
    pub fn fail_find(&self, n: u32) {
        self.fail_find.store(n, SeqCst);
    }

    /// The next `n` lookups fail.
    pub fn fail_locate(&self, n: u32) {
        self.fail_locate.store(n, SeqCst);
    }

    /// For each record, whether the store held the pack when it was recorded.
    pub fn held_at_record(&self) -> Vec<bool> {
        self.held
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Where the index has each of `digests`, in order.
    pub fn where_is(&self, digests: &[Digest]) -> Vec<Option<Location>> {
        let asked: Vec<BlobDigest> = digests.iter().map(index_form).collect();
        self.inner.locate(&asked).expect("located")
    }

    /// Whether the index has a row for every one of `digests`.
    pub fn has_all(&self, digests: &[Digest]) -> bool {
        self.where_is(digests).iter().all(Option::is_some)
    }

    /// Whether the index has a row for none of `digests`.
    pub fn has_none(&self, digests: &[Digest]) -> bool {
        self.where_is(digests).iter().all(Option::is_none)
    }
}

impl PackIndex for Index {
    fn record_pack(&self, pack: &NewPack) -> Result<Recorded, PackIndexError> {
        let held = ObjectKey::new(pack.key.clone()).is_ok_and(|key| {
            let asked = self.store.may_hold(&key);
            futures::executor::block_on(asked).unwrap_or(false)
        });
        self.held
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(held);
        if fires(&self.fail_record) {
            return Err(PackIndexError::NotMigrated);
        }
        self.inner.record_pack(pack)
    }

    fn pack(&self, key: &str) -> Result<Option<PackInfo>, PackIndexError> {
        self.inner.pack(key)
    }

    fn locate(&self, digests: &[BlobDigest]) -> Result<Vec<Option<Location>>, PackIndexError> {
        if fires(&self.fail_locate) {
            return Err(PackIndexError::NotMigrated);
        }
        self.inner.locate(digests)
    }

    fn find_missing(&self, digests: &[BlobDigest]) -> Result<Vec<BlobDigest>, PackIndexError> {
        if fires(&self.fail_find) {
            return Err(PackIndexError::NotMigrated);
        }
        self.inner.find_missing(digests)
    }

    fn touch(&self, digests: &[BlobDigest]) -> Result<usize, PackIndexError> {
        self.inner.touch(digests)
    }

    fn dead_packs(&self, grace: Duration, limit: usize) -> Result<Vec<PackInfo>, PackIndexError> {
        self.inner.dead_packs(grace, limit)
    }

    fn unreferenced_keys(&self, keys: &[String]) -> Result<Vec<String>, PackIndexError> {
        let answer = self.inner.unreferenced_keys(keys);
        let hook = self
            .after_unreferenced
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        if let Some(hook) = hook {
            hook();
        }
        answer
    }
}

/// The index's form of a digest.
pub fn index_form(digest: &Digest) -> BlobDigest {
    BlobDigest {
        hash: digest.hash,
        size: digest.size_bytes,
    }
}

/// A blob of the text's bytes, with its digest.
pub fn blob(text: &str) -> (Digest, Bytes) {
    let bytes = Bytes::copy_from_slice(text.as_bytes());
    (kbf_segments::sha256(&bytes), bytes)
}

/// Serves `routes` on a loopback port.
fn serve(routes: Routes) -> SocketAddr {
    let incoming = TcpIncoming::bind(([127, 0, 0, 1], 0).into()).expect("bind");
    let addr = incoming.local_addr().expect("address");
    tokio::spawn(
        Server::builder()
            .add_routes(routes)
            .serve_with_incoming(incoming),
    );
    addr
}

fn lazy(addr: SocketAddr) -> Channel {
    Endpoint::from_shared(format!("http://{addr}"))
        .expect("endpoint")
        .connect_lazy()
}

/// A storage node over an empty in-memory cache, and its address.
pub fn node() -> (Arc<dyn Storage>, SocketAddr) {
    let store: Arc<dyn Storage> = Arc::new(CacheStorage::new(
        Arc::new(Cache::memory()),
        SegmentsConfig::DEFAULT,
    ));
    let config = Config {
        write_bytes_in_flight: 64 << 20,
    };
    let service = StorageService::new(Arc::clone(&store), Admission::Open, config);
    (store, serve(service.routes()))
}

/// The frontend with packs over a pack store and an index, in front of storage nodes.
pub struct Rig {
    pub store: Arc<Spy>,
    pub index: Arc<Index>,
    pub packs: Packs,
    /// The storage node behind the frontend, when it is a real one.
    pub node: Option<Arc<dyn Storage>>,
    /// The frontend, as a client sees it.
    pub front: Channel,
}

impl Rig {
    /// Packs of the default settings in front of a real storage node.
    pub async fn new() -> Self {
        Self::with(PackSettings::default()).await
    }

    /// Packs as `settings` say, in front of a real storage node.
    pub async fn with(settings: PackSettings) -> Self {
        let (node, addr) = node();
        let owners = Owners::single(Owner::new("node", lazy(addr)));
        let mut rig = Self::over(owners, settings).await;
        rig.node = Some(node);
        rig
    }

    /// Packs as `settings` say, in front of `owners`.
    pub async fn over(owners: Owners, settings: PackSettings) -> Self {
        Self::over_store(owners, settings, Spy::new()).await
    }

    /// Packs of the default settings over a store that replaces an object it holds, in front of
    /// a real storage node.
    pub async fn replacing() -> Self {
        let (node, addr) = node();
        let owners = Owners::single(Owner::new("node", lazy(addr)));
        let mut rig = Self::over_store(owners, PackSettings::default(), Spy::replacing()).await;
        rig.node = Some(node);
        rig
    }

    /// Packs as `settings` say, over `store`, in front of `owners`.
    async fn over_store(owners: Owners, settings: PackSettings, store: Spy) -> Self {
        let store = Arc::new(store);
        let index = Arc::new(Index::over(Arc::clone(&store)));
        let shared: Arc<dyn PackIndex> = index.clone();
        let keys = PackKeys::new(WriterKind::Frontend, "fe1", "boot1").expect("keys");
        let packs = Packs::new(Arc::clone(&store), shared, keys, settings).expect("packs");
        // The keys of these tests are minted at one fixed time, which the packs take as now.
        let packs = packs.with_clock(Arc::new(crate::untrusted_task::now));
        let authorizers = Arc::new(Authorizers::allow_all());
        let proxy = Proxy::new(owners, authorizers).with_packs(Some(packs.clone()));
        let front = harness::serve(proxy.routes()).await;
        Self {
            store,
            index,
            packs,
            node: None,
            front,
        }
    }

    /// The storage node behind the frontend.
    pub fn node(&self) -> &Arc<dyn Storage> {
        self.node.as_ref().expect("a real storage node")
    }

    /// Whether the storage node holds `digest`.
    pub async fn node_holds(&self, digest: &Digest) -> bool {
        let missing = self.node().find_missing(&[*digest]).await;
        missing.expect("the node answers").is_empty()
    }
}

/// The status code of each item of a BatchUpdateBlobs of these requests.
pub async fn send_update(
    front: &Channel,
    requests: Vec<batch_update_blobs_request::Request>,
) -> Vec<i32> {
    let response = ContentAddressableStorageClient::new(front.clone())
        .batch_update_blobs(BatchUpdateBlobsRequest {
            requests,
            ..BatchUpdateBlobsRequest::default()
        })
        .await
        .expect("the batch is answered")
        .into_inner();
    response
        .responses
        .iter()
        .map(|r| r.status.as_ref().expect("a status").code)
        .collect()
}

/// One item of a BatchUpdateBlobs: the blob with the digest it names.
pub fn item(digest: &Digest, bytes: &Bytes) -> batch_update_blobs_request::Request {
    batch_update_blobs_request::Request {
        digest: Some(kbf_front::digest_to_proto(digest)),
        data: bytes.to_vec(),
        compressor: reapi::compressor::Value::Identity as i32,
    }
}

/// The status code of each item of a BatchUpdateBlobs of `blobs`.
pub async fn update(front: &Channel, blobs: &[(Digest, Bytes)]) -> Vec<i32> {
    let requests = blobs.iter().map(|(d, b)| item(d, b)).collect();
    send_update(front, requests).await
}

/// The digests among `digests` that FindMissingBlobs reports missing.
pub async fn missing(front: &Channel, digests: &[Digest]) -> Result<Vec<Digest>, tonic::Status> {
    let asked = FindMissingBlobsRequest {
        blob_digests: digests.iter().map(kbf_front::digest_to_proto).collect(),
        ..FindMissingBlobsRequest::default()
    };
    let response = ContentAddressableStorageClient::new(front.clone())
        .find_missing_blobs(asked)
        .await?
        .into_inner();
    let digests = response.missing_blob_digests.iter();
    Ok(digests
        .map(|d| kbf_front::digest_from_proto(Some(d)).expect("a digest"))
        .collect())
}

/// The status code and bytes of each item of a BatchReadBlobs of `digests`.
pub async fn read(front: &Channel, digests: &[Digest]) -> Vec<(i32, Vec<u8>)> {
    let response = ContentAddressableStorageClient::new(front.clone())
        .batch_read_blobs(BatchReadBlobsRequest {
            digests: digests.iter().map(kbf_front::digest_to_proto).collect(),
            ..BatchReadBlobsRequest::default()
        })
        .await
        .expect("the batch is answered")
        .into_inner();
    response
        .responses
        .into_iter()
        .map(|r| (r.status.expect("a status").code, r.data))
        .collect()
}
