//! Fixtures: an in-memory store that counts its calls, a pack index that can be made to fail,
//! the credentials file the tool reads, trees to backfill, and runs of the tool over them.

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use bytes::Bytes;
use futures::future::BoxFuture;
use kbf_front::action_tree::Sleeper;
use kbf_front::large_backfill::LargeStore;
use kbf_l2_backfill::pace::Clock;
use kbf_l2_backfill::store::Bucket;
use kbf_l2_backfill::{Seams, main_with_seams};
use kbf_objstore::{
    BodyStream, Capabilities, MemoryStore, ObjectKey, ObjectStore as _, ObjectStoreError,
};
use kbf_pack_index::cells::{LivePack, ObjectCells, Unlisted};
use kbf_pack_index::memory::MemoryPackIndex;
use kbf_pack_index::{
    BlobDigest, Forgot, Location, NewPack, PackBlob, PackIndex, PackIndexError, PackInfo, Recorded,
};
use kbf_state::ManualClock;
use kbf_testkit::dir::TestDir;
use sha2::{Digest as _, Sha256};
use tierstore::{BlobSource, ListEntry, ListKind, Mode, TreeDigest, TreeList};

/// A URL that is parsed and never connected to.
pub const SOCKET_URL: &str = "postgres:///kbf?host=/run/postgresql";

/// A store address that is parsed and never connected to.
pub const ENDPOINT: &str = "http://127.0.0.1:1";

/// A window that holds every tree the tests record.
pub const EVER: Duration = Duration::from_secs(3650 * 86_400);

/// What a run of the tool printed and ended with.
pub struct Ran {
    pub code: u8,
    pub out: String,
    pub err: String,
}

fn locked<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The lowercase hex of the SHA-256 of `bytes`.
pub fn sha_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// A clock the tests move.
#[derive(Default)]
pub struct StepClock(AtomicU64);

impl StepClock {
    pub fn advance(&self, by: Duration) {
        self.0.fetch_add(by.as_nanos() as u64, Ordering::SeqCst);
    }
}

impl Clock for StepClock {
    fn now(&self) -> Duration {
        Duration::from_nanos(self.0.load(Ordering::SeqCst))
    }
}

/// A sleeper that records what it was asked to wait, and moves the clock by it.
pub struct Naps {
    pub waits: Mutex<Vec<Duration>>,
    pub clock: Arc<StepClock>,
}

impl Naps {
    pub fn waits(&self) -> Vec<Duration> {
        locked(&self.waits).clone()
    }
}

impl Sleeper for Naps {
    fn sleep(&self, wait: Duration) -> BoxFuture<'_, ()> {
        locked(&self.waits).push(wait);
        self.clock.advance(wait);
        Box::pin(std::future::ready(()))
    }
}

/// A store in memory that counts the calls it gets and can be made to fail them.
pub struct Probe {
    pub objects: Arc<MemoryStore>,
    inner: Bucket<MemoryStore>,
    pub lens: AtomicU64,
    pub reads: AtomicU64,
    pub puts: AtomicU64,
    pub fail_len: Mutex<BTreeSet<String>>,
    pub fail_read: Mutex<BTreeSet<String>>,
    /// The clock a `len` call moves, and by how much: the work a tree costs.
    pub work: Mutex<Option<(Arc<StepClock>, Duration)>>,
}

impl Probe {
    fn new() -> Self {
        let objects = Arc::new(MemoryStore::new(Capabilities::default()));
        Self {
            inner: Bucket::new(Arc::clone(&objects)),
            objects,
            lens: AtomicU64::new(0),
            reads: AtomicU64::new(0),
            puts: AtomicU64::new(0),
            fail_len: Mutex::default(),
            fail_read: Mutex::default(),
            work: Mutex::default(),
        }
    }

    pub async fn put(&self, key: &str, bytes: &[u8]) {
        let key = ObjectKey::new(key).expect("a key");
        self.objects
            .put_new(&key, Bytes::copy_from_slice(bytes), None)
            .await
            .expect("a put");
    }

    pub async fn delete(&self, key: &str) {
        let key = ObjectKey::new(key).expect("a key");
        self.objects.delete(&key).await.expect("a delete");
    }

    pub async fn holds(&self, key: &str) -> bool {
        let key = ObjectKey::new(key).expect("a key");
        self.objects.len(&key).await.expect("a len").is_some()
    }

    pub fn calls(&self) -> (u64, u64, u64) {
        (
            self.lens.load(Ordering::SeqCst),
            self.reads.load(Ordering::SeqCst),
            self.puts.load(Ordering::SeqCst),
        )
    }
}

fn injected() -> ObjectStoreError {
    ObjectStoreError::Service {
        status: 500,
        code: "InternalError".to_owned(),
        message: "injected".to_owned(),
    }
}

impl LargeStore for Probe {
    fn len<'a>(
        &'a self,
        key: &'a ObjectKey,
    ) -> BoxFuture<'a, Result<Option<u64>, ObjectStoreError>> {
        self.lens.fetch_add(1, Ordering::SeqCst);
        if let Some((clock, by)) = &*locked(&self.work) {
            clock.advance(*by);
        }
        if locked(&self.fail_len).contains(key.as_str()) {
            return Box::pin(std::future::ready(Err(injected())));
        }
        self.inner.len(key)
    }

    fn put<'a>(
        &'a self,
        key: &'a ObjectKey,
        len: u64,
        body: BodyStream,
    ) -> BoxFuture<'a, Result<(), ObjectStoreError>> {
        self.puts.fetch_add(1, Ordering::SeqCst);
        self.inner.put(key, len, body)
    }

    fn read_range<'a>(
        &'a self,
        key: &'a ObjectKey,
        offset: u64,
        len: u64,
    ) -> BoxFuture<'a, Result<Bytes, ObjectStoreError>> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        if locked(&self.fail_read).contains(key.as_str()) {
            return Box::pin(std::future::ready(Err(injected())));
        }
        self.inner.read_range(key, offset, len)
    }
}

/// A pack index over the in-memory one whose listing calls can be made to fail.
pub struct Faulty {
    pub inner: MemoryPackIndex,
    pub fail_unlisted: AtomicBool,
    pub fail_drop: AtomicBool,
    pub fail_live: AtomicBool,
}

fn down() -> PackIndexError {
    PackIndexError::Invalid("injected")
}

impl PackIndex for Faulty {
    fn record_pack(&self, pack: &NewPack) -> Result<Recorded, PackIndexError> {
        self.inner.record_pack(pack)
    }
    fn pack(&self, key: &str) -> Result<Option<PackInfo>, PackIndexError> {
        self.inner.pack(key)
    }
    fn locate(&self, digests: &[BlobDigest]) -> Result<Vec<Option<Location>>, PackIndexError> {
        self.inner.locate(digests)
    }
    fn find_missing(&self, digests: &[BlobDigest]) -> Result<Vec<BlobDigest>, PackIndexError> {
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
    fn drop_location(&self, digest: &BlobDigest, pack_key: &str) -> Result<bool, PackIndexError> {
        self.inner.drop_location(digest, pack_key)
    }
    fn forget_pack(&self, pack_key: &str, min_age: Duration) -> Result<Forgot, PackIndexError> {
        self.inner.forget_pack(pack_key, min_age)
    }
    fn object_cells(&self, keys: &[String]) -> Result<Vec<Option<ObjectCells>>, PackIndexError> {
        self.inner.object_cells(keys)
    }
    fn record_cells(&self, key: &str, cells: &ObjectCells) -> Result<bool, PackIndexError> {
        self.inner.record_cells(key, cells)
    }
    fn record_unlisted(&self, tree: &[u8; 32]) -> Result<(), PackIndexError> {
        self.inner.record_unlisted(tree)
    }
    fn unlisted(
        &self,
        within: Duration,
        after: Option<&[u8; 32]>,
        limit: usize,
    ) -> Result<Vec<Unlisted>, PackIndexError> {
        if self.fail_unlisted.load(Ordering::SeqCst) {
            return Err(down());
        }
        self.inner.unlisted(within, after, limit)
    }
    fn drop_unlisted(&self, tree: &[u8; 32]) -> Result<bool, PackIndexError> {
        if self.fail_drop.load(Ordering::SeqCst) {
            return Err(down());
        }
        self.inner.drop_unlisted(tree)
    }
    fn live_packs(
        &self,
        after: Option<&str>,
        limit: usize,
    ) -> Result<Vec<LivePack>, PackIndexError> {
        if self.fail_live.load(Ordering::SeqCst) {
            return Err(down());
        }
        self.inner.live_packs(after, limit)
    }
}

/// A tree to backfill: its id, and the keys of its objects.
pub struct Tree {
    pub id: [u8; 32],
    pub objects: Vec<String>,
}

impl Tree {
    pub fn hex(&self) -> String {
        self.id.iter().map(|byte| format!("{byte:02x}")).collect()
    }
}

/// Everything a run needs: the store, the index, the clocks and the credentials.
pub struct World {
    pub dir: TestDir,
    pub runtime: tokio::runtime::Runtime,
    pub store: Arc<Probe>,
    pub index: Arc<Faulty>,
    pub index_clock: Arc<ManualClock>,
    pub clock: Arc<StepClock>,
    pub naps: Arc<Naps>,
    pub seed: u64,
    pub credentials: PathBuf,
}

impl World {
    pub fn new(name: &str) -> Self {
        let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), name);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("a runtime");
        let credentials = dir.path().join("keys.json");
        write_secret(&credentials, 0o600);
        let index_clock = Arc::new(ManualClock::default());
        let clock = Arc::new(StepClock::default());
        Self {
            dir,
            runtime,
            store: Arc::new(Probe::new()),
            index: Arc::new(Faulty {
                inner: MemoryPackIndex::new(index_clock.clone()),
                fail_unlisted: AtomicBool::new(false),
                fail_drop: AtomicBool::new(false),
                fail_live: AtomicBool::new(false),
            }),
            index_clock,
            naps: Arc::new(Naps {
                waits: Mutex::default(),
                clock: Arc::clone(&clock),
            }),
            clock,
            seed: 7,
            credentials,
        }
    }

    pub fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    /// A tree of one file per entry of `contents`, in the store and recorded unlisted.
    pub fn tree(&self, contents: &[Vec<u8>]) -> Tree {
        let tree = self.tree_unrecorded(contents);
        self.index.record_unlisted(&tree.id).expect("record");
        tree
    }

    /// As [`World::tree`], without the unlisted row.
    pub fn tree_unrecorded(&self, contents: &[Vec<u8>]) -> Tree {
        let mut entries = Vec::new();
        let mut objects = Vec::new();
        for (at, data) in contents.iter().enumerate() {
            let key = format!("cas/l/{}", sha_hex(data));
            self.runtime.block_on(self.store.put(&key, data));
            entries.push(ListEntry {
                path: format!("f{at:03}"),
                kind: ListKind::File {
                    mode: Mode::FILE,
                    size: data.len() as u64,
                    source: Some(BlobSource {
                        key: Arc::from(key.as_str()),
                        offset: 0,
                    }),
                    sha256: Some(TreeDigest::of(data)),
                },
            });
            objects.push(key);
        }
        let list = TreeList { entries }.encode_v2().expect("a list");
        let id: [u8; 32] = Sha256::digest(&list).into();
        let tree = Tree { id, objects };
        self.runtime
            .block_on(self.store.put(&format!("cas/l/{}", tree.hex()), &list));
        tree
    }

    /// A tree recorded unlisted whose list the store does not have.
    pub fn tree_without_list(&self, n: u32) -> Tree {
        let id: [u8; 32] = Sha256::digest(n.to_le_bytes()).into();
        self.index.record_unlisted(&id).expect("record");
        Tree {
            id,
            objects: Vec::new(),
        }
    }

    /// The unlisted trees the index holds.
    pub fn unlisted(&self) -> Vec<[u8; 32]> {
        let page = self.index.unlisted(EVER, None, 1000).expect("unlisted");
        page.into_iter().map(|row| row.tree).collect()
    }

    /// A live pack under `key`.
    pub fn live_pack(&self, key: &str) {
        let digest = BlobDigest {
            hash: Sha256::digest(key.as_bytes()).into(),
            size: 1,
        };
        let pack = NewPack {
            key: key.to_owned(),
            writer: "test".to_owned(),
            size: 1,
            blobs: vec![PackBlob { digest, offset: 0 }],
        };
        self.index.record_pack(&pack).expect("a pack");
    }

    /// A pack under `key` that no blob row points at.
    pub fn dead_pack(&self, key: &str) {
        self.live_pack(key);
        let digest = BlobDigest {
            hash: Sha256::digest(key.as_bytes()).into(),
            size: 1,
        };
        self.index.drop_location(&digest, key).expect("a drop");
    }

    /// The seams a run is given.
    pub fn seams(&self) -> Seams {
        let index = Arc::clone(&self.index);
        let store = Arc::clone(&self.store);
        let seed = self.seed;
        Seams {
            open_index: Box::new(move |_| Ok(index.clone())),
            open_store: Box::new(move |_| store.clone()),
            clock: self.clock.clone(),
            sleeper: self.naps.clone(),
            seed: Box::new(move || seed),
        }
    }

    /// The flags every run has, and `more`.
    pub fn argv(&self, more: &[&str]) -> Vec<OsString> {
        let mut argv: Vec<OsString> = [
            "kbf-l2-backfill",
            "--store-endpoint",
            ENDPOINT,
            "--store-bucket",
            "kbf-fake",
            "--pack-index",
            SOCKET_URL,
            "--store-credentials-file",
        ]
        .into_iter()
        .map(OsString::from)
        .collect();
        argv.push(self.credentials.clone().into_os_string());
        argv.extend(more.iter().map(OsString::from));
        argv
    }

    pub fn run(&self, more: &[&str]) -> Ran {
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = main_with_seams(self.argv(more), &self.seams(), &mut out, &mut err);
        Ran {
            code,
            out: String::from_utf8_lossy(&out).into_owned(),
            err: String::from_utf8_lossy(&err).into_owned(),
        }
    }
}

/// Writes a JSON key pair to `path` with `mode`.
pub fn write_secret(path: &Path, mode: u32) {
    std::fs::write(
        path,
        r#"{"access_key_id": "AKIAEXAMPLE", "secret_access_key": "not-a-real-secret"}"#,
    )
    .expect("a credentials file");
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).expect("a mode");
}

/// Asserts that `text` contains `needle`.
pub fn says(text: &str, needle: &str) {
    assert!(text.contains(needle), "{needle:?} not in:\n{text}");
}
