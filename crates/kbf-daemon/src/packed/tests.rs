//! [`PackedCas`] over a pack reader on a memory object store and a memory pack index, and a
//! plane underneath that says which of its calls were made.

use std::collections::BTreeMap;
use std::sync::atomic::AtomicUsize;
use std::sync::{Mutex, PoisonError};

use kbf_objstore::{Capabilities, MemoryStore, ObjectKey, ObjectStore as _};
use kbf_pack::{PackKeys, Packer, PackerConfig, ReaderConfig, WriterKind, blob_digest};
use kbf_pack_index::memory::MemoryPackIndex;
use kbf_pack_index::{NewPack, PackBlob, PackIndex, PackIndexError};
use kbf_state::ManualClock;
use kbf_testkit::dir::TestDir;
use tokio::io::AsyncWriteExt as _;

use super::*;
use crate::cas::digest_of;

/// A plane that holds blobs in memory and notes the calls made on it.
#[derive(Clone, Default)]
struct Held {
    blobs: Arc<Mutex<BTreeMap<String, Vec<u8>>>>,
    calls: Arc<Mutex<Vec<&'static str>>>,
}

impl Held {
    fn note(&self, call: &'static str) {
        let mut calls = self.calls.lock().unwrap_or_else(PoisonError::into_inner);
        calls.push(call);
    }

    fn calls(&self) -> Vec<&'static str> {
        self.calls
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn insert(&self, bytes: &[u8]) -> Digest {
        let digest = digest_of(bytes);
        let mut blobs = self.blobs.lock().unwrap_or_else(PoisonError::into_inner);
        blobs.insert(label(&digest), bytes.to_vec());
        digest
    }

    fn blob(&self, digest: &Digest) -> Result<Vec<u8>, CasError> {
        let blobs = self.blobs.lock().unwrap_or_else(PoisonError::into_inner);
        let held = blobs.get(&label(digest)).cloned();
        held.ok_or_else(|| CasError::Missing(label(digest)))
    }
}

impl Cas for Held {
    type Lease = Held;

    fn lease(self: &Arc<Self>, _: &LeaseId) -> Held {
        self.note("lease");
        Held::clone(self)
    }

    async fn get(&self, digest: &Digest) -> Result<Vec<u8>, CasError> {
        self.note("get");
        self.blob(digest)
    }

    async fn put(&self, bytes: Vec<u8>) -> Result<Digest, CasError> {
        self.note("put");
        Ok(self.insert(&bytes))
    }

    async fn put_chunks(&self, digest: Digest, _: Chunks) -> Result<Digest, CasError> {
        self.note("put_chunks");
        Ok(digest)
    }

    async fn read_to(&self, digest: &Digest, file: &mut tokio::fs::File) -> Result<(), CasError> {
        self.note("read_to");
        let bytes = self.blob(digest)?;
        file.write_all(&bytes).await.expect("write");
        file.flush().await.expect("flush");
        Ok(())
    }

    async fn missing(&self, digests: &[Digest]) -> Vec<Digest> {
        self.note("missing");
        digests.iter().rev().cloned().collect()
    }

    async fn put_records(&self, _: Vec<Record>) -> Result<(), CasError> {
        self.note("put_records");
        Ok(())
    }

    async fn put_spool(&self, _: Arc<Spool>) -> Result<(), CasError> {
        self.note("put_spool");
        Ok(())
    }

    fn intake(&self) -> Option<Arc<dyn Intake>> {
        self.note("intake");
        None
    }

    fn in_flight(&self) -> usize {
        7
    }
}

/// Packs that cannot be asked: the index fails every call.
struct Unasked;

impl PackReads for Unasked {
    fn read_blobs<'a>(
        &'a self,
        _: &'a [BlobDigest],
    ) -> BoxFuture<'a, Result<Vec<Fetch>, ReadError>> {
        let failed = ReadError::Index(PackIndexError::NotMigrated);
        Box::pin(std::future::ready(Err(failed)))
    }
}

/// Packs that have no blob, answered after a yield, noting how many calls are in flight at once.
#[derive(Default)]
struct Counting {
    now: AtomicUsize,
    peak: AtomicUsize,
}

impl PackReads for Counting {
    fn read_blobs<'a>(
        &'a self,
        digests: &'a [BlobDigest],
    ) -> BoxFuture<'a, Result<Vec<Fetch>, ReadError>> {
        Box::pin(async move {
            let now = self.now.fetch_add(1, Ordering::SeqCst) + 1;
            self.peak.fetch_max(now, Ordering::SeqCst);
            tokio::task::yield_now().await;
            self.now.fetch_sub(1, Ordering::SeqCst);
            Ok(digests.iter().map(|_| Fetch::Missing).collect())
        })
    }
}

/// The plane underneath, an object store and an index, a packer over them, and a
/// [`PackedCas`] whose reader reads them.
struct Rig {
    held: Held,
    store: Arc<MemoryStore>,
    index: Arc<MemoryPackIndex>,
    packer: Packer<MemoryStore>,
    cas: Arc<PackedCas<Held>>,
}

/// Where the pack of a blob that is recorded without being packed is.
const FORGED: &str = "cas/d/node1/boot1/00000000000000ff";

impl Rig {
    fn new() -> Self {
        let caps = Capabilities {
            conditional_put: true,
            object_lock: false,
        };
        let store = Arc::new(MemoryStore::new(caps));
        let index = Arc::new(MemoryPackIndex::new(Arc::new(ManualClock::default())));
        let shared: Arc<dyn PackIndex> = index.clone();
        let keys = PackKeys::new(WriterKind::Daemon, "node1", "boot1").expect("keys");
        let config = PackerConfig::default();
        let packer = Packer::new(Arc::clone(&store), Arc::clone(&shared), keys, config);
        let reader = Reader::new(Arc::clone(&store), shared, None, ReaderConfig::default());
        let held = Held::default();
        let cas = PackedCas::new(Arc::new(held.clone()), Arc::new(reader));
        Self {
            held,
            store,
            index,
            packer: packer.expect("packer"),
            cas: Arc::new(cas),
        }
    }

    /// Packs `bytes`, as a daemon's packer would, and returns their digest.
    async fn pack(&self, bytes: &'static [u8]) -> Digest {
        let blob = kbf_pack::Blob {
            digest: blob_digest(bytes),
            bytes: Bytes::from_static(bytes),
        };
        self.packer.pack(vec![blob]).await.expect("packed");
        digest_of(bytes)
    }

    /// Records a pack that holds `true_bytes` at its start, as the index of a farm would, and
    /// stores `stored` as that pack's bytes if given. The digest of `true_bytes`.
    async fn forge(&self, true_bytes: &'static [u8], stored: Option<&'static [u8]>) -> Digest {
        let pack = NewPack {
            key: FORGED.to_owned(),
            writer: "node1".to_owned(),
            size: true_bytes.len() as u64,
            blobs: vec![PackBlob {
                digest: blob_digest(true_bytes),
                offset: 0,
            }],
        };
        self.index.record_pack(&pack).expect("recorded");
        if let Some(stored) = stored {
            let key = ObjectKey::new(FORGED).expect("a key");
            let body = Bytes::from_static(stored);
            let put = self.store.put_new(&key, body, None).await;
            put.expect("stored");
        }
        digest_of(true_bytes)
    }
}

/// A scratch file in `dir`, created empty and opened for writing.
async fn open(dir: &TestDir, name: &str) -> tokio::fs::File {
    let created = tokio::fs::File::create(dir.join(name)).await;
    created.expect("create")
}

/// Asserts that a blob in a pack is served from it, whole and hashed, to `get` and to `read_to`
/// (written into the file after what the file already holds), and that neither asks the plane
/// underneath: its calls are none.
#[tokio::test]
async fn a_blob_in_a_pack_is_served_from_it() {
    let rig = Rig::new();
    let digest = rig.pack(b"in a pack").await;
    assert_eq!(rig.cas.get(&digest).await, Ok(b"in a pack".to_vec()));

    let dir = TestDir::under(std::env::temp_dir(), "packed-cas-served");
    let mut file = open(&dir, "blob").await;
    file.write_all(b"head").await.expect("head");
    let read = rig.cas.read_to(&digest, &mut file).await;
    read.expect("read_to");
    let written = std::fs::read(dir.join("blob")).expect("read back");
    assert_eq!(written, b"headin a pack");

    assert!(rig.held.calls().is_empty(), "{:?}", rig.held.calls());
    let stats = rig.cas.stats();
    assert_eq!((stats.served, stats.served_bytes), (2, 18));
    let others = stats.elsewhere + stats.unasked + stats.lost + stats.damaged;
    assert_eq!(others, 0);
}

/// Asserts that a blob the index does not have is read from the plane underneath exactly as it
/// would be without packs, by `get` and by `read_to`, and that a blob nowhere is that plane's
/// `Missing`.
#[tokio::test]
async fn a_blob_in_no_pack_is_read_from_the_plane_underneath() {
    let rig = Rig::new();
    let digest = rig.held.insert(b"elsewhere");
    assert_eq!(rig.cas.get(&digest).await, Ok(b"elsewhere".to_vec()));

    let dir = TestDir::under(std::env::temp_dir(), "packed-cas-elsewhere");
    let mut file = open(&dir, "blob").await;
    let read = rig.cas.read_to(&digest, &mut file).await;
    read.expect("read_to");
    let written = std::fs::read(dir.join("blob")).expect("read back");
    assert_eq!(written, b"elsewhere");

    let nowhere = digest_of(b"nowhere");
    let missing = CasError::Missing(label(&nowhere));
    assert_eq!(rig.cas.get(&nowhere).await, Err(missing.clone()));
    assert_eq!(rig.cas.read_to(&nowhere, &mut file).await, Err(missing));
    assert_eq!(rig.held.calls(), ["get", "read_to", "get", "read_to"]);
    let stats = rig.cas.stats();
    assert_eq!((stats.served, stats.elsewhere), (0, 4));
}

/// Asserts that an index row whose pack the store does not hold fails the read as
/// `Unavailable`, for `get` and for `read_to`, without asking the plane underneath, and counts
/// each as lost.
#[tokio::test]
async fn a_lost_pack_fails_the_read_unavailable() {
    let rig = Rig::new();
    let digest = rig.forge(b"gone", None).await;
    let refused = CasError::Unavailable(
        label(&digest),
        "the pack that holds it is gone or shorter than the index says".to_owned(),
    );
    assert_eq!(rig.cas.get(&digest).await, Err(refused.clone()));
    let dir = TestDir::under(std::env::temp_dir(), "packed-cas-lost");
    let mut file = open(&dir, "blob").await;
    assert_eq!(rig.cas.read_to(&digest, &mut file).await, Err(refused));
    assert!(rig.held.calls().is_empty());
    assert_eq!(rig.cas.stats().lost, 2);
}

/// Asserts that bytes in a pack that are not the digest are never served: `get` and `read_to`
/// fail `Corrupt`, naming the blob, without asking the plane underneath, and each is counted as
/// damaged.
#[tokio::test]
async fn damaged_bytes_in_a_pack_fail_the_read_corrupt() {
    let rig = Rig::new();
    let digest = rig.forge(b"true", Some(b"xxxx")).await;
    let refused = CasError::Corrupt(
        label(&digest),
        "another digest, read from its pack".to_owned(),
    );
    assert_eq!(rig.cas.get(&digest).await, Err(refused.clone()));
    let dir = TestDir::under(std::env::temp_dir(), "packed-cas-damaged");
    let mut file = open(&dir, "blob").await;
    assert_eq!(rig.cas.read_to(&digest, &mut file).await, Err(refused));
    assert!(std::fs::read(dir.join("blob")).expect("read").is_empty());
    assert!(rig.held.calls().is_empty());
    assert_eq!(rig.cas.stats().damaged, 2);
}

/// Asserts that a digest that is not SHA-256 (a hash that is not hex, one of another length, a
/// negative size) is not the packs' to answer: the plane underneath is asked, and its answer
/// is the answer.
#[tokio::test]
async fn a_digest_that_is_not_sha256_goes_to_the_plane_underneath() {
    let rig = Rig::new();
    let wrong = |hash: &str, size_bytes: i64| Digest {
        hash: hash.to_owned(),
        size_bytes,
    };
    let sound = "ab".repeat(32);
    for digest in [
        wrong("not hex", 1),
        wrong(&"ab".repeat(31), 1),
        wrong(&sound, -1),
    ] {
        let missing = CasError::Missing(label(&digest));
        assert_eq!(rig.cas.get(&digest).await, Err(missing));
    }
    assert_eq!(rig.held.calls(), ["get", "get", "get"]);
    assert_eq!(rig.cas.stats().elsewhere, 3);
}

/// Asserts that packs that cannot be asked (the index fails) are read around: the plane
/// underneath answers, and the read is counted as unasked, not as a blob in no pack.
#[tokio::test]
async fn packs_that_cannot_be_asked_are_read_around() {
    let held = Held::default();
    let digest = held.insert(b"still there");
    let cas = PackedCas::new(Arc::new(held.clone()), Arc::new(Unasked));
    assert_eq!(cas.get(&digest).await, Ok(b"still there".to_vec()));
    assert_eq!(held.calls(), ["get"]);
    let stats = cas.stats();
    assert_eq!((stats.unasked, stats.elsewhere, stats.served), (1, 0, 0));
}

/// Asserts that a blob of `MAX_PACKED_READ_BYTES` is asked of the packs and one byte more is
/// not: it is read by the plane underneath and counted as read elsewhere, where the blob asked
/// of the packs, which fail, is counted as unasked.
#[tokio::test]
async fn a_blob_too_large_to_hold_is_not_asked_of_the_packs() {
    let held = Held::default();
    let cas = PackedCas::new(Arc::new(held.clone()), Arc::new(Unasked));
    let sized = |size_bytes: i64| Digest {
        hash: "ab".repeat(32),
        size_bytes,
    };
    let at = sized(MAX_PACKED_READ_BYTES);
    assert_eq!(cas.get(&at).await, Err(CasError::Missing(label(&at))));
    let stats = cas.stats();
    assert_eq!((stats.unasked, stats.elsewhere), (1, 0));

    let over = sized(MAX_PACKED_READ_BYTES + 1);
    assert_eq!(cas.get(&over).await, Err(CasError::Missing(label(&over))));
    let stats = cas.stats();
    assert_eq!((stats.unasked, stats.elsewhere), (1, 1));
    assert_eq!(held.calls(), ["get", "get"]);
}

/// Asserts that no more than `MAX_ASKING` reads ask the packs at once, however many are in
/// flight, that the reads kept waiting are all answered, and that the plane underneath answers
/// each of them.
#[tokio::test]
async fn reads_ask_the_packs_a_few_at_a_time() {
    let held = Held::default();
    let digest = held.insert(b"in flight");
    let packs = Arc::new(Counting::default());
    let reads: Arc<dyn PackReads> = packs.clone();
    let cas = PackedCas::new(Arc::new(held), reads);
    let gets = (0..MAX_ASKING * 3).map(|_| cas.get(&digest));
    for read in futures::future::join_all(gets).await {
        assert_eq!(read, Ok(b"in flight".to_vec()));
    }
    assert_eq!(packs.peak.load(Ordering::SeqCst), MAX_ASKING);
    assert_eq!(cas.stats().elsewhere, (MAX_ASKING * 3) as u64);
}

/// Asserts that a file that takes no bytes fails a packed blob's `read_to` as `Write`, naming
/// the blob, and that the plane underneath is not asked.
#[tokio::test]
async fn a_file_that_cannot_be_written_fails_read_to_write() {
    let rig = Rig::new();
    let digest = rig.pack(b"to a closed file").await;
    let dir = TestDir::under(std::env::temp_dir(), "packed-cas-write");
    std::fs::write(dir.join("blob"), b"").expect("create");
    let readonly = std::fs::File::open(dir.join("blob")).expect("open for reading");
    let mut file = tokio::fs::File::from_std(readonly);
    let failed = rig.cas.read_to(&digest, &mut file).await;
    assert!(matches!(failed, Err(CasError::Write(..))), "{failed:?}");
    assert!(format!("{failed:?}").contains(&label(&digest)));
    assert!(rig.held.calls().is_empty());
}

/// Asserts that every call but the two reads is the plane underneath's, unchanged: its answer
/// comes back as it gave it (`missing` is not answered from the index), each call reaches it
/// once, and the number of requests it runs is its own.
#[tokio::test]
async fn everything_but_the_reads_is_the_plane_underneaths() {
    let rig = Rig::new();
    let packed = rig.pack(b"packed").await;
    let absent = digest_of(b"absent");
    let asked = [packed.clone(), absent.clone()];
    assert_eq!(rig.cas.missing(&asked).await, [absent, packed.clone()]);
    let stored = rig.cas.put(b"stored".to_vec()).await.expect("put");
    assert_eq!(stored, digest_of(b"stored"));
    let chunks: Chunks = Box::pin(futures::stream::empty());
    let sent = rig.cas.put_chunks(packed.clone(), chunks).await;
    assert_eq!(sent, Ok(packed));
    rig.cas.put_records(Vec::new()).await.expect("records");
    let dir = TestDir::under(std::env::temp_dir(), "packed-cas-spool");
    let spool = Spool::create(&dir).expect("a spool");
    // Through the wrapper itself: the blanket `Cas for Arc<C>` does not forward `put_spool`.
    let wrapper: &PackedCas<Held> = &rig.cas;
    wrapper.put_spool(Arc::new(spool)).await.expect("spool");
    assert!(rig.cas.intake().is_none());
    assert_eq!(rig.cas.in_flight(), 7);
    let calls = [
        "missing",
        "put",
        "put_chunks",
        "put_records",
        "put_spool",
        "intake",
    ];
    assert_eq!(rig.held.calls(), calls);
}

/// Asserts that a lease reads packs too and counts into the same totals as the plane it was
/// made from, and that making it makes the underneath plane's lease.
#[tokio::test]
async fn a_lease_reads_packs_and_counts_with_its_plane() {
    let rig = Rig::new();
    let digest = rig.pack(b"for a lease").await;
    let lease = rig.cas.lease(&LeaseId::new(1, 2));
    assert_eq!(lease.get(&digest).await, Ok(b"for a lease".to_vec()));
    let elsewhere = rig.held.insert(b"leased elsewhere");
    let leased = lease.get(&elsewhere).await;
    assert_eq!(leased, Ok(b"leased elsewhere".to_vec()));
    assert_eq!(rig.held.calls(), ["lease", "get"]);
    let stats = rig.cas.stats();
    assert_eq!((stats.served, stats.elsewhere), (1, 1));
    assert_eq!(lease.stats(), stats);
}

/// Asserts that a plane made with a counter counts into it, its leases' reads too, and that a
/// clone of the counter reads the same totals; and that a plane made without one has a counter
/// of its own, which reads what that plane counted.
#[tokio::test]
async fn a_plane_counts_into_the_counter_it_is_made_with() {
    let held = Held::default();
    let digest = held.insert(b"counted");
    let counter = PackCounter::default();
    let packs = Arc::new(Counting::default());
    let cas = Arc::new(PackedCas::with_counter(Arc::new(held), packs, &counter));
    assert_eq!(cas.get(&digest).await, Ok(b"counted".to_vec()));
    let lease = cas.lease(&LeaseId::new(1, 2));
    assert_eq!(lease.get(&digest).await, Ok(b"counted".to_vec()));
    assert_eq!(counter.stats().elsewhere, 2);
    assert_eq!(counter.clone().stats(), cas.stats());

    let alone = PackedCas::new(Arc::new(Held::default()), Arc::new(Counting::default()));
    let absent = digest_of(b"absent");
    let missing = CasError::Missing(label(&absent));
    assert_eq!(alone.get(&absent).await, Err(missing));
    assert_eq!(alone.stats().elsewhere, 1);
    assert_eq!(counter.stats().elsewhere, 2);
}

/// Asserts that the wire counts are the plane's counts, each in its own field.
#[test]
fn the_wire_counts_are_the_planes_counts() {
    let stats = PackStats {
        served: 5,
        served_bytes: 4096,
        elsewhere: 2,
        unasked: 1,
        lost: 3,
        damaged: 4,
        writes: WriteStats::default(),
    };
    let want = PackReadCounts {
        served: 5,
        served_bytes: 4096,
        elsewhere: 2,
        unasked: 1,
        lost: 3,
        damaged: 4,
    };
    assert_eq!(PackReadCounts::from(stats), want);
}

/// Asserts that the wire write counts are the plane's write counts, each in its own field: the
/// twelve are told apart by their values.
#[test]
fn the_wire_write_counts_are_the_planes_write_counts() {
    let stats = WriteStats {
        packs: 1,
        packed: 2,
        packed_bytes: 3,
        fallbacks: FallbackStats {
            off: 4,
            unfit: 5,
            unsupported: 6,
            refused: 7,
            store: 8,
            record: 9,
            index: 10,
        },
        missing_indexed: 11,
        missing_unasked: 12,
    };
    let want = PackWriteCounts {
        packs: 1,
        packed: 2,
        packed_bytes: 3,
        fallback_off: 4,
        fallback_unfit: 5,
        fallback_unsupported: 6,
        fallback_refused: 7,
        fallback_store: 8,
        fallback_record: 9,
        fallback_index: 10,
        missing_indexed: 11,
        missing_unasked: 12,
    };
    assert_eq!(PackWriteCounts::from(stats), want);
}

/// Asserts the counts' text, which the plane logs when it is dropped.
#[test]
fn the_counts_are_spelled_for_the_log() {
    let stats = PackStats {
        served: 5,
        served_bytes: 4096,
        elsewhere: 2,
        unasked: 1,
        lost: 3,
        damaged: 4,
        writes: WriteStats::default(),
    };
    assert_eq!(
        stats.to_string(),
        "served=5 served_bytes=4096 elsewhere=2 unasked=1 lost=3 damaged=4"
    );
    assert_eq!(PackStats::default().served, 0);
}
