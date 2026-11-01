//! Fixtures: an object store and a pack index that fail when told to, and record what they
//! are asked.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, SystemTime};

use bytes::Bytes;
use kbf_objstore::{
    ByteRange, Capabilities, KeyPrefix, ListPage, ListToken, MemoryStore, ObjectKey, ObjectStore,
    ObjectStoreError, PageSize,
};
use kbf_pack::{Blob, PackKeys, Packed, Packer, PackerConfig, Reader, ReaderConfig, WriterKind};
use kbf_pack_index::memory::MemoryPackIndex;
use kbf_pack_index::{
    BlobDigest, Location, NewPack, PackIndex, PackIndexError, PackInfo, Recorded,
};
use kbf_state::ManualClock;

/// Counts an armed failure down; true while one is left.
fn fires(armed: &AtomicU32) -> bool {
    let down = |n: u32| n.checked_sub(1);
    armed
        .try_update(Ordering::SeqCst, Ordering::SeqCst, down)
        .is_ok()
}

fn injected() -> ObjectStoreError {
    ObjectStoreError::Service {
        status: 503,
        code: "SlowDown".to_owned(),
        message: "injected".to_owned(),
    }
}

fn note<T>(log: &Mutex<Vec<T>>, entry: T) {
    log.lock()
        .unwrap_or_else(PoisonError::into_inner)
        .push(entry);
}

fn entries<T: Clone>(log: &Mutex<Vec<T>>) -> Vec<T> {
    log.lock().unwrap_or_else(PoisonError::into_inner).clone()
}

/// A memory object store with create-only writes, whose calls fail when armed to and whose
/// reads are logged.
pub struct FaultyStore {
    inner: MemoryStore,
    pass_puts: AtomicU32,
    reject_puts: AtomicU32,
    lose_puts: AtomicU32,
    fail_gets: AtomicU32,
    fail_deletes: AtomicU32,
    deny_deletes: AtomicU32,
    delete_calls: AtomicU32,
    pass_gets: AtomicU32,
    pass_lists: AtomicU32,
    fail_lists: AtomicU32,
    vanishing: Mutex<Option<String>>,
    ranges: Mutex<Vec<(String, u64, u64)>>,
}

impl FaultyStore {
    pub fn empty() -> Self {
        Self::with(Capabilities {
            conditional_put: true,
            object_lock: false,
        })
    }

    /// An empty store that claims no create-only writes: a second write of a key replaces it.
    pub fn replacing() -> Self {
        Self::with(Capabilities::default())
    }

    fn with(capabilities: Capabilities) -> Self {
        Self {
            inner: MemoryStore::new(capabilities),
            pass_puts: AtomicU32::new(0),
            reject_puts: AtomicU32::new(0),
            lose_puts: AtomicU32::new(0),
            fail_gets: AtomicU32::new(0),
            fail_deletes: AtomicU32::new(0),
            deny_deletes: AtomicU32::new(0),
            delete_calls: AtomicU32::new(0),
            pass_gets: AtomicU32::new(0),
            pass_lists: AtomicU32::new(0),
            fail_lists: AtomicU32::new(0),
            vanishing: Mutex::new(None),
            ranges: Mutex::new(Vec::new()),
        }
    }

    /// The next `n` writes fail before they store anything.
    pub fn reject_puts(&self, n: u32) {
        self.reject_puts.store(n, Ordering::SeqCst);
    }

    /// After the next `ok` writes go through, the `n` after them fail before they store
    /// anything.
    pub fn reject_puts_after(&self, ok: u32, n: u32) {
        self.pass_puts.store(ok, Ordering::SeqCst);
        self.reject_puts.store(n, Ordering::SeqCst);
    }

    /// The next `n` writes store the object and then fail: the answer is lost.
    pub fn lose_puts(&self, n: u32) {
        self.lose_puts.store(n, Ordering::SeqCst);
    }

    /// The next `n` reads fail.
    pub fn fail_gets(&self, n: u32) {
        self.fail_gets.store(n, Ordering::SeqCst);
    }

    /// After the next `ok` reads go through, the `n` after them fail.
    pub fn fail_gets_after(&self, ok: u32, n: u32) {
        self.pass_gets.store(ok, Ordering::SeqCst);
        self.fail_gets.store(n, Ordering::SeqCst);
    }

    /// The next `n` deletes fail.
    pub fn fail_deletes(&self, n: u32) {
        self.fail_deletes.store(n, Ordering::SeqCst);
    }

    /// From now on every delete is answered `status` (401 or 403) AccessDenied.
    pub fn deny_deletes(&self, status: u32) {
        self.deny_deletes.store(status, Ordering::SeqCst);
    }

    /// The deletes asked of the store so far, refused ones included.
    pub fn deletes(&self) -> u32 {
        self.delete_calls.load(Ordering::SeqCst)
    }

    /// After the next `ok` listing calls go through, the one after them fails.
    pub fn fail_list_after(&self, ok: u32) {
        self.pass_lists.store(ok, Ordering::SeqCst);
        self.fail_lists.store(1, Ordering::SeqCst);
    }

    /// From now on `key` is deleted from the store just before it is read: it was listed, and
    /// is gone by the time its footer is asked for.
    pub fn vanish(&self, key: &str) {
        let mut doomed = self
            .vanishing
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        *doomed = Some(key.to_owned());
    }

    /// Every range read so far: the key, the offset and the length.
    pub fn ranges(&self) -> Vec<(String, u64, u64)> {
        entries(&self.ranges)
    }

    /// Whether the store holds `key`.
    pub async fn holds(&self, key: &str) -> bool {
        let key = ObjectKey::new(key).unwrap();
        self.inner.may_hold(&key).await.unwrap()
    }

    /// Stores `body` under `key` as another writer would.
    pub async fn seed(&self, key: &str, body: &'static [u8]) {
        let key = ObjectKey::new(key).unwrap();
        let body = Bytes::from_static(body);
        self.inner.put_new(&key, body, None).await.unwrap();
    }

    /// The keys under `prefix`.
    pub async fn keys_under(&self, prefix: &str) -> Vec<String> {
        let prefix = KeyPrefix::new(prefix).unwrap();
        let page = self.list(&prefix, None, PageSize::MAX).await.unwrap();
        let found = page.objects.into_iter();
        found.map(|o| o.key.to_string()).collect()
    }
}

impl ObjectStore for FaultyStore {
    fn capabilities(&self) -> Capabilities {
        self.inner.capabilities()
    }

    async fn put_new(
        &self,
        key: &ObjectKey,
        body: Bytes,
        retain_until: Option<SystemTime>,
    ) -> Result<(), ObjectStoreError> {
        let passes = fires(&self.pass_puts);
        if !passes && fires(&self.reject_puts) {
            return Err(injected());
        }
        let stored = self.inner.put_new(key, body, retain_until).await;
        if fires(&self.lose_puts) {
            return Err(injected());
        }
        stored
    }

    async fn get_range(
        &self,
        key: &ObjectKey,
        range: ByteRange,
    ) -> Result<Bytes, ObjectStoreError> {
        let asked = (key.to_string(), range.offset(), range.size());
        note(&self.ranges, asked);
        let passes = fires(&self.pass_gets);
        if !passes && fires(&self.fail_gets) {
            return Err(injected());
        }
        let doomed = self
            .vanishing
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        if doomed.as_deref() == Some(key.as_str()) {
            self.inner.delete(key).await?;
        }
        self.inner.get_range(key, range).await
    }

    async fn may_hold(&self, key: &ObjectKey) -> Result<bool, ObjectStoreError> {
        self.inner.may_hold(key).await
    }

    async fn delete(&self, key: &ObjectKey) -> Result<(), ObjectStoreError> {
        self.delete_calls.fetch_add(1, Ordering::SeqCst);
        let denied = self.deny_deletes.load(Ordering::SeqCst);
        if denied != 0 {
            return Err(ObjectStoreError::Service {
                status: u16::try_from(denied).unwrap(),
                code: "AccessDenied".to_owned(),
                message: "injected".to_owned(),
            });
        }
        if fires(&self.fail_deletes) {
            return Err(injected());
        }
        self.inner.delete(key).await
    }

    async fn list(
        &self,
        prefix: &KeyPrefix,
        after: Option<&ListToken>,
        max_keys: PageSize,
    ) -> Result<ListPage, ObjectStoreError> {
        let passes = fires(&self.pass_lists);
        if !passes && fires(&self.fail_lists) {
            return Err(injected());
        }
        self.inner.list(prefix, after, max_keys).await
    }
}

/// A memory pack index whose calls fail when armed to and are logged. It notes, at each
/// record, whether the object store holds the pack being recorded.
pub struct FlakyIndex {
    inner: MemoryPackIndex,
    store: Arc<FaultyStore>,
    fail_record: AtomicU32,
    refuse_record: AtomicU32,
    lose_record: AtomicU32,
    fail_find: AtomicU32,
    fail_locate: AtomicU32,
    fail_touch: AtomicU32,
    fail_unreferenced: AtomicU32,
    held: Mutex<Vec<bool>>,
    finds: Mutex<Vec<usize>>,
    locates: Mutex<Vec<usize>>,
    touches: Mutex<Vec<usize>>,
}

impl FlakyIndex {
    pub fn over(store: Arc<FaultyStore>) -> Self {
        Self {
            inner: MemoryPackIndex::new(Arc::new(ManualClock::default())),
            store,
            fail_record: AtomicU32::new(0),
            refuse_record: AtomicU32::new(0),
            lose_record: AtomicU32::new(0),
            fail_find: AtomicU32::new(0),
            fail_locate: AtomicU32::new(0),
            fail_touch: AtomicU32::new(0),
            fail_unreferenced: AtomicU32::new(0),
            held: Mutex::new(Vec::new()),
            finds: Mutex::new(Vec::new()),
            locates: Mutex::new(Vec::new()),
            touches: Mutex::new(Vec::new()),
        }
    }

    /// The next `n` records fail.
    pub fn fail_record(&self, n: u32) {
        self.fail_record.store(n, Ordering::SeqCst);
    }

    /// The next `n` records are refused: the index says a repeat would be refused too.
    pub fn refuse_record(&self, n: u32) {
        self.refuse_record.store(n, Ordering::SeqCst);
    }

    /// The next `n` records are made and then fail: the answer is lost.
    pub fn lose_record(&self, n: u32) {
        self.lose_record.store(n, Ordering::SeqCst);
    }

    /// The next `n` questions of which blobs are missing fail.
    pub fn fail_find(&self, n: u32) {
        self.fail_find.store(n, Ordering::SeqCst);
    }

    /// The next `n` lookups fail.
    pub fn fail_locate(&self, n: u32) {
        self.fail_locate.store(n, Ordering::SeqCst);
    }

    /// The next `n` touches fail.
    pub fn fail_touch(&self, n: u32) {
        self.fail_touch.store(n, Ordering::SeqCst);
    }

    /// The next `n` questions of which keys are unreferenced fail.
    pub fn fail_unreferenced(&self, n: u32) {
        self.fail_unreferenced.store(n, Ordering::SeqCst);
    }

    /// For each record, whether the store held the pack when it was recorded.
    pub fn held_at_record(&self) -> Vec<bool> {
        entries(&self.held)
    }

    /// The size of each batch of digests asked of `find_missing`.
    pub fn find_batches(&self) -> Vec<usize> {
        entries(&self.finds)
    }

    /// The size of each batch of digests asked of `locate`.
    pub fn locate_batches(&self) -> Vec<usize> {
        entries(&self.locates)
    }

    /// The size of each batch of digests asked of `touch`.
    pub fn touch_batches(&self) -> Vec<usize> {
        entries(&self.touches)
    }
}

impl PackIndex for FlakyIndex {
    fn record_pack(&self, pack: &NewPack) -> Result<Recorded, PackIndexError> {
        let held = ObjectKey::new(pack.key.clone()).is_ok_and(|key| {
            let asked = self.store.may_hold(&key);
            futures::executor::block_on(asked).unwrap_or(false)
        });
        note(&self.held, held);
        if fires(&self.refuse_record) {
            return Err(PackIndexError::Refused("scripted".to_owned()));
        }
        if fires(&self.fail_record) {
            return Err(PackIndexError::NotMigrated);
        }
        let recorded = self.inner.record_pack(pack);
        if fires(&self.lose_record) {
            return Err(PackIndexError::NotMigrated);
        }
        recorded
    }

    fn pack(&self, key: &str) -> Result<Option<PackInfo>, PackIndexError> {
        self.inner.pack(key)
    }

    fn locate(&self, digests: &[BlobDigest]) -> Result<Vec<Option<Location>>, PackIndexError> {
        note(&self.locates, digests.len());
        if fires(&self.fail_locate) {
            return Err(PackIndexError::NotMigrated);
        }
        self.inner.locate(digests)
    }

    fn find_missing(&self, digests: &[BlobDigest]) -> Result<Vec<BlobDigest>, PackIndexError> {
        note(&self.finds, digests.len());
        if fires(&self.fail_find) {
            return Err(PackIndexError::NotMigrated);
        }
        self.inner.find_missing(digests)
    }

    fn touch(&self, digests: &[BlobDigest]) -> Result<usize, PackIndexError> {
        note(&self.touches, digests.len());
        if fires(&self.fail_touch) {
            return Err(PackIndexError::NotMigrated);
        }
        self.inner.touch(digests)
    }

    fn dead_packs(&self, grace: Duration, limit: usize) -> Result<Vec<PackInfo>, PackIndexError> {
        self.inner.dead_packs(grace, limit)
    }

    fn unreferenced_keys(&self, keys: &[String]) -> Result<Vec<String>, PackIndexError> {
        if fires(&self.fail_unreferenced) {
            return Err(PackIndexError::NotMigrated);
        }
        self.inner.unreferenced_keys(keys)
    }
}

/// A store and an index, and the packers and readers over them.
pub struct Rig {
    pub store: Arc<FaultyStore>,
    pub index: Arc<FlakyIndex>,
}

impl Default for Rig {
    fn default() -> Self {
        Self::new()
    }
}

impl Rig {
    pub fn new() -> Self {
        let store = Arc::new(FaultyStore::empty());
        let index = Arc::new(FlakyIndex::over(Arc::clone(&store)));
        Self { store, index }
    }

    pub fn packer(&self, config: PackerConfig) -> Packer<FaultyStore> {
        let index: Arc<dyn PackIndex> = self.index.clone();
        let store = Arc::clone(&self.store);
        Packer::new(store, index, keys(), config).unwrap()
    }

    pub fn reader(
        &self,
        cache: Option<Arc<kbf_pack::ReadCache>>,
        config: ReaderConfig,
    ) -> Reader<FaultyStore> {
        let index: Arc<dyn PackIndex> = self.index.clone();
        Reader::new(Arc::clone(&self.store), index, cache, config)
    }
}

/// The key scheme of the packers of the tests: node `node1`, boot `boot1`.
pub fn keys() -> PackKeys {
    PackKeys::new(WriterKind::Daemon, "node1", "boot1").unwrap()
}

/// The key of the `seq`th pack of the packers of the tests.
pub fn pack_key(seq: u64) -> String {
    keys().key(seq).to_string()
}

/// The default config with these limits.
pub fn limits(pack_bytes: u64, max_blobs: usize) -> PackerConfig {
    PackerConfig {
        pack_bytes,
        max_blobs,
        ..PackerConfig::default()
    }
}

/// Packs `blobs`, which must go well.
pub async fn pack_all(packer: &Packer<FaultyStore>, blobs: &[Blob]) -> Packed {
    packer.pack(blobs.to_vec()).await.unwrap()
}

/// A blob of the text's bytes, with its true digest.
pub fn blob(text: &str) -> Blob {
    let bytes = Bytes::copy_from_slice(text.as_bytes());
    Blob {
        digest: kbf_pack::blob_digest(&bytes),
        bytes,
    }
}

/// A blob of `len` bytes all `fill`, with its true digest.
pub fn filler(len: usize, fill: u8) -> Blob {
    let bytes = Bytes::from(vec![fill; len]);
    Blob {
        digest: kbf_pack::blob_digest(&bytes),
        bytes,
    }
}

/// The digests of `blobs`.
pub fn digests(blobs: &[Blob]) -> Vec<BlobDigest> {
    blobs.iter().map(|blob| blob.digest).collect()
}

/// A cache directory of its own that is removed when the test ends.
pub fn cache_dir(name: &str) -> kbf_testkit::dir::TestDir {
    kbf_testkit::dir::TestDir::under(env!("CARGO_TARGET_TMPDIR"), name)
}
