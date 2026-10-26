//! Fixtures: an object store and a pack index that fail when told to and record what they
//! are asked, a storage node, and the frontend in front of them with packs.

// Each test file uses some of the fixtures.
#![allow(dead_code)]

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU32, Ordering::SeqCst};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, SystemTime};

use bytes::Bytes;
use kbf_auth::Authorizers;
use kbf_front::{Cache, CacheStorage, Owner, Owners, PackSettings, Packs, Proxy, SegmentsConfig};
use kbf_objstore::{
    ByteRange, Capabilities, KeyPrefix, ListPage, ListToken, MemoryStore, ObjectKey, ObjectStore,
    ObjectStoreError, PageSize,
};
use kbf_pack::{PackKeys, WriterKind};
use kbf_proto::reapi::content_addressable_storage_client::ContentAddressableStorageClient;
use kbf_proto::reapi::{
    self, BatchReadBlobsRequest, BatchUpdateBlobsRequest, FindMissingBlobsRequest,
    batch_update_blobs_request,
};
use kbf_state::pack_index::memory::MemoryPackIndex;
use kbf_state::{
    BlobDigest, Location, ManualClock, NewPack, PackIndex, PackIndexError, PackInfo, Recorded,
};
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
    ObjectStoreError::Service {
        status: 503,
        code: "SlowDown".to_owned(),
        message: "injected".to_owned(),
    }
}

/// A memory object store that claims what it is told to, whose writes and range reads fail when
/// armed to, and whose range reads are logged.
pub struct Spy {
    inner: MemoryStore,
    claims: Capabilities,
    reject_puts: AtomicU32,
    reject_reads: AtomicU32,
    puts: AtomicU32,
    ranges: Mutex<Vec<(String, u64, u64)>>,
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
            puts: AtomicU32::new(0),
            ranges: Mutex::new(Vec::new()),
        }
    }

    /// The next `n` writes fail before they store anything.
    pub fn reject_puts(&self, n: u32) {
        self.reject_puts.store(n, SeqCst);
    }

    /// The next `n` range reads fail (after they are logged).
    pub fn reject_reads(&self, n: u32) {
        self.reject_reads.store(n, SeqCst);
    }

    /// How many writes were tried.
    pub fn puts(&self) -> u32 {
        self.puts.load(SeqCst)
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
        if fires(&self.reject_reads) {
            return Err(injected());
        }
        self.inner.get_range(key, range).await
    }

    async fn may_hold(&self, key: &ObjectKey) -> Result<bool, ObjectStoreError> {
        self.inner.may_hold(key).await
    }

    async fn delete(&self, key: &ObjectKey) -> Result<(), ObjectStoreError> {
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
        }
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
        self.inner.unreferenced_keys(keys)
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
        let store = Arc::new(Spy::new());
        let index = Arc::new(Index::over(Arc::clone(&store)));
        let shared: Arc<dyn PackIndex> = index.clone();
        let keys = PackKeys::new(WriterKind::Frontend, "fe1", "boot1").expect("keys");
        let packs = Packs::new(Arc::clone(&store), shared, keys, settings).expect("packs");
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
