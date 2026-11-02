//! The plane `over` makes: reads through an in-process stand-in for the tierstore, the places
//! kept from a stand-in for the frontend's pack index that counts what it is asked, and the
//! frontend's plane underneath, which notes every call and can be told to fail.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use bytes::Bytes;
use kbf_objstore::{
    ByteRange, Capabilities, KeyPrefix, ListPage, ListToken, MemoryStore, ObjectKey,
    ObjectStoreError, PageSize,
};
use kbf_pack::{PackKeys, Packer, PackerConfig, WriterKind, blob_digest};
use kbf_pack_index::memory::MemoryPackIndex;
use kbf_pack_index::{BlobDigest, Location, NewPack, PackBlob, PackIndexError, PackInfo, Recorded};
use kbf_proto::reapi::Digest;
use kbf_state::ManualClock;
use kbf_transfer::CasError;
use kbf_transfer::cas::{digest_of, label};
use tokio::io::AsyncWriteExt as _;
use tokio::sync::watch;

use super::*;

mod stalls;
mod transcript;

/// The frontend's plane, as the daemon reads it without the flag: blobs in memory, the calls
/// made on it, and the error it answers every read with while one is set.
#[derive(Clone, Default)]
pub(super) struct Front {
    blobs: Arc<Mutex<BTreeMap<String, Vec<u8>>>>,
    calls: Arc<Mutex<Vec<&'static str>>>,
    failing: Arc<Mutex<Option<CasError>>>,
}

impl Front {
    fn note(&self, call: &'static str) {
        self.calls
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(call);
    }

    pub(super) fn calls(&self) -> Vec<&'static str> {
        self.calls
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    pub(super) fn insert(&self, bytes: &[u8]) -> Digest {
        let digest = digest_of(bytes);
        let mut blobs = self.blobs.lock().unwrap_or_else(PoisonError::into_inner);
        blobs.insert(label(&digest), bytes.to_vec());
        digest
    }

    /// Makes every read answer `error` (`None`: answer what is held).
    pub(super) fn fail(&self, error: Option<CasError>) {
        *self.failing.lock().unwrap_or_else(PoisonError::into_inner) = error;
    }

    fn blob(&self, digest: &Digest) -> Result<Vec<u8>, CasError> {
        if let Some(error) = self
            .failing
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
        {
            return Err(error);
        }
        let blobs = self.blobs.lock().unwrap_or_else(PoisonError::into_inner);
        let held = blobs.get(&label(digest)).cloned();
        held.ok_or_else(|| CasError::Missing(label(digest)))
    }
}

impl Cas for Front {
    type Lease = Front;

    fn lease(self: &Arc<Self>, _: &kbf_types::LeaseId) -> Front {
        self.note("lease");
        Front::clone(self)
    }

    async fn get(&self, digest: &Digest) -> Result<Vec<u8>, CasError> {
        self.note("get");
        self.blob(digest)
    }

    async fn read_to(&self, digest: &Digest, file: &mut tokio::fs::File) -> Result<(), CasError> {
        self.note("read_to");
        let bytes = self.blob(digest)?;
        let written = file.write_all(&bytes).await.and(file.flush().await);
        written.map_err(|e| CasError::Write(label(digest), e.to_string()))
    }

    async fn put(&self, bytes: Vec<u8>) -> Result<Digest, CasError> {
        self.note("put");
        Ok(self.insert(&bytes))
    }

    async fn missing(&self, digests: &[Digest]) -> Vec<Digest> {
        self.note("missing");
        let blobs = self.blobs.lock().unwrap_or_else(PoisonError::into_inner);
        let absent = |digest: &&Digest| !blobs.contains_key(&label(digest));
        digests.iter().filter(absent).cloned().collect()
    }
}

/// How the stand-in for the tierstore answers a read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Mode {
    /// From what it holds.
    Serve,
    /// A connection refused, at once.
    Down,
    /// Not until the gate opens, and then from what it holds.
    HeldThenServe,
    /// Not until the gate opens, and then a request that timed out.
    HeldThenFail,
}

/// The tierstore, in process: a memory object store that counts the reads made of it and the
/// reads under way, and answers as its [`Mode`] says.
pub(super) struct Tier {
    store: MemoryStore,
    reads: AtomicUsize,
    mode: Mutex<Mode>,
    /// Reads that have reached the store and not answered yet.
    waiting: watch::Sender<usize>,
    /// Opened by the test: the held reads go on.
    gate: watch::Sender<bool>,
}

impl Tier {
    fn new() -> Self {
        Self {
            // Create-only, so that the test's packer, the farm's writer of packs here, takes it.
            store: MemoryStore::new(Capabilities {
                conditional_put: true,
                object_lock: false,
            }),
            reads: AtomicUsize::new(0),
            mode: Mutex::new(Mode::Serve),
            waiting: watch::channel(0).0,
            gate: watch::channel(false).0,
        }
    }

    pub(super) fn set(&self, mode: Mode) {
        *self.mode.lock().unwrap_or_else(PoisonError::into_inner) = mode;
    }

    /// Waits until `n` reads are held at the store.
    pub(super) async fn held(&self, n: usize) {
        let mut seen = self.waiting.subscribe();
        seen.wait_for(|waiting| *waiting >= n)
            .await
            .expect("the tier is kept");
    }

    /// Lets the held reads go on.
    pub(super) fn open(&self) {
        self.gate.send_replace(true);
    }
}

/// Counts a held read down when it ends, answered or dropped.
struct Waiting<'a>(&'a watch::Sender<usize>);

impl Drop for Waiting<'_> {
    fn drop(&mut self) {
        self.0.send_modify(|n| *n -= 1);
    }
}

impl ObjectStore for Tier {
    fn capabilities(&self) -> Capabilities {
        self.store.capabilities()
    }

    async fn put_new(
        &self,
        key: &ObjectKey,
        body: Bytes,
        retain_until: Option<std::time::SystemTime>,
    ) -> Result<(), ObjectStoreError> {
        self.store.put_new(key, body, retain_until).await
    }

    async fn get_range(
        &self,
        key: &ObjectKey,
        range: ByteRange,
    ) -> Result<Bytes, ObjectStoreError> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        let mode = *self.mode.lock().unwrap_or_else(PoisonError::into_inner);
        if mode == Mode::Down {
            let refused = std::io::Error::from(std::io::ErrorKind::ConnectionRefused);
            return Err(ObjectStoreError::Transport(Arc::new(refused)));
        }
        if matches!(mode, Mode::HeldThenServe | Mode::HeldThenFail) {
            self.waiting.send_modify(|n| *n += 1);
            let _counted = Waiting(&self.waiting);
            let mut gate = self.gate.subscribe();
            gate.wait_for(|open| *open).await.expect("the tier is kept");
            if mode == Mode::HeldThenFail {
                let timed_out = std::io::Error::from(std::io::ErrorKind::TimedOut);
                return Err(ObjectStoreError::Transport(Arc::new(timed_out)));
            }
        }
        self.store.get_range(key, range).await
    }

    async fn delete(&self, key: &ObjectKey) -> Result<(), ObjectStoreError> {
        self.store.delete(key).await
    }

    async fn list(
        &self,
        prefix: &KeyPrefix,
        after: Option<&ListToken>,
        max_keys: PageSize,
    ) -> Result<ListPage, ObjectStoreError> {
        self.store.list(prefix, after, max_keys).await
    }
}

/// The frontend's pack index, as the daemon asks it: an index in memory that counts the blobs it
/// is asked to locate and, while `down`, fails every call, as a frontend that cannot be reached.
pub(super) struct Asked {
    index: MemoryPackIndex,
    located: AtomicUsize,
    down: AtomicBool,
}

impl Asked {
    fn check(&self) -> Result<(), PackIndexError> {
        if self.down.load(Ordering::SeqCst) {
            return Err(PackIndexError::NotMigrated);
        }
        Ok(())
    }
}

impl PackIndex for Asked {
    fn record_pack(&self, pack: &NewPack) -> Result<Recorded, PackIndexError> {
        self.check()?;
        self.index.record_pack(pack)
    }

    fn pack(&self, key: &str) -> Result<Option<PackInfo>, PackIndexError> {
        self.check()?;
        self.index.pack(key)
    }

    fn locate(&self, digests: &[BlobDigest]) -> Result<Vec<Option<Location>>, PackIndexError> {
        self.located.fetch_add(digests.len(), Ordering::SeqCst);
        self.check()?;
        self.index.locate(digests)
    }

    fn find_missing(&self, digests: &[BlobDigest]) -> Result<Vec<BlobDigest>, PackIndexError> {
        self.check()?;
        self.index.find_missing(digests)
    }

    fn touch(&self, digests: &[BlobDigest]) -> Result<usize, PackIndexError> {
        self.check()?;
        self.index.touch(digests)
    }

    fn dead_packs(&self, grace: Duration, limit: usize) -> Result<Vec<PackInfo>, PackIndexError> {
        self.check()?;
        self.index.dead_packs(grace, limit)
    }

    fn unreferenced_keys(&self, keys: &[String]) -> Result<Vec<String>, PackIndexError> {
        self.check()?;
        self.index.unreferenced_keys(keys)
    }
}

/// Where the pack of a blob that is recorded in the index without being stored is.
const FORGED: &str = "cas/d/node1/boot1/00000000000000ff";

/// The frontend's plane, the tierstore, the frontend's pack index, a packer over the last two,
/// the places kept, and the plane [`over_placed`] makes of them.
pub(super) struct Rig {
    pub(super) front: Front,
    pub(super) tier: Arc<Tier>,
    pub(super) asked: Arc<Asked>,
    packer: Packer<Tier>,
    pub(super) placed: Arc<Placed>,
    pub(super) counter: PackCounter,
    pub(super) cas: Arc<PackedCas<Front>>,
}

impl Rig {
    pub(super) fn new() -> Self {
        let tier = Arc::new(Tier::new());
        let asked = Arc::new(Asked {
            index: MemoryPackIndex::new(Arc::new(ManualClock::default())),
            located: AtomicUsize::new(0),
            down: AtomicBool::new(false),
        });
        let shared: Arc<dyn PackIndex> = asked.clone();
        let keys = PackKeys::new(WriterKind::Daemon, "node1", "boot1").expect("keys");
        let config = PackerConfig::default();
        let packer = Packer::new(Arc::clone(&tier), Arc::clone(&shared), keys, config);
        let front = Front::default();
        let counter = PackCounter::default();
        let placed = Arc::new(Placed::new(shared));
        let cas = over_placed(
            Arc::new(front.clone()),
            Arc::clone(&tier),
            Arc::clone(&placed),
            &counter,
        );
        Self {
            front,
            tier,
            asked,
            packer: packer.expect("packer"),
            placed,
            counter,
            cas,
        }
    }

    /// Packs `bytes` into the tierstore, as a writer of the farm's packs would, and records the
    /// pack in the index; their digest.
    pub(super) async fn pack(&self, bytes: &'static [u8]) -> Digest {
        let blob = kbf_pack::Blob {
            digest: blob_digest(bytes),
            bytes: Bytes::from_static(bytes),
        };
        self.packer.pack(vec![blob]).await.expect("packed");
        digest_of(bytes)
    }

    /// Records in the index an object `key` that holds `true_bytes` at its start, and stores
    /// `stored` as that object's bytes in the tierstore if given (`None`: the tierstore answers
    /// 404 for it). The digest of `true_bytes`.
    async fn record(
        &self,
        key: &str,
        true_bytes: &'static [u8],
        stored: Option<&'static [u8]>,
    ) -> Digest {
        let pack = NewPack {
            key: key.to_owned(),
            writer: "node1".to_owned(),
            size: true_bytes.len() as u64,
            blobs: vec![PackBlob {
                digest: blob_digest(true_bytes),
                offset: 0,
            }],
        };
        self.asked.index.record_pack(&pack).expect("recorded");
        if let Some(stored) = stored {
            let object = ObjectKey::new(key).expect("a key");
            let put = self
                .tier
                .put_new(&object, Bytes::from_static(stored), None)
                .await;
            put.expect("stored");
        }
        digest_of(true_bytes)
    }

    /// [`Self::record`] at a daemon pack's key.
    pub(super) async fn forge(
        &self,
        true_bytes: &'static [u8],
        stored: Option<&'static [u8]>,
    ) -> Digest {
        self.record(FORGED, true_bytes, stored).await
    }

    pub(super) fn reads(&self) -> usize {
        self.tier.reads.load(Ordering::SeqCst)
    }

    pub(super) fn located(&self) -> usize {
        self.asked.located.load(Ordering::SeqCst)
    }
}

/// `digest` read with `get`, and with `read_to` into a fresh file of `dir`: the two answers,
/// the second as the bytes the file holds after it.
async fn both(cas: &PackedCas<Front>, dir: &Path, digest: &Digest) -> (Vec<u8>, Vec<u8>) {
    let got = cas.get(digest).await.expect("get");
    let path = dir.join(label(digest).replace('/', "-"));
    let mut file = tokio::fs::File::create(&path).await.expect("create");
    cas.read_to(digest, &mut file).await.expect("read_to");
    (got, std::fs::read(&path).expect("read back"))
}

/// Asserts that a blob the tierstore holds is served from it to `get` and to `read_to`, whole,
/// that the frontend's CAS is never called, and that its place is asked of the frontend's index
/// once, by the first read; the second read is found from the kept place.
/// Catches a read that fetches the bytes from the frontend though the tierstore has the blob.
#[tokio::test]
async fn a_hit_is_served_from_the_tierstore_and_never_read_from_the_frontends_cas() {
    let dir = scratch("tierstore-hit");
    let rig = Rig::new();
    let digest = rig.pack(b"in the tierstore").await;
    let located = rig.located();
    let (got, written) = both(&rig.cas, &dir, &digest).await;
    assert_eq!(got, b"in the tierstore");
    assert_eq!(written, b"in the tierstore");
    assert_eq!(rig.front.calls(), Vec::<&str>::new());
    assert_eq!((rig.reads(), rig.located() - located), (2, 1));
    let stats = rig.counter.stats();
    assert_eq!((stats.served, stats.served_bytes), (2, 32));
    assert_eq!(
        stats.elsewhere + stats.unasked + stats.lost + stats.damaged,
        0
    );
    kbf_outputs::remove_tree(&dir).expect("clean");
}

/// Asserts that a warm hit touches nothing of the frontend: with the frontend's index failing
/// every call, a blob read once is read again from the tierstore, and neither the index is asked
/// nor the frontend's CAS called.
/// Catches a warm read that waits on, or fails with, the frontend.
#[tokio::test]
async fn a_warm_hit_asks_nothing_of_the_frontend_even_when_its_index_is_down() {
    let rig = Rig::new();
    let digest = rig.pack(b"warm").await;
    assert_eq!(rig.cas.get(&digest).await, Ok(b"warm".to_vec()));
    let located = rig.located();
    rig.asked.down.store(true, Ordering::SeqCst);
    assert_eq!(rig.cas.get(&digest).await, Ok(b"warm".to_vec()));
    assert_eq!(rig.located(), located);
    assert_eq!(rig.front.calls(), Vec::<&str>::new());
    assert_eq!(rig.counter.stats().served, 2);
}

/// Asserts that the place of a blob whose pack the tierstore no longer has is forgotten: the read
/// falls back to the frontend and succeeds, nothing is kept for it, and its next read asks the
/// frontend's index again rather than the stale place.
#[tokio::test]
async fn a_place_whose_pack_is_gone_is_forgotten() {
    let rig = Rig::new();
    let digest = rig.forge(b"gone later", Some(b"gone later")).await;
    rig.front.insert(b"gone later");
    assert_eq!(rig.cas.get(&digest).await, Ok(b"gone later".to_vec()));
    assert_eq!(rig.placed.len(), 1);
    let key = ObjectKey::new(FORGED).expect("a key");
    rig.tier.delete(&key).await.expect("deleted");
    assert_eq!(rig.cas.get(&digest).await, Ok(b"gone later".to_vec()));
    assert_eq!(rig.placed.len(), 0);
    let located = rig.located();
    assert_eq!(rig.cas.get(&digest).await, Ok(b"gone later".to_vec()));
    assert_eq!(rig.located(), located + 1);
    assert_eq!(rig.front.calls(), ["get", "get"]);
}

/// Asserts that at most the given number of places are kept, the oldest going first, and that a
/// place kept again is not kept twice.
#[test]
fn the_oldest_place_goes_first() {
    let asked = Arc::new(Asked {
        index: MemoryPackIndex::new(Arc::new(ManualClock::default())),
        located: AtomicUsize::new(0),
        down: AtomicBool::new(false),
    });
    let blobs: Vec<BlobDigest> = [b"one".as_slice(), b"two", b"six"]
        .iter()
        .map(|b| blob_digest(b))
        .collect();
    for (i, digest) in blobs.iter().enumerate() {
        let pack = NewPack {
            key: format!("cas/d/node1/boot1/{i:016x}"),
            writer: "node1".to_owned(),
            size: 3,
            blobs: vec![PackBlob {
                digest: *digest,
                offset: 0,
            }],
        };
        asked.index.record_pack(&pack).expect("recorded");
    }
    let placed = Placed::keeping(asked.clone(), 2);
    placed.locate(&blobs[..2]).expect("located");
    placed.locate(&blobs[..1]).expect("located again");
    assert_eq!((placed.len(), asked.located.load(Ordering::SeqCst)), (2, 2));
    placed.locate(&blobs[2..]).expect("located");
    assert_eq!(placed.len(), 2);
    // The first is the oldest kept: it is asked again; the other two are answered as kept.
    placed.locate(&blobs).expect("located");
    assert_eq!(asked.located.load(Ordering::SeqCst), 4);
}

/// Asserts that every call of the index but `locate` is the frontend's index's, answered as it
/// answers: a pack recorded through the places is in the index, and the other questions get the
/// index's own answers.
#[test]
fn the_places_pass_every_other_call_to_the_index() {
    let asked = Arc::new(Asked {
        index: MemoryPackIndex::new(Arc::new(ManualClock::default())),
        located: AtomicUsize::new(0),
        down: AtomicBool::new(false),
    });
    let placed = Placed::new(asked.clone());
    let blob = blob_digest(b"recorded");
    let key = "cas/d/node1/boot1/00000000000000aa";
    let pack = NewPack {
        key: key.to_owned(),
        writer: "node1".to_owned(),
        size: 8,
        blobs: vec![PackBlob {
            digest: blob,
            offset: 0,
        }],
    };
    placed.record_pack(&pack).expect("recorded");
    assert!(asked.index.pack(key).expect("asked").is_some());
    assert_eq!(
        placed.pack(key).expect("asked"),
        asked.index.pack(key).expect("asked")
    );
    let absent = blob_digest(b"absent");
    let missing = placed.find_missing(&[blob, absent]).expect("asked");
    assert_eq!(missing, vec![absent]);
    assert_eq!(
        placed.touch(&[blob]).expect("touched"),
        asked.index.touch(&[blob]).expect("touched")
    );
    let dead = placed.dead_packs(Duration::ZERO, 10).expect("asked").len();
    assert_eq!(
        dead,
        asked
            .index
            .dead_packs(Duration::ZERO, 10)
            .expect("asked")
            .len()
    );
    let keys = ["cas/x".to_owned(), key.to_owned()];
    let unreferenced = placed.unreferenced_keys(&keys).expect("asked");
    assert_eq!(
        unreferenced,
        asked.index.unreferenced_keys(&keys).expect("asked")
    );
    asked.down.store(true, Ordering::SeqCst);
    assert!(placed.locate(&[absent]).is_err());
}

/// Asserts that a blob the frontend's index places in no pack is read from the frontend, with the
/// frontend's bytes, the tierstore not asked for it, and counted as read elsewhere.
#[tokio::test]
async fn a_blob_in_no_pack_is_read_from_the_frontend() {
    let dir = scratch("tierstore-unplaced");
    let rig = Rig::new();
    let digest = rig.front.insert(b"only at the frontend");
    let (got, written) = both(&rig.cas, &dir, &digest).await;
    assert_eq!(got, b"only at the frontend");
    assert_eq!(written, b"only at the frontend");
    assert_eq!(rig.front.calls(), ["get", "read_to"]);
    assert_eq!(rig.reads(), 0);
    assert_eq!(rig.counter.stats().elsewhere, 2);
    kbf_outputs::remove_tree(&dir).expect("clean");
}

/// Asserts that a blob in a segment a storage node flushed to the object store, which the index
/// places as it places a pack, is served from the tierstore like any packed blob.
#[tokio::test]
async fn a_blob_of_a_flushed_segment_is_served_from_the_tierstore() {
    let rig = Rig::new();
    let segment = "cas/member1/boot1/0000000000000001";
    let digest = rig
        .record(segment, b"segment bytes", Some(b"segment bytes"))
        .await;
    assert_eq!(rig.cas.get(&digest).await, Ok(b"segment bytes".to_vec()));
    assert_eq!(rig.front.calls(), Vec::<&str>::new());
}

/// Asserts that a pack the tierstore answers 404 for costs a read from the frontend and never a
/// failed one: both reads answer the frontend's bytes, counted as fallbacks and not as lost.
/// Catches a tierstore miss that fails a lease's input fetch, or is reported as a failed read.
#[tokio::test]
async fn a_miss_in_the_tierstore_falls_back_to_the_frontend_and_the_read_succeeds() {
    let dir = scratch("tierstore-miss");
    let rig = Rig::new();
    let digest = rig.forge(b"not in the tierstore", None).await;
    rig.front.insert(b"not in the tierstore");
    let (got, written) = both(&rig.cas, &dir, &digest).await;
    assert_eq!(got, b"not in the tierstore");
    assert_eq!(written, b"not in the tierstore");
    assert_eq!(rig.front.calls(), ["get", "read_to"]);
    let stats = rig.counter.stats();
    assert_eq!((stats.fallback_lost, stats.lost, stats.served), (2, 0, 0));
    kbf_outputs::remove_tree(&dir).expect("clean");
}

/// Asserts that bytes from the tierstore that are not the digest are never served: both reads
/// answer the frontend's true bytes, counted as fallbacks and not as damaged.
#[tokio::test]
async fn damaged_bytes_from_the_tierstore_fall_back_to_the_frontend() {
    let dir = scratch("tierstore-damaged");
    let rig = Rig::new();
    let digest = rig.forge(b"true bytes", Some(b"xxxx bytes")).await;
    rig.front.insert(b"true bytes");
    let (got, written) = both(&rig.cas, &dir, &digest).await;
    assert_eq!(got, b"true bytes");
    assert_eq!(written, b"true bytes");
    assert_eq!(rig.front.calls(), ["get", "read_to"]);
    let stats = rig.counter.stats();
    assert_eq!((stats.fallback_damaged, stats.damaged), (2, 0));
    kbf_outputs::remove_tree(&dir).expect("clean");
}

/// Asserts that after the tierstore missed or had damaged bytes, the frontend's answer is the
/// read's answer exactly, whatever it is: `Missing`, `Unavailable` and `Corrupt` come back as the
/// frontend gave them, for `get` and `read_to`; a destination that already holds bytes keeps
/// them, with the frontend's bytes after them and nothing of the tierstore's; and a destination
/// that cannot be written fails as `Write`.
/// Catches a fallback that hides or rewrites the frontend's failure, or writes rejected bytes.
#[tokio::test]
async fn after_a_fallback_the_frontends_answer_is_the_answer() {
    let dir = scratch("tierstore-authority");
    let rig = Rig::new();
    let lost = rig
        .record("cas/d/node1/boot1/0000000000000001", b"lost bytes", None)
        .await;
    let damaged = rig
        .record(
            "cas/d/node1/boot1/0000000000000002",
            b"good bytes",
            Some(b"evil bytes"),
        )
        .await;
    let errors = [
        CasError::Missing("m".to_owned()),
        CasError::Unavailable("u".to_owned(), "the frontend is down".to_owned()),
        CasError::Corrupt("c".to_owned(), "another digest".to_owned()),
    ];
    for error in errors {
        rig.front.fail(Some(error.clone()));
        for digest in [&lost, &damaged] {
            assert_eq!(rig.cas.get(digest).await, Err(error.clone()));
            let mut file = tokio::fs::File::create(dir.join("failed"))
                .await
                .expect("create");
            assert_eq!(rig.cas.read_to(digest, &mut file).await, Err(error.clone()));
        }
    }
    rig.front.fail(None);
    rig.front.insert(b"good bytes");
    let path = dir.join("prefixed");
    let mut file = tokio::fs::File::create(&path).await.expect("create");
    file.write_all(b"head:").await.expect("head");
    file.flush().await.expect("flush");
    rig.cas.read_to(&damaged, &mut file).await.expect("read_to");
    assert_eq!(std::fs::read(&path).expect("read back"), b"head:good bytes");

    std::fs::write(dir.join("closed"), b"").expect("create");
    let readonly = std::fs::File::open(dir.join("closed")).expect("open for reading");
    let mut closed = tokio::fs::File::from_std(readonly);
    let failed = rig.cas.read_to(&damaged, &mut closed).await;
    assert!(
        matches!(&failed, Err(CasError::Write(blob, _)) if *blob == label(&damaged)),
        "{failed:?}"
    );
    let packed = rig.pack(b"packed and served").await;
    let failed = rig.cas.read_to(&packed, &mut closed).await;
    assert!(
        matches!(&failed, Err(CasError::Write(blob, _)) if *blob == label(&packed)),
        "{failed:?}"
    );
    kbf_outputs::remove_tree(&dir).expect("clean");
}

/// Asserts that a tierstore that is down (each read refused) costs reads from the frontend and
/// never a failed one, counted as unasked; and that once it is up again, with no restart, the
/// same blob is served from it again and the frontend is not asked.
#[tokio::test]
async fn a_tierstore_that_is_down_is_read_around_and_used_again_once_up() {
    let dir = scratch("tierstore-down");
    let rig = Rig::new();
    let digest = rig.pack(b"packed and at the frontend").await;
    rig.front.insert(b"packed and at the frontend");
    rig.tier.set(Mode::Down);
    let (got, written) = both(&rig.cas, &dir, &digest).await;
    assert_eq!(got, b"packed and at the frontend");
    assert_eq!(written, b"packed and at the frontend");
    assert_eq!(rig.front.calls(), ["get", "read_to"]);
    assert_eq!(rig.counter.stats().unasked, 2);

    rig.tier.set(Mode::Serve);
    assert_eq!(
        rig.cas.get(&digest).await,
        Ok(b"packed and at the frontend".to_vec())
    );
    assert_eq!(rig.front.calls(), ["get", "read_to"]);
    assert_eq!(rig.counter.stats().served, 1);
    kbf_outputs::remove_tree(&dir).expect("clean");
}

/// Asserts that a blob over 64 MiB, which a read from a pack would hold whole, is read from the
/// frontend without asking the tierstore, and that a blob in neither is the frontend's
/// `Missing`, as without the flag.
#[tokio::test]
async fn a_large_blob_and_a_blob_nowhere_are_the_frontends() {
    let rig = Rig::new();
    let large = Digest {
        hash: "ab".repeat(32),
        size_bytes: (64 << 20) + 1,
    };
    assert_eq!(
        rig.cas.get(&large).await,
        Err(CasError::Missing(label(&large)))
    );
    let nowhere = digest_of(b"nowhere");
    assert_eq!(
        rig.cas.get(&nowhere).await,
        Err(CasError::Missing(label(&nowhere)))
    );
    assert_eq!(rig.front.calls(), ["get", "get"]);
    assert_eq!(rig.reads(), 0);
}

/// Asserts that a lease's plane reads through the tierstore as its plane does, and falls back as
/// it does: a hit is not read from the frontend, a miss is.
#[tokio::test]
async fn a_lease_reads_through_the_tierstore_and_falls_back() {
    let rig = Rig::new();
    let hit = rig.pack(b"leased hit").await;
    let miss = rig.forge(b"leased miss", None).await;
    rig.front.insert(b"leased miss");
    let lease = rig.cas.lease(&kbf_types::LeaseId::new(1, 2));
    assert_eq!(lease.get(&hit).await, Ok(b"leased hit".to_vec()));
    assert_eq!(lease.get(&miss).await, Ok(b"leased miss".to_vec()));
    assert_eq!(rig.front.calls(), ["lease", "get"]);
}

/// Asserts that `over`, the production composition, keeps the places it is answered: a second
/// read of a blob does not ask the index.
#[tokio::test]
async fn over_keeps_the_places_it_is_answered() {
    let rig = Rig::new();
    let digest = rig.pack(b"through over").await;
    let shared: Arc<dyn PackIndex> = rig.asked.clone();
    let cas = over(
        Arc::new(rig.front.clone()),
        Arc::clone(&rig.tier),
        shared,
        &PackCounter::default(),
    );
    let located = rig.located();
    assert_eq!(cas.get(&digest).await, Ok(b"through over".to_vec()));
    assert_eq!(cas.get(&digest).await, Ok(b"through over".to_vec()));
    assert_eq!(rig.located(), located + 1);
}
