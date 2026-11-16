//! Fixtures: the pack index with the action cache, that fails when told to, and the frontend
//! with no storage node in front of an object store.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering::SeqCst};
use std::time::Duration;

use bytes::Bytes;
use kbf_auth::Authorizers;
use kbf_front::{Backend, PackSettings, Stored};
use kbf_pack::{PackKeys, WriterKind};
use kbf_pack_index::action::{ActionEntry, ActionPut, ActionStore};
use kbf_pack_index::memory::MemoryPackIndex;
use kbf_pack_index::{
    BlobDigest, Location, NewPack, PackIndex, PackIndexError, PackInfo, Recorded,
};
use kbf_state::ManualClock;
use kbf_types::Digest;
use tonic::transport::Channel;

use crate::harness;
use crate::support::Spy;

/// Counts an armed failure down; true while one is left.
fn fires(armed: &AtomicU32) -> bool {
    armed
        .try_update(SeqCst, SeqCst, |n| n.checked_sub(1))
        .is_ok()
}

/// A memory index and action cache whose calls fail when armed to.
pub struct Index {
    pub inner: MemoryPackIndex,
    fail_record: AtomicU32,
    fail_find: AtomicU32,
    fail_locate: AtomicU32,
    fail_get_action: AtomicU32,
    fail_put_action: AtomicU32,
}

impl Index {
    pub fn new() -> Self {
        Self {
            inner: MemoryPackIndex::new(Arc::new(ManualClock::default())),
            fail_record: AtomicU32::new(0),
            fail_find: AtomicU32::new(0),
            fail_locate: AtomicU32::new(0),
            fail_get_action: AtomicU32::new(0),
            fail_put_action: AtomicU32::new(0),
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

    /// The next `n` reads of an action row fail.
    pub fn fail_get_action(&self, n: u32) {
        self.fail_get_action.store(n, SeqCst);
    }

    /// The next `n` writes of an action row fail.
    pub fn fail_put_action(&self, n: u32) {
        self.fail_put_action.store(n, SeqCst);
    }

    /// Where the index has each of `digests`, in order.
    pub fn where_is(&self, digests: &[Digest]) -> Vec<Option<Location>> {
        let asked: Vec<BlobDigest> = digests.iter().map(index_form).collect();
        self.inner.locate(&asked).expect("located")
    }
}

impl PackIndex for Index {
    fn record_pack(&self, pack: &NewPack) -> Result<Recorded, PackIndexError> {
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

    fn drop_location(&self, digest: &BlobDigest, pack_key: &str) -> Result<bool, PackIndexError> {
        self.inner.drop_location(digest, pack_key)
    }

    fn unreferenced_keys(&self, keys: &[String]) -> Result<Vec<String>, PackIndexError> {
        self.inner.unreferenced_keys(keys)
    }
}

impl ActionStore for Index {
    fn get_action(&self, action: &BlobDigest) -> Result<Option<ActionEntry>, PackIndexError> {
        if fires(&self.fail_get_action) {
            return Err(PackIndexError::NotMigrated);
        }
        self.inner.get_action(action)
    }

    fn put_action(
        &self,
        action: &BlobDigest,
        result: &[u8],
        packs: &[NewPack],
    ) -> Result<ActionPut, PackIndexError> {
        if fires(&self.fail_put_action) {
            return Err(PackIndexError::NotMigrated);
        }
        self.inner.put_action(action, result, packs)
    }
}

/// The index's form of a digest.
pub fn index_form(digest: &Digest) -> BlobDigest {
    BlobDigest {
        hash: digest.hash,
        size: digest.size_bytes,
    }
}

/// The most a blob may be and still be packed in these tests: a blob of more is a large one.
pub const SMALL: u64 = 1024;

/// A frontend with no storage node.
pub struct Rig {
    pub store: Arc<Spy>,
    pub index: Arc<Index>,
    pub stored: Arc<Stored>,
    pub backend: Backend,
    /// The frontend, as a client sees it.
    pub front: Channel,
}

impl Rig {
    /// Packs that take blobs of at most [`SMALL`] bytes.
    pub async fn new() -> Self {
        let settings = PackSettings {
            max_blob_bytes: SMALL,
            ..PackSettings::default()
        };
        Self::with(settings).await
    }

    /// Packs as `settings` say, over a store that claims and keeps create-only writes.
    pub async fn with(settings: PackSettings) -> Self {
        Self::over(Spy::new(), settings, Authorizers::allow_all()).await
    }

    /// Packs as `settings` say, over `store`, every call authorized by `authorizers`.
    pub async fn over(store: Spy, settings: PackSettings, authorizers: Authorizers) -> Self {
        let store = Arc::new(store);
        let index = Arc::new(Index::new());
        let keys = PackKeys::new(WriterKind::Frontend, "fe1", "boot1").expect("keys");
        let actions: Arc<dyn ActionStore> = index.clone();
        let stored = Stored::new(Arc::clone(&store), actions, keys, settings).expect("stored");
        let stored = Arc::new(stored);
        let front = harness::serve(stored.routes(&Arc::new(authorizers))).await;
        let backend = Backend::stored(Arc::clone(&stored));
        Self {
            store,
            index,
            stored,
            backend,
            front,
        }
    }
}

/// The blob of `bytes` and its digest.
pub fn blob_of(bytes: &[u8]) -> (Digest, Bytes) {
    let bytes = Bytes::copy_from_slice(bytes);
    (kbf_segments::sha256(&bytes), bytes)
}

/// A blob of `len` bytes that no other of a different `salt` or length shares.
pub fn blob_len(len: usize, salt: u8) -> (Digest, Bytes) {
    let bytes: Vec<u8> = (0..len)
        .map(|i| (i % 251) as u8 ^ salt.wrapping_mul(31))
        .collect();
    blob_of(&bytes)
}
