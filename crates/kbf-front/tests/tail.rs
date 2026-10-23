//! The standing tail of a cache's index ([`Cache::tail_pass`]): objects another member
//! writes to the store after the one-shot rebuild finished are read back after a pass,
//! and what a pass must not do on the way.

use std::num::{NonZeroU64, NonZeroUsize};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::SystemTime;

use bytes::Bytes;
use kbf_front::{
    Cache, CacheError, MemoryMetaLog, MetaLog, MetaLogError, ObjectKeys, RebuildConfig, TailState,
    VerifiedBlob,
};
use kbf_meta::{Applied, Command, NodeWriter, Retention};
use kbf_objstore::footers::FooterReads;
use kbf_objstore::{
    ByteRange, Capabilities, KeyPrefix, ListPage, ListToken, MemoryStore, ObjectKey, ObjectStore,
    ObjectStoreError, PageSize,
};
use kbf_types::Digest;

const ROOT: &str = "kbf/";

/// One bucket for every cache of a test, which counts the listing calls and can be told
/// to fail the n-th.
#[derive(Clone)]
struct Bucket(Arc<Inner>);

struct Inner {
    store: MemoryStore,
    lists: AtomicUsize,
    /// The listing call (counting from 1) that fails; 0 for none.
    fail_list: AtomicUsize,
    /// How ranged reads go: [`READS_OK`], [`READS_FAIL`] or [`READS_GONE`].
    reads: AtomicUsize,
}

const READS_OK: usize = 0;
/// Every ranged read fails with a store error.
const READS_FAIL: usize = 1;
/// Every ranged read finds the object gone.
const READS_GONE: usize = 2;

impl Bucket {
    fn new() -> Self {
        Self(Arc::new(Inner {
            store: MemoryStore::new(Capabilities::default()),
            lists: AtomicUsize::new(0),
            fail_list: AtomicUsize::new(0),
            reads: AtomicUsize::new(READS_OK),
        }))
    }

    fn set_reads(&self, mode: usize) {
        self.0.reads.store(mode, Ordering::SeqCst);
    }

    /// The key of every object, in order.
    async fn keys(&self) -> Vec<ObjectKey> {
        let page = self
            .0
            .store
            .list(
                &KeyPrefix::default(),
                None,
                PageSize::new(1000).expect("size"),
            )
            .await
            .expect("list");
        page.objects.into_iter().map(|o| o.key).collect()
    }

    async fn bytes(&self, key: &ObjectKey) -> Bytes {
        let page = self
            .0
            .store
            .list(
                &KeyPrefix::default(),
                None,
                PageSize::new(1000).expect("size"),
            )
            .await
            .expect("list");
        let size = page
            .objects
            .iter()
            .find(|o| o.key == *key)
            .unwrap_or_else(|| panic!("{key} is gone"))
            .size;
        let range = ByteRange::new(0, size).expect("a range");
        self.0.store.get_range(key, range).await.expect("read")
    }

    async fn put(&self, key: &str, bytes: Bytes) {
        let key = ObjectKey::new(key).expect("key");
        self.0
            .store
            .put_new(&key, bytes, None)
            .await
            .expect("plant");
    }
}

impl ObjectStore for Bucket {
    fn capabilities(&self) -> Capabilities {
        self.0.store.capabilities()
    }

    async fn put_new(
        &self,
        key: &ObjectKey,
        body: Bytes,
        retain_until: Option<SystemTime>,
    ) -> Result<(), ObjectStoreError> {
        self.0.store.put_new(key, body, retain_until).await
    }

    async fn get_range(
        &self,
        key: &ObjectKey,
        range: ByteRange,
    ) -> Result<Bytes, ObjectStoreError> {
        match self.0.reads.load(Ordering::SeqCst) {
            READS_OK => self.0.store.get_range(key, range).await,
            READS_GONE => Err(ObjectStoreError::NotFound(key.clone())),
            _ => Err(ObjectStoreError::Protocol("a scripted read failure".into())),
        }
    }

    async fn delete(&self, key: &ObjectKey) -> Result<(), ObjectStoreError> {
        self.0.store.delete(key).await
    }

    async fn list(
        &self,
        prefix: &KeyPrefix,
        after: Option<&ListToken>,
        max_keys: PageSize,
    ) -> Result<ListPage, ObjectStoreError> {
        let call = self.0.lists.fetch_add(1, Ordering::SeqCst) + 1;
        if self.0.fail_list.load(Ordering::SeqCst) == call {
            return Err(ObjectStoreError::Protocol(
                "a scripted listing failure".into(),
            ));
        }
        self.0.store.list(prefix, after, max_keys).await
    }
}

/// The keys of a cache over the log with id `log`: its epochs under `<root><log id>/`,
/// a node's under the root.
fn keys(log: u64) -> ObjectKeys {
    ObjectKeys {
        legacy: KeyPrefix::new(format!("{ROOT}{log:016x}/")).expect("prefix"),
        root: KeyPrefix::new(ROOT).expect("root"),
    }
}

fn node(member: u64, boot: u64) -> NodeWriter {
    let n = |v| NonZeroU64::new(v).expect("not zero");
    NodeWriter::new(n(member), n(boot))
}

/// The digest and bytes of `n` blobs named `<tag> <i>`.
fn blobs(tag: &str, n: usize) -> Vec<(Digest, Bytes)> {
    (0..n)
        .map(|i| {
            let bytes = Bytes::from(format!("{tag} blob {i}"));
            (kbf_segments::sha256(&bytes), bytes)
        })
        .collect()
}

/// The metadata log of every cache of a test, so every cache is one type
/// (`Cache<Log, Bucket>`). Clones share the log, as successive processes over one
/// directory do;
#[derive(Clone)]
struct Log {
    log: Arc<MemoryMetaLog>,
}

impl Log {
    fn new() -> Self {
        Self {
            log: Arc::new(MemoryMetaLog::new(Retention::default())),
        }
    }
}

impl MetaLog for Log {
    async fn commit(&self, command: Command) -> Result<Applied, MetaLogError> {
        self.log.commit(command).await
    }

    async fn query<R, F>(&self, f: F) -> Result<R, MetaLogError>
    where
        F: FnOnce(&kbf_meta::MetaState) -> R + Send,
        R: Send,
    {
        self.log.query(f).await
    }
}

fn fresh_log() -> Log {
    Log::new()
}

/// Stores `blobs` one segment each through a node writer's cache over a log of its own
/// (which the caller then loses), under `keys`.
async fn write_as_node(
    bucket: &Bucket,
    keys: ObjectKeys,
    writer: NodeWriter,
    blobs: &[(Digest, Bytes)],
) {
    let cache = Cache::open_node(fresh_log(), bucket.clone(), keys, writer)
        .await
        .expect("open as a node");
    for (_, bytes) in blobs {
        let one = VerifiedBlob::hashed(bytes.clone());
        cache.store_blobs(vec![one]).await.expect("store");
    }
}

/// Stores `blobs` one segment each as an old server over a log with id `log` does: an
/// epoch writer, `<root><log id>/cas/<epoch>/<seq>`.
async fn write_as_old_server(bucket: &Bucket, log: u64, blobs: &[(Digest, Bytes)]) {
    let cache = Cache::open_under(fresh_log(), bucket.clone(), keys(log))
        .await
        .expect("open as a server");
    for (_, bytes) in blobs {
        let one = VerifiedBlob::hashed(bytes.clone());
        cache.store_blobs(vec![one]).await.expect("store");
    }
}

/// A new node's cache over a new log (id `log`) and the bucket.
async fn new_node(bucket: &Bucket, log: u64, writer: NodeWriter) -> Cache<Log, Bucket> {
    Cache::open_node(fresh_log(), bucket.clone(), keys(log), writer)
        .await
        .expect("open as a node")
}

/// Every blob reads back byte for byte, and none is reported missing.
async fn assert_holds<M: MetaLog, O: ObjectStore>(cache: &Cache<M, O>, blobs: &[(Digest, Bytes)]) {
    let digests: Vec<Digest> = blobs.iter().map(|(d, _)| *d).collect();
    assert_eq!(cache.find_missing(&digests).await.expect("find"), []);
    for (digest, bytes) in blobs {
        let got = cache.read_blob(digest).await.expect("read");
        assert_eq!(&got, bytes, "{digest}");
    }
}

fn small() -> RebuildConfig {
    RebuildConfig {
        reads: FooterReads {
            concurrency: NonZeroUsize::new(4).expect("not zero"),
            page: PageSize::new(2).expect("in range"),
            tail: FooterReads::default().tail,
        },
        batch: NonZeroUsize::new(2).expect("not zero"),
    }
}

fn reads() -> FooterReads {
    small().reads
}

/// A node over a new log (id 0x33) whose rebuild of the (empty or not) bucket is done.
async fn rebuilt_node(bucket: &Bucket, member: u64) -> Cache<Log, Bucket> {
    let node = new_node(bucket, 0x33, node(member, 1)).await;
    node.rebuild_index(small()).await.expect("rebuild");
    node
}

/// Catches: a node whose index stops at its one-shot rebuild, so a segment another member
/// flushes to the cold store afterwards is not readable here (the join case: a previous
/// owner flushes what it had staged after the joiner rebuilt). Member 7 rebuilds an empty
/// bucket; member 8 then writes 3 blobs. Before a tail pass member 7 reports them missing;
/// after one it reads each byte for byte, and a second pass finds nothing new. Catches:
///  `tail_pass` listing nothing (the blobs stay missing).
#[tokio::test]
async fn a_segment_flushed_by_another_member_after_the_rebuild_is_adopted() {
    let bucket = Bucket::new();
    let node = rebuilt_node(&bucket, 7).await;
    let theirs = blobs("member 8", 3);
    write_as_node(&bucket, keys(0xbb), self::node(8, 1), &theirs).await;
    let digests: Vec<Digest> = theirs.iter().map(|(d, _)| *d).collect();
    assert_eq!(node.find_missing(&digests).await.expect("find"), digests);

    let mut state = TailState::default();
    let report = node.tail_pass(&mut state, reads()).await.expect("pass");
    assert_eq!(
        (report.objects, report.segments, report.inserted),
        (3, 3, 3)
    );
    assert_holds(&node, &theirs).await;

    let again = node.tail_pass(&mut state, reads()).await.expect("pass 2");
    assert!(again.quiet(), "{again:?}");
    assert_eq!(state.ranges(), 1, "three consecutive seqs are one range");
}

/// Catches: a tail that resumes after the last key it listed (one cursor), which never
/// reads a segment that appears below it: uploads finish in no seq order, so seq 2 can
/// land after seq 3 was listed. Member 8 has seqs 1 to 3; seq 2 is held back through the
/// first pass and put after it. The second pass adopts exactly that one. Catches:
///  `TailState::holds` answering true for any seq below a held range's end (seq 2 is
/// never read).
#[tokio::test]
async fn a_segment_that_lands_below_one_already_handled_is_still_found() {
    let bucket = Bucket::new();
    let theirs = blobs("late", 3);
    write_as_node(&bucket, keys(0xbb), node(8, 1), &theirs).await;
    let all = bucket.keys().await;
    assert_eq!(all.len(), 3);
    let held_back = all[1].clone();
    let bytes = bucket.bytes(&held_back).await;
    bucket.0.store.delete(&held_back).await.expect("delete");

    let node = rebuilt_node(&bucket, 7).await;
    let mut state = TailState::default();
    let first = node.tail_pass(&mut state, reads()).await.expect("pass");
    assert_eq!(first.segments, 2);
    bucket.put(held_back.as_str(), bytes).await;
    let second = node.tail_pass(&mut state, reads()).await.expect("pass 2");
    assert_eq!((second.objects, second.segments), (1, 1));
    assert_holds(&node, &theirs).await;
}

/// Catches: a tail that reads this member's own objects (its current boot's, or an earlier
/// boot's from before its log was wiped): they are left to its puts and to the rebuild, so
/// a pass lists them, reads none, and does not index an earlier boot's. Member 7 boot 1
/// wrote 2 blobs under a log it lost; boot 2 passes, and the pass reads nothing, so the
/// blobs are not held (a rebuild would hold them). Catches: no own-member filter
/// (2 objects read and adopted).
#[tokio::test]
async fn the_tail_leaves_its_own_members_objects_alone() {
    let bucket = Bucket::new();
    let earlier = blobs("boot one", 2);
    write_as_node(&bucket, keys(0xaa), node(7, 1), &earlier).await;
    let node = new_node(&bucket, 0x33, self::node(7, 2)).await;
    let mut state = TailState::default();
    let report = node.tail_pass(&mut state, reads()).await.expect("pass");
    assert!(report.quiet(), "{report:?}");
    let digests: Vec<Digest> = earlier.iter().map(|(d, _)| *d).collect();
    assert_eq!(node.find_missing(&digests).await.expect("find"), digests);
    assert_eq!(state.ranges(), 0);
}

/// Catches: a tail that lists outside `<root>cas/`: a segment of a frozen old-layout
/// prefix (`<root><log id>/cas/`, another log's epoch) is not read or adopted, while a
/// segment under `<root>cas/` whose key names no object of the cache is read once,
/// counted foreign, not adopted, and not read again. Catches: listing the root
/// (the frozen segment is adopted and `objects` is 2).
#[tokio::test]
async fn frozen_prefixes_are_ignored_and_a_foreign_key_is_read_once() {
    let bucket = Bucket::new();
    let old = blobs("old log", 1);
    write_as_old_server(&bucket, 0x11, &old).await;
    let frozen = bucket.keys().await.remove(0);
    let segment = bucket.bytes(&frozen).await;
    bucket.put("kbf/cas/notes", segment).await;

    let node = rebuilt_node(&bucket, 7).await;
    let before = node.find_missing(&[old[0].0]).await.expect("find");
    assert_eq!(before, [], "the rebuild adopted the frozen segment");
    let mut state = TailState::default();
    let report = node.tail_pass(&mut state, reads()).await.expect("pass");
    assert_eq!((report.objects, report.foreign, report.segments), (1, 1, 0));
    let again = node.tail_pass(&mut state, reads()).await.expect("pass 2");
    assert!(again.quiet(), "{again:?}");
}

/// Catches: a tail over a cache that names itself as an epoch writer (an old server's
/// layout, `<root>cas/<epoch>/<seq>`) indexing its own puts a second time or reading them
/// at every pass: the pass reads the 2 segments once, every record is kept (indexed
/// already, none inserted), and the second pass reads nothing. Catches:
/// `TailState::insert` forgetting what it is given (the second pass reads both again).
#[tokio::test]
async fn an_epoch_writers_own_objects_add_nothing_and_are_read_once() {
    let bucket = Bucket::new();
    let node = Cache::open_under(
        fresh_log(),
        bucket.clone(),
        ObjectKeys::under(KeyPrefix::new(ROOT).expect("root")),
    )
    .await
    .expect("open as a server");
    let mine = blobs("mine", 2);
    for (_, bytes) in &mine {
        node.store_blobs(vec![VerifiedBlob::hashed(bytes.clone())])
            .await
            .expect("store");
    }
    let mut state = TailState::default();
    let report = node.tail_pass(&mut state, reads()).await.expect("pass");
    assert_eq!((report.segments, report.inserted, report.kept), (2, 0, 2));
    assert!(
        node.tail_pass(&mut state, reads())
            .await
            .expect("pass 2")
            .quiet()
    );
    assert_holds(&node, &mine).await;
}

/// Catches: a failed footer read taken as the object being no segment (a transient error
/// then drops a segment for good): the pass fails, remembers nothing, and the next pass
/// reads and adopts it. Also an object too short to be a segment, and one with a torn tail, are counted skipped and
/// not read again, and one deleted between the listing and the read is counted gone and
/// read again if it comes back. Catches: `SkipReason::Read` counted as skipped
/// (the first pass succeeds without the blobs); `Gone` remembered (the object that comes
/// back is never adopted).
#[tokio::test]
async fn failed_short_and_gone_objects_are_told_apart() {
    let bucket = Bucket::new();
    let theirs = blobs("member 8", 2);
    write_as_node(&bucket, keys(0xbb), node(8, 1), &theirs).await;
    let node = rebuilt_node(&bucket, 7).await;
    let mut state = TailState::default();

    bucket.set_reads(READS_FAIL);
    let failed = node.tail_pass(&mut state, reads()).await.err();
    assert!(matches!(failed, Some(CacheError::Store(_))), "{failed:?}");
    assert_eq!(
        state.ranges(),
        0,
        "nothing is remembered from a failed read"
    );

    bucket.set_reads(READS_GONE);
    let gone = node.tail_pass(&mut state, reads()).await.expect("pass");
    assert_eq!((gone.objects, gone.gone, gone.segments), (2, 2, 0));
    assert_eq!(state.ranges(), 0, "a gone object is not remembered");

    bucket.set_reads(READS_OK);
    bucket
        .put(
            "kbf/cas/0000000000000009/0000000000000001/0000000000000001",
            Bytes::from_static(b"0123456789"),
        )
        .await;
    bucket
        .put(
            "kbf/cas/0000000000000009/0000000000000001/0000000000000002",
            Bytes::from(vec![7_u8; 100]),
        )
        .await;
    let ok = node.tail_pass(&mut state, reads()).await.expect("pass");
    assert_eq!((ok.segments, ok.skipped), (2, 2), "short, and torn");
    assert_holds(&node, &theirs).await;
    let again = node.tail_pass(&mut state, reads()).await.expect("pass");
    assert!(again.quiet(), "{again:?}");
}

/// Catches: a failed listing call ending a pass as if the store were empty (the pass
/// returns `Ok` with nothing adopted, and the caller reports the epoch done): the pass
/// fails with the store's error, and the next one adopts what is there. Catches:
/// a listing error ended the stream quietly (the first pass is `Ok`).
#[tokio::test]
async fn a_failed_listing_fails_the_pass() {
    let bucket = Bucket::new();
    let theirs = blobs("member 8", 3);
    write_as_node(&bucket, keys(0xbb), node(8, 1), &theirs).await;
    let node = rebuilt_node(&bucket, 7).await;
    let calls = bucket.0.lists.load(Ordering::SeqCst);
    bucket.0.fail_list.store(calls + 1, Ordering::SeqCst);
    let mut state = TailState::default();
    let failed = node.tail_pass(&mut state, reads()).await.err();
    assert!(matches!(failed, Some(CacheError::Store(_))), "{failed:?}");
    let ok = node.tail_pass(&mut state, reads()).await.expect("pass");
    assert_eq!(ok.segments, 3);
    assert_holds(&node, &theirs).await;
}
