//! [`PackedCas`] writing packs, over a memory object store that can refuse puts, a memory pack
//! index that can be scripted (and held at the commit), and a plane underneath that notes what
//! it was asked.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, channel};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, SystemTime};

use futures::{FutureExt as _, StreamExt as _};
use kbf_objstore::{
    ByteRange, Capabilities, KeyPrefix, ListPage, ListToken, MemoryStore, ObjectKey, ObjectStore,
    ObjectStoreError, PageSize,
};
use kbf_pack::{PackKeys, ReaderConfig, WriterKind};
use kbf_pack_index::memory::MemoryPackIndex;
use kbf_pack_index::{Location, NewPack, PackIndex, PackIndexError, PackInfo, Recorded};
use kbf_state::{ManualClock, StoreError};
use kbf_testkit::dir::TestDir;
use kbf_transfer::Source;
use tokio::sync::Notify;

use super::*;
use crate::cas::digest_of;

/// A plane that holds blobs in memory and notes the calls made on it, what it was sent, and
/// what it was asked about.
#[derive(Clone, Default)]
struct Under {
    held: Arc<Mutex<BTreeMap<String, Vec<u8>>>>,
    sent: Arc<Mutex<Vec<String>>>,
    asked: Arc<Mutex<Vec<Vec<String>>>>,
    calls: Arc<Mutex<Vec<&'static str>>>,
}

/// Locks `mutex`, whoever panicked with it.
fn locked<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Under {
    fn note(&self, call: &'static str) {
        locked(&self.calls).push(call);
    }

    fn calls(&self) -> Vec<&'static str> {
        locked(&self.calls).clone()
    }

    fn sent(&self) -> Vec<String> {
        locked(&self.sent).clone()
    }

    fn asked(&self) -> Vec<Vec<String>> {
        locked(&self.asked).clone()
    }

    fn hold(&self, bytes: &[u8]) -> Digest {
        let digest = digest_of(bytes);
        locked(&self.held).insert(label(&digest), bytes.to_vec());
        digest
    }

    fn store(&self, digest: &Digest, bytes: Vec<u8>) {
        locked(&self.sent).push(label(digest));
        locked(&self.held).insert(label(digest), bytes);
    }
}

impl Cas for Under {
    type Lease = Under;

    fn lease(self: &Arc<Self>, _: &LeaseId) -> Under {
        Under::clone(self)
    }

    async fn get(&self, digest: &Digest) -> Result<Vec<u8>, CasError> {
        let held = locked(&self.held).get(&label(digest)).cloned();
        held.ok_or_else(|| CasError::Missing(label(digest)))
    }

    async fn put(&self, bytes: Vec<u8>) -> Result<Digest, CasError> {
        self.note("put");
        let digest = digest_of(&bytes);
        self.store(&digest, bytes);
        Ok(digest)
    }

    async fn put_chunks(&self, digest: Digest, mut chunks: Chunks) -> Result<Digest, CasError> {
        self.note("put_chunks");
        let mut bytes = Vec::new();
        while let Some(chunk) = chunks.next().await {
            bytes.extend(chunk.expect("a chunk"));
        }
        self.store(&digest, bytes);
        Ok(digest)
    }

    async fn missing(&self, digests: &[Digest]) -> Vec<Digest> {
        self.note("missing");
        locked(&self.asked).push(digests.iter().map(label).collect());
        let held = locked(&self.held);
        let lacking = digests.iter().filter(|d| !held.contains_key(&label(d)));
        lacking.cloned().collect()
    }

    async fn put_records(&self, records: Vec<Record>) -> Result<(), CasError> {
        self.note("put_records");
        for record in records {
            let digest = kbf_transfer::proto_digest(&record.digest);
            let bytes = match &record.source {
                Source::Bytes(bytes) => bytes.to_vec(),
                _ => Vec::new(),
            };
            self.store(&digest, bytes);
        }
        Ok(())
    }

    async fn put_spool(&self, spool: Arc<Spool>) -> Result<(), CasError> {
        self.note("put_spool");
        for spooled in spool.records() {
            locked(&self.sent).push(label(&kbf_transfer::proto_digest(&spooled.digest)));
        }
        Ok(())
    }
}

/// What the scripted index answers a commit with.
#[derive(Clone, Copy)]
enum Commit {
    Accept,
    /// UNIMPLEMENTED.
    Unsupported,
    /// A refusal a repeat does not change.
    Refuse,
    /// A store failure a repeat may pass.
    Flaky,
}

/// A memory pack index whose commits and lookups can be made to fail, and whose next commit
/// can be held.
struct Index {
    inner: MemoryPackIndex,
    commit: Mutex<Commit>,
    commits: AtomicUsize,
    lookups_fail: AtomicBool,
    gate: Mutex<Option<(Arc<Notify>, Receiver<()>)>>,
}

impl Index {
    fn new() -> Self {
        Self {
            inner: MemoryPackIndex::new(Arc::new(ManualClock::default())),
            commit: Mutex::new(Commit::Accept),
            commits: AtomicUsize::new(0),
            lookups_fail: AtomicBool::new(false),
            gate: Mutex::new(None),
        }
    }

    /// Holds the next commit, after telling `entered`, until `release` is sent to.
    fn hold_next_commit(&self, entered: Arc<Notify>, release: Receiver<()>) {
        *locked(&self.gate) = Some((entered, release));
    }

    fn commits(&self) -> usize {
        self.commits.load(Ordering::SeqCst)
    }

    fn has(&self, digest: &Digest) -> bool {
        let blob = blob_of(digest).expect("a SHA-256 digest");
        let located = self.inner.locate(&[blob]).expect("locate");
        located[0].is_some()
    }
}

impl PackIndex for Index {
    fn record_pack(&self, pack: &NewPack) -> Result<Recorded, PackIndexError> {
        self.commits.fetch_add(1, Ordering::SeqCst);
        let gate = locked(&self.gate).take();
        if let Some((entered, release)) = gate {
            entered.notify_one();
            release.recv().expect("released");
        }
        let commit = *locked(&self.commit);
        match commit {
            Commit::Accept => self.inner.record_pack(pack),
            Commit::Unsupported => Err(PackIndexError::Unsupported),
            Commit::Refuse => Err(PackIndexError::Refused("scripted".to_owned())),
            Commit::Flaky => Err(PackIndexError::Store(StoreError::Failed("down".to_owned()))),
        }
    }

    fn pack(&self, key: &str) -> Result<Option<PackInfo>, PackIndexError> {
        self.inner.pack(key)
    }

    fn locate(&self, digests: &[BlobDigest]) -> Result<Vec<Option<Location>>, PackIndexError> {
        self.inner.locate(digests)
    }

    fn find_missing(&self, digests: &[BlobDigest]) -> Result<Vec<BlobDigest>, PackIndexError> {
        if self.lookups_fail.load(Ordering::SeqCst) {
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

/// A memory object store that can refuse every put, and notes the keys it took.
struct Probe {
    inner: MemoryStore,
    refuse: AtomicBool,
    written: Mutex<Vec<String>>,
}

impl Probe {
    fn new() -> Self {
        let caps = Capabilities {
            conditional_put: true,
            object_lock: false,
        };
        Self {
            inner: MemoryStore::new(caps),
            refuse: AtomicBool::new(false),
            written: Mutex::new(Vec::new()),
        }
    }

    fn written(&self) -> Vec<String> {
        locked(&self.written).clone()
    }
}

impl ObjectStore for Probe {
    fn capabilities(&self) -> Capabilities {
        self.inner.capabilities()
    }

    async fn put_new(
        &self,
        key: &ObjectKey,
        body: Bytes,
        retain_until: Option<SystemTime>,
    ) -> Result<(), ObjectStoreError> {
        if self.refuse.load(Ordering::SeqCst) {
            return Err(ObjectStoreError::Service {
                status: 500,
                code: "InternalError".to_owned(),
                message: "scripted".to_owned(),
            });
        }
        self.inner.put_new(key, body, retain_until).await?;
        locked(&self.written).push(key.to_string());
        Ok(())
    }

    async fn get_range(
        &self,
        key: &ObjectKey,
        range: ByteRange,
    ) -> Result<Bytes, ObjectStoreError> {
        self.inner.get_range(key, range).await
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

/// The plane underneath, an object store and an index, and a [`PackedCas`] that reads and
/// writes packs in them.
struct Rig {
    under: Under,
    store: Arc<Probe>,
    index: Arc<Index>,
    commit: PackCommit,
    cas: Arc<PackedCas<Under>>,
}

impl Rig {
    /// A rig whose frontend takes commits.
    fn new() -> Self {
        Self::with(WriteSettings::default(), true)
    }

    fn with(settings: WriteSettings, welcomed: bool) -> Self {
        let store = Arc::new(Probe::new());
        let index = Arc::new(Index::new());
        let shared: Arc<dyn PackIndex> = index.clone();
        let keys = PackKeys::new(WriterKind::Daemon, "node1", "boot1").expect("keys");
        let writer = PackWriter::new(Arc::clone(&store), Arc::clone(&shared), keys, settings);
        let reader = Reader::new(Arc::clone(&store), shared, None, ReaderConfig::default());
        let under = Under::default();
        let commit = PackCommit::new();
        if welcomed {
            commit.welcomed(true);
        }
        let cas = PackedCas::new(Arc::new(under.clone()), Arc::new(reader))
            .with_writes(Arc::new(writer.expect("writer")), commit.clone());
        Self {
            under,
            store,
            index,
            commit,
            cas: Arc::new(cas),
        }
    }

    fn writes(&self) -> WriteStats {
        self.cas.stats().writes
    }

    /// Asserts that `bytes` are in a committed pack and that the plane reads them from it, not
    /// from the plane underneath.
    async fn assert_in_a_pack(&self, bytes: &[u8]) {
        let digest = digest_of(bytes);
        assert!(self.index.has(&digest), "indexed");
        let before = self.cas.stats().served;
        assert_eq!(self.cas.get(&digest).await, Ok(bytes.to_vec()));
        assert_eq!(self.cas.stats().served, before + 1, "read from its pack");
    }
}

/// Chunks of a blob.
fn chunks(parts: &[&[u8]]) -> Chunks {
    let parts: Vec<std::io::Result<Vec<u8>>> = parts.iter().map(|p| Ok(p.to_vec())).collect();
    Box::pin(futures::stream::iter(parts))
}

/// A record of `bytes` whose digest names `named`.
fn record_of(named: &[u8], bytes: &'static [u8]) -> Record {
    Record {
        digest: kbf_segments::sha256(named),
        crc32c: 0,
        source: Source::Bytes(Bytes::from_static(bytes)),
    }
}

/// Asserts that a `put` is in a committed pack when it returns, that the plane underneath was
/// not asked to write it, that the pack is under the daemon's prefix, and that the blob reads
/// back through the plane from its pack.
#[tokio::test]
async fn a_put_is_committed_in_a_pack_and_reads_back_from_it() {
    let rig = Rig::new();
    let digest = rig.cas.put(b"output one".to_vec()).await.expect("put");
    assert_eq!(digest, digest_of(b"output one"));
    let written = rig.store.written();
    assert_eq!(written.len(), 1);
    assert!(written[0].starts_with("cas/d/node1/boot1/"), "{written:?}");
    assert_eq!(rig.index.commits(), 1);
    rig.assert_in_a_pack(b"output one").await;
    assert_eq!(rig.under.calls(), Vec::<&str>::new());
    let stats = rig.writes();
    assert_eq!((stats.packs, stats.packed), (1, 1));
    assert!(stats.packed_bytes > 0);
    assert_eq!(stats.fallbacks, FallbackStats::default());
}

/// Asserts that a `put_chunks` gathers its chunks into a pack, as `put` does, and returns the
/// digest it was given.
#[tokio::test]
async fn put_chunks_is_committed_in_a_pack() {
    let rig = Rig::new();
    let digest = digest_of(b"abcd");
    let stored = rig
        .cas
        .put_chunks(digest.clone(), chunks(&[b"ab", b"cd"]))
        .await;
    assert_eq!(stored, Ok(digest));
    rig.assert_in_a_pack(b"abcd").await;
    assert_eq!(rig.under.calls(), Vec::<&str>::new());
    assert_eq!(rig.writes().packs, 1);
}

/// Asserts that the records of one `put_records` are packed together (one pack, one commit),
/// and read back through the plane.
#[tokio::test]
async fn put_records_packs_its_blobs_together() {
    let rig = Rig::new();
    let records = vec![Record::bytes(b"r1".to_vec()), Record::bytes(b"r2".to_vec())];
    rig.cas.put_records(records).await.expect("records");
    assert_eq!(rig.index.commits(), 1);
    rig.assert_in_a_pack(b"r1").await;
    rig.assert_in_a_pack(b"r2").await;
    assert_eq!(rig.under.calls(), Vec::<&str>::new());
    let stats = rig.writes();
    assert_eq!((stats.packs, stats.packed), (1, 2));
}

/// Asserts that a spool's blobs are packed, not handed to the plane underneath's `put_spool`.
#[tokio::test]
async fn put_spool_packs_the_blobs_of_the_spool() {
    let rig = Rig::new();
    let dir = TestDir::under(std::env::temp_dir(), "packed-cas-write-spool");
    let spool = Arc::new(Spool::create(&dir).expect("a spool"));
    spool.put(b"spooled".to_vec()).await.expect("spooled");
    let wrapper: &PackedCas<Under> = &rig.cas;
    wrapper.put_spool(spool).await.expect("put_spool");
    rig.assert_in_a_pack(b"spooled").await;
    assert_eq!(rig.under.calls(), Vec::<&str>::new());
    assert_eq!(rig.writes().packed, 1);
}

/// Asserts that a lease of the plane writes packs too and counts into the totals of the plane
/// it was made from.
#[tokio::test]
async fn a_lease_writes_packs_and_counts_with_its_plane() {
    let rig = Rig::new();
    let lease = rig.cas.lease(&LeaseId::new(1, 2));
    lease.put(b"leased".to_vec()).await.expect("put");
    rig.assert_in_a_pack(b"leased").await;
    assert_eq!(rig.writes().packs, 1);
    assert_eq!(lease.stats(), rig.cas.stats());
}

/// Asserts that a put does not return before its pack is committed: with the commit held, the
/// pack is in the object store and not in the index and the put is pending; once the commit is
/// released the put returns and the blob is indexed. (No clock: the test waits for the commit to
/// begin, which the put cannot pass.)
#[tokio::test]
async fn a_put_does_not_return_before_its_commit() {
    let rig = Rig::new();
    let entered = Arc::new(Notify::new());
    let (release, hold) = channel();
    rig.index.hold_next_commit(Arc::clone(&entered), hold);
    let cas = Arc::clone(&rig.cas);
    let put = tokio::spawn(async move { cas.put(b"gated".to_vec()).await });
    entered.notified().await;
    assert!(!put.is_finished(), "the put returned before the commit");
    assert_eq!(rig.store.written().len(), 1, "the pack is stored");
    assert!(!rig.index.has(&digest_of(b"gated")), "and not yet indexed");
    release.send(()).expect("the commit is waiting");
    assert_eq!(put.await.expect("joined"), Ok(digest_of(b"gated")));
    assert!(rig.index.has(&digest_of(b"gated")));
}

/// A rig with an in-flight budget of 16 bytes.
fn tight_rig() -> Rig {
    let settings = WriteSettings {
        inflight_bytes: 16,
        ..WriteSettings::default()
    };
    Rig::with(settings, true)
}

/// Whether the whole in-flight budget (16 bytes) can be taken now.
fn budget_is_free(rig: &Rig) -> bool {
    let writes = rig.cas.writes.as_ref().expect("a plane that writes");
    writes
        .room(16)
        .now_or_never()
        .is_some_and(|room| room.is_some())
}

/// Asserts that the in-flight budget is given back when a put is done, and that a blob exactly
/// the size of the budget fits it: with a budget of 16 bytes, two puts of 16 bytes in a row are
/// both committed in packs (a put that kept its bytes would leave the second waiting for ever),
/// and the whole budget is free after each.
#[tokio::test]
async fn the_budget_is_given_back_when_a_put_is_done() {
    let rig = tight_rig();
    rig.cas.put(vec![1; 16]).await.expect("first");
    assert!(budget_is_free(&rig), "given back after the first put");
    rig.cas.put(vec![2; 16]).await.expect("second");
    assert!(budget_is_free(&rig), "given back after the second put");
    assert_eq!(rig.index.commits(), 2);
    assert_eq!(rig.under.calls(), Vec::<&str>::new());
    assert_eq!(rig.writes().fallbacks.unfit, 0);
    rig.assert_in_a_pack(&[2; 16]).await;
}

/// Asserts that a put holds its bytes of the budget until its commit is done, and that a put
/// behind it waits for them: with the first commit held, the budget cannot be taken and the
/// second put has stored no pack and sent no commit; once the commit is released both puts are
/// committed in packs of their own and the budget is free. (No clock: the held commit is the
/// only thing the first put waits for.)
#[tokio::test]
async fn a_put_waits_for_the_puts_ahead_to_give_bytes_back() {
    let rig = tight_rig();
    let entered = Arc::new(Notify::new());
    let (release, hold) = channel();
    rig.index.hold_next_commit(Arc::clone(&entered), hold);
    let cas = Arc::clone(&rig.cas);
    let first = tokio::spawn(async move { cas.put(vec![1; 16]).await });
    entered.notified().await;
    assert!(!budget_is_free(&rig), "held while the commit is");
    let cas = Arc::clone(&rig.cas);
    let second = tokio::spawn(async move { cas.put(vec![2; 16]).await });
    for _ in 0..32 {
        tokio::task::yield_now().await;
    }
    assert!(!second.is_finished(), "the second put went ahead");
    assert_eq!(
        rig.store.written().len(),
        1,
        "only the first pack is stored"
    );
    assert_eq!(rig.index.commits(), 1, "and only the first is committed");
    release.send(()).expect("the commit is waiting");
    first.await.expect("joined").expect("first");
    second.await.expect("joined").expect("second");
    assert_eq!(rig.index.commits(), 2);
    assert_eq!(rig.writes().packs, 2);
    assert!(budget_is_free(&rig));
}

/// Asserts the layering of `missing` when the plane writes packs: the plane underneath is asked
/// only about the digests the index lacks, not at all when the index has every one, and a digest
/// that is not SHA-256 (which the index cannot hold) goes to it too; the answer is the plane
/// underneath's, so a blob only it holds is present, and the digests are counted as answered
/// by the index.
#[tokio::test]
async fn missing_asks_the_index_first_and_the_plane_for_what_it_lacks() {
    let rig = Rig::new();
    let in_index = rig.cas.put(b"in the index".to_vec()).await.expect("put");
    let nowhere = digest_of(b"nowhere");
    let missing = rig.cas.missing(&[in_index.clone(), nowhere.clone()]).await;
    assert_eq!(missing, std::slice::from_ref(&nowhere));
    assert_eq!(rig.under.asked(), [[label(&nowhere)]]);

    let only_under = rig.under.hold(b"only underneath");
    let missing = rig.cas.missing(std::slice::from_ref(&only_under)).await;
    assert_eq!(missing, Vec::<Digest>::new());
    assert_eq!(rig.under.asked().len(), 2);

    let all_indexed = rig.cas.missing(std::slice::from_ref(&in_index)).await;
    assert_eq!(all_indexed, Vec::<Digest>::new());
    assert_eq!(rig.under.asked().len(), 2, "the plane was not asked");

    let odd = Digest {
        hash: "not hex".to_owned(),
        size_bytes: 1,
    };
    let missing = rig.cas.missing(&[in_index, odd.clone()]).await;
    assert_eq!(missing, std::slice::from_ref(&odd));
    assert_eq!(rig.under.asked()[2], [label(&odd)]);

    let stats = rig.writes();
    assert_eq!((stats.missing_indexed, stats.missing_unasked), (5, 0));
}

/// Asserts that when the index cannot answer, `missing` asks the plane underneath about every
/// digest, in the order given, and counts them as unasked; nothing is reported present on the
/// index's word.
#[tokio::test]
async fn missing_asks_the_plane_for_everything_when_the_index_fails() {
    let rig = Rig::new();
    let in_index = rig.cas.put(b"in the index".to_vec()).await.expect("put");
    let nowhere = digest_of(b"nowhere");
    rig.index.lookups_fail.store(true, Ordering::SeqCst);
    let asked = [in_index.clone(), nowhere.clone()];
    assert_eq!(rig.cas.missing(&asked).await, asked);
    assert_eq!(rig.under.asked(), [[label(&in_index), label(&nowhere)]]);
    let stats = rig.writes();
    assert_eq!((stats.missing_indexed, stats.missing_unasked), (0, 2));
}

/// Asserts that a plane whose frontend did not say it takes commits writes no pack: the plane
/// underneath writes the blobs (every call of the four), `missing` is its own answer to the
/// digests as asked, and each blob is counted as written elsewhere because the capability is
/// off.
#[tokio::test]
async fn without_the_capability_everything_goes_to_the_plane_underneath() {
    let rig = Rig::with(WriteSettings::default(), false);
    let put = rig.cas.put(b"one".to_vec()).await.expect("put");
    assert_eq!(put, digest_of(b"one"));
    let digest = digest_of(b"abcd");
    rig.cas
        .put_chunks(digest, chunks(&[b"abcd"]))
        .await
        .expect("chunks");
    let records = vec![Record::bytes(b"r1".to_vec()), Record::bytes(b"r2".to_vec())];
    rig.cas.put_records(records).await.expect("records");
    let dir = TestDir::under(std::env::temp_dir(), "packed-cas-write-off");
    let spool = Arc::new(Spool::create(&dir).expect("a spool"));
    spool.put(b"spooled".to_vec()).await.expect("spooled");
    let wrapper: &PackedCas<Under> = &rig.cas;
    wrapper.put_spool(spool).await.expect("put_spool");
    assert_eq!(
        rig.under.calls(),
        ["put", "put_chunks", "put_records", "put_spool"]
    );
    assert_eq!(rig.store.written(), Vec::<String>::new());
    assert_eq!(rig.index.commits(), 0);
    let asked = [digest_of(b"one"), digest_of(b"nowhere")];
    assert_eq!(rig.cas.missing(&asked).await, [digest_of(b"nowhere")]);
    assert_eq!(rig.under.asked(), [[label(&asked[0]), label(&asked[1])]]);
    let stats = rig.writes();
    assert_eq!(stats.fallbacks.off, 1 + 1 + 2 + 1);
    assert_eq!((stats.packs, stats.missing_indexed), (0, 0));
}

/// Asserts that a handle that is on can be turned off by a later `Welcome` that does not say
/// it (another frontend), and then packs are not written.
#[tokio::test]
async fn a_welcome_that_does_not_say_it_turns_writes_off() {
    let rig = Rig::new();
    rig.commit.welcomed(false);
    rig.cas.put(b"after".to_vec()).await.expect("put");
    assert_eq!(rig.under.calls(), ["put"]);
    assert_eq!(rig.store.written(), Vec::<String>::new());
}

/// Asserts that a commit answered UNIMPLEMENTED writes the blobs to the plane underneath, that
/// the put returns its result, that packing is off from then on (the next put is the plane
/// underneath's, with no commit sent and no pack written, and `missing` is its own), and that a
/// later `Welcome` saying the frontend takes commits does not turn it back on. The pack the
/// first put stored is left, unindexed.
#[tokio::test]
async fn an_unimplemented_commit_turns_packing_off_for_the_process() {
    let rig = Rig::new();
    *locked(&rig.index.commit) = Commit::Unsupported;
    let first = rig.cas.put(b"first".to_vec()).await.expect("put");
    assert_eq!(first, digest_of(b"first"));
    assert_eq!(rig.under.calls(), ["put"]);
    assert!(!rig.commit.enabled());
    assert_eq!(rig.store.written().len(), 1, "the pack is left behind");
    assert!(!rig.index.has(&first), "unindexed");
    assert_eq!(rig.index.commits(), 1, "no repeat of a refused call");

    rig.cas.put(b"second".to_vec()).await.expect("put");
    assert_eq!(rig.under.calls(), ["put", "put"]);
    assert_eq!(rig.index.commits(), 1);
    assert_eq!(rig.store.written().len(), 1);
    rig.commit.welcomed(true);
    assert!(!rig.commit.enabled());
    let asked = [digest_of(b"first")];
    assert_eq!(rig.cas.missing(&asked).await, Vec::<Digest>::new());
    assert_eq!(rig.under.asked(), [[label(&asked[0])]]);

    let stats = rig.writes();
    assert_eq!((stats.fallbacks.unsupported, stats.fallbacks.off), (1, 1));
    assert_eq!(stats.packs, 0);
}

/// Asserts that a commit refused for a reason a repeat does not change is not repeated, and
/// that the blobs of the put are written to the plane underneath.
#[tokio::test]
async fn a_refused_commit_writes_the_blobs_to_the_plane_underneath() {
    let rig = Rig::new();
    *locked(&rig.index.commit) = Commit::Refuse;
    let records = vec![Record::bytes(b"r1".to_vec()), Record::bytes(b"r2".to_vec())];
    rig.cas.put_records(records).await.expect("records");
    assert_eq!(rig.index.commits(), 1);
    assert_eq!(rig.under.calls(), ["put_records"]);
    assert_eq!(rig.under.sent().len(), 2);
    assert!(rig.commit.enabled(), "a refusal does not turn packing off");
    let stats = rig.writes();
    assert_eq!((stats.fallbacks.refused, stats.packs), (2, 0));
}

/// Asserts that a commit that keeps failing is tried as often as the packer is set to, and
/// then the blobs go to the plane underneath, for each of two puts in a row.
#[tokio::test]
async fn a_commit_that_keeps_failing_writes_the_blobs_to_the_plane_underneath() {
    let rig = Rig::new();
    *locked(&rig.index.commit) = Commit::Flaky;
    rig.cas.put(b"one".to_vec()).await.expect("put");
    rig.cas.put(b"two".to_vec()).await.expect("put");
    assert_eq!(rig.index.commits(), 6, "three attempts a put");
    assert_eq!(rig.under.calls(), ["put", "put"]);
    assert_eq!(rig.writes().fallbacks.record, 2);
}

/// Asserts that an object store that refuses the put of the pack leaves no pack, sends no
/// commit, and has the blobs written to the plane underneath, for each of two puts in a row:
/// a `put`, and a `put_chunks` whose gathered bytes go to the plane underneath as one `put`.
#[tokio::test]
async fn a_failed_object_store_put_writes_the_blobs_to_the_plane_underneath() {
    let rig = Rig::new();
    rig.store.refuse.store(true, Ordering::SeqCst);
    rig.cas.put(b"one".to_vec()).await.expect("put");
    let two = digest_of(b"two");
    let sent = rig
        .cas
        .put_chunks(two.clone(), chunks(&[b"t", b"wo"]))
        .await;
    assert_eq!(sent.expect("put_chunks"), two);
    assert_eq!(rig.store.written(), Vec::<String>::new());
    assert_eq!(rig.index.commits(), 0);
    assert_eq!(rig.under.calls(), ["put", "put"]);
    assert_eq!(rig.writes().fallbacks.store, 2);
}

/// Asserts that an index that cannot say which blobs it lacks, when the packer asks before it
/// writes, writes nothing to the object store and has the blob written to the plane
/// underneath.
#[tokio::test]
async fn an_index_that_cannot_be_asked_writes_the_blobs_to_the_plane_underneath() {
    let rig = Rig::new();
    rig.index.lookups_fail.store(true, Ordering::SeqCst);
    rig.cas.put(b"one".to_vec()).await.expect("put");
    assert_eq!(rig.store.written(), Vec::<String>::new());
    assert_eq!(rig.under.calls(), ["put"]);
    assert_eq!(rig.writes().fallbacks.index, 1);
}

/// Asserts that a blob over `MAX_PACKED_READ_BYTES`, a digest that is not SHA-256 and a blob
/// over the in-flight budget go to the plane underneath, whole and unpacked, and that a record
/// over the limit goes to it after the records that are packed.
#[tokio::test]
async fn blobs_a_pack_does_not_take_go_to_the_plane_underneath() {
    let rig = Rig::new();
    let large = Digest {
        hash: hex::encode([7_u8; 32]),
        size_bytes: MAX_PACKED_READ_BYTES + 1,
    };
    let sent = rig.cas.put_chunks(large.clone(), chunks(&[])).await;
    assert_eq!(sent, Ok(large.clone()));
    let odd = Digest {
        hash: "not hex".to_owned(),
        size_bytes: 3,
    };
    let sent = rig.cas.put_chunks(odd.clone(), chunks(&[b"odd"])).await;
    assert_eq!(sent, Ok(odd));
    assert_eq!(rig.under.calls(), ["put_chunks", "put_chunks"]);
    assert_eq!(rig.store.written(), Vec::<String>::new());

    let big = Record {
        digest: kbf_types::Digest::new(
            kbf_types::DigestFunction::Sha256,
            [7; 32],
            u64::try_from(MAX_PACKED_READ_BYTES).expect("positive") + 1,
        ),
        crc32c: 0,
        source: Source::Bytes(Bytes::new()),
    };
    let records = vec![big, Record::bytes(b"small".to_vec())];
    rig.cas.put_records(records).await.expect("records");
    rig.assert_in_a_pack(b"small").await;
    assert_eq!(rig.under.calls()[2..], ["put_records"]);
    assert_eq!(rig.under.sent().last(), Some(&label(&large)));
    assert_eq!(rig.writes().fallbacks.unfit, 3);

    let tight = WriteSettings {
        inflight_bytes: 8,
        ..WriteSettings::default()
    };
    let rig = Rig::with(tight, true);
    rig.cas.put(vec![9; 16]).await.expect("put");
    rig.cas
        .put_records(vec![Record::bytes(vec![8; 16])])
        .await
        .expect("records");
    assert_eq!(rig.under.calls(), ["put", "put_records"]);
    assert_eq!(rig.store.written(), Vec::<String>::new());
    assert_eq!(rig.writes().fallbacks.unfit, 2);
}

/// Asserts that `put_records` reads and packs its records in groups of about one pack, each
/// group committed (a commit of its own) before the next is read.
#[tokio::test]
async fn put_records_packs_in_groups_of_a_pack() {
    let small = WriteSettings {
        pack_bytes: 8,
        ..WriteSettings::default()
    };
    let rig = Rig::with(small, true);
    let records = vec![
        Record::bytes(b"aaaaaa".to_vec()),
        Record::bytes(b"bbbbbb".to_vec()),
        Record::bytes(b"cccccc".to_vec()),
    ];
    rig.cas.put_records(records).await.expect("records");
    assert_eq!(rig.index.commits(), 3);
    assert_eq!(rig.writes().packs, 3);
    rig.assert_in_a_pack(b"bbbbbb").await;
}

/// Asserts how records are cut into groups: a record that would take a group over the limit
/// starts the next, a record over the limit is a group of its own, and nothing is a group of
/// nothing.
#[test]
fn records_are_grouped_up_to_the_limit() {
    let records = |sizes: &[usize]| -> Vec<Record> {
        sizes.iter().map(|n| Record::bytes(vec![1; *n])).collect()
    };
    let lens = |groups: Vec<Vec<Record>>| -> Vec<usize> { groups.iter().map(Vec::len).collect() };
    assert_eq!(lens(groups(records(&[6, 6, 6]), 12)), [2, 1]);
    assert_eq!(lens(groups(records(&[6, 6, 6]), 5)), [1, 1, 1]);
    assert_eq!(lens(groups(records(&[1, 20, 1]), 10)), [1, 1, 1]);
    assert_eq!(lens(groups(records(&[3, 3, 3]), 100)), [3]);
    assert_eq!(lens(groups(Vec::new(), 10)), Vec::<usize>::new());
}

/// Asserts that bytes that are not the digest named for them are `Corrupt`, not written to the
/// plane underneath: chunks that differ, that are shorter, that are longer, and a record whose
/// bytes are not its digest; and that a chunk that cannot be read is `Read`, and a record whose
/// bytes cannot be read is `Read`. None is counted as a fallback.
#[tokio::test]
async fn bytes_that_are_not_their_digest_are_refused() {
    let rig = Rig::new();
    let digest = digest_of(b"abcd");
    let corrupt = |result: Result<Digest, CasError>| {
        assert!(matches!(result, Err(CasError::Corrupt(..))), "{result:?}");
    };
    corrupt(rig.cas.put_chunks(digest.clone(), chunks(&[b"abXd"])).await);
    corrupt(rig.cas.put_chunks(digest.clone(), chunks(&[b"abc"])).await);
    corrupt(
        rig.cas
            .put_chunks(digest.clone(), chunks(&[b"abcd", b"e"]))
            .await,
    );
    let broken: Chunks = Box::pin(futures::stream::iter(vec![Err(std::io::Error::other(
        "gone",
    ))]));
    let result = rig.cas.put_chunks(digest, broken).await;
    assert!(matches!(result, Err(CasError::Read(..))), "{result:?}");

    let wrong = rig.cas.put_records(vec![record_of(b"abcd", b"abXd")]).await;
    assert!(matches!(wrong, Err(CasError::Corrupt(..))), "{wrong:?}");
    let short = rig
        .cas
        .put_records(vec![record_of(b"0123456789", b"abc")])
        .await;
    assert!(matches!(short, Err(CasError::Read(..))), "{short:?}");

    assert_eq!(rig.under.calls(), Vec::<&str>::new());
    assert_eq!(rig.store.written(), Vec::<String>::new());
    let stats = rig.writes();
    assert_eq!(
        (stats.packs, stats.fallbacks),
        (0, FallbackStats::default())
    );
}

/// Asserts that `put_chunks` of bytes that are not the digest named for them is `Corrupt` when
/// the packer does not hash them either: the index cannot be asked which blobs it lacks (the
/// packer fails before it hashes anything) and the index has the digest named already (the
/// packer skips the blob). The plane underneath is not written, and nothing is returned for a
/// digest the bytes do not have.
#[tokio::test]
async fn chunks_that_are_not_their_digest_are_refused_whatever_the_index_says() {
    let rig = Rig::new();
    let digest = digest_of(b"abcd");
    rig.index.lookups_fail.store(true, Ordering::SeqCst);
    let result = rig.cas.put_chunks(digest.clone(), chunks(&[b"abXd"])).await;
    assert!(matches!(result, Err(CasError::Corrupt(..))), "{result:?}");

    rig.index.lookups_fail.store(false, Ordering::SeqCst);
    rig.cas.put(b"abcd".to_vec()).await.expect("put");
    assert!(rig.index.has(&digest));
    let result = rig.cas.put_chunks(digest, chunks(&[b"abXd"])).await;
    assert!(matches!(result, Err(CasError::Corrupt(..))), "{result:?}");
    assert_eq!(rig.under.calls(), Vec::<&str>::new());
    assert_eq!(rig.index.commits(), 1);
    assert!(budget_is_free(&rig), "the room is given back");
}

/// Asserts the handle's transitions: off until a `Welcome`
/// says the frontend takes commits, off again when one does not, and off for good once a commit
/// was answered UNIMPLEMENTED.
#[test]
fn the_commit_handle_follows_the_welcome_until_unimplemented() {
    let commit = PackCommit::new();
    assert!(!commit.enabled());
    commit.welcomed(true);
    assert!(commit.clone().enabled(), "a clone is the same handle");
    commit.welcomed(false);
    assert!(!commit.enabled());
    commit.welcomed(true);
    commit.unsupported();
    commit.unsupported();
    assert!(!commit.enabled());
    commit.welcomed(true);
    assert!(!commit.enabled());
    commit.welcomed(false);
    assert!(!commit.enabled());
}

/// Asserts the write counts' text, which the plane logs when it is dropped, and the defaults of
/// the writer's settings.
#[test]
fn the_write_counts_are_spelled_for_the_log() {
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
    assert_eq!(
        stats.to_string(),
        "packs=1 packed=2 packed_bytes=3 fallback_off=4 fallback_unfit=5 \
         fallback_unsupported=6 fallback_refused=7 fallback_store=8 fallback_record=9 \
         fallback_index=10 missing_indexed=11 missing_unasked=12"
    );
    let settings = WriteSettings::default();
    assert_eq!(settings.inflight_bytes, DEFAULT_WRITE_INFLIGHT_BYTES);
    assert_eq!(settings.pack_bytes, kbf_pack::FRONTEND_PACK_BYTES);
}
