//! The input check ([`Cache::missing_inputs`]): decoded directories and verified
//! subtrees remembered in memory, single flight between requests, bounded reads and a
//! bounded cache, and the retention rule that makes skipping a subtree safe.
//!
//! Directory reads are counted at the object store, not inferred from the cache's own
//! counters, so a test of "reads nothing" cannot pass because a counter was not bumped.

mod common;

use std::mem::size_of;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use bytes::Bytes;
use common::{Blob, DAY, days};
use kbf_front::{Cache, CacheError, ENTRY_OVERHEAD, InputTreeConfig, MemoryMetaLog, VerifiedBlob};
use kbf_meta::Retention;
use kbf_objstore::{
    ByteRange, Capabilities, KeyPrefix, ListPage, ListToken, MemoryStore, ObjectKey, ObjectStore,
    ObjectStoreError, PageSize,
};
use kbf_proto::reapi::{Directory, DirectoryNode, FileNode};
use kbf_types::{Digest, FarmTime};

/// A memory store that counts its reads and the most that were ever in flight, and
/// yields while a read is in flight so concurrent callers interleave.
#[derive(Clone)]
struct Counting(Arc<Counts>);

#[derive(Default)]
struct Counts {
    inner: Option<MemoryStore>,
    gets: AtomicUsize,
    active: AtomicUsize,
    peak: AtomicUsize,
}

impl Counting {
    fn new() -> Self {
        Self(Arc::new(Counts {
            inner: Some(MemoryStore::new(Capabilities::default())),
            ..Counts::default()
        }))
    }

    fn gets(&self) -> usize {
        self.0.gets.load(Ordering::SeqCst)
    }

    /// Forgets the reads so far (an upload reads its segment's footer back).
    fn reset(&self) {
        self.0.gets.store(0, Ordering::SeqCst);
        self.0.peak.store(0, Ordering::SeqCst);
    }

    fn peak(&self) -> usize {
        self.0.peak.load(Ordering::SeqCst)
    }

    fn store(&self) -> &MemoryStore {
        self.0.inner.as_ref().expect("a store")
    }
}

impl ObjectStore for Counting {
    fn capabilities(&self) -> Capabilities {
        self.store().capabilities()
    }

    async fn put_new(
        &self,
        key: &ObjectKey,
        body: Bytes,
        retain_until: Option<std::time::SystemTime>,
    ) -> Result<(), ObjectStoreError> {
        self.store().put_new(key, body, retain_until).await
    }

    async fn get_range(
        &self,
        key: &ObjectKey,
        range: ByteRange,
    ) -> Result<Bytes, ObjectStoreError> {
        self.0.gets.fetch_add(1, Ordering::SeqCst);
        let now = self.0.active.fetch_add(1, Ordering::SeqCst) + 1;
        self.0.peak.fetch_max(now, Ordering::SeqCst);
        for _ in 0..4 {
            tokio::task::yield_now().await;
        }
        let bytes = self.store().get_range(key, range).await;
        self.0.active.fetch_sub(1, Ordering::SeqCst);
        bytes
    }

    async fn delete(&self, key: &ObjectKey) -> Result<(), ObjectStoreError> {
        self.store().delete(key).await
    }

    async fn list(
        &self,
        prefix: &KeyPrefix,
        after: Option<&ListToken>,
        max_keys: PageSize,
    ) -> Result<ListPage, ObjectStoreError> {
        self.store().list(prefix, after, max_keys).await
    }
}

type TestCache = Cache<MemoryMetaLog, Counting>;

async fn cache_with(config: InputTreeConfig) -> (Arc<TestCache>, Counting) {
    let store = Counting::new();
    let cache = Cache::open(
        MemoryMetaLog::new(Retention::default()),
        store.clone(),
        KeyPrefix::default(),
    )
    .await
    .expect("open the cache")
    .with_input_trees(config);
    (Arc::new(cache), store)
}

async fn cache() -> (Arc<TestCache>, Counting) {
    cache_with(InputTreeConfig::DEFAULT).await
}

async fn put(cache: &TestCache, blobs: &[&Blob]) {
    let blobs = blobs
        .iter()
        .map(|b| VerifiedBlob::new(b.digest, Bytes::from(b.data.clone())).expect("verified"))
        .collect();
    cache.store_blobs(blobs).await.expect("stored");
}

fn file(name: &str) -> Blob {
    Blob::new(format!("contents of {name}"))
}

/// A directory holding `files` (named by their position) and `dirs`.
fn dir(files: &[&Blob], dirs: &[&Blob]) -> Blob {
    Blob::of(&Directory {
        files: files
            .iter()
            .enumerate()
            .map(|(i, f)| FileNode {
                name: format!("f{i}"),
                digest: Some(f.proto.clone()),
                ..FileNode::default()
            })
            .collect(),
        directories: dirs
            .iter()
            .enumerate()
            .map(|(i, d)| DirectoryNode {
                name: format!("d{i}"),
                digest: Some(d.proto.clone()),
            })
            .collect(),
        ..Directory::default()
    })
}

/// A toolchain of three directories (`tc` over `bin` and `lib`) with a file each, and
/// every blob in it.
struct Toolchain {
    tc: Blob,
    blobs: Vec<Blob>,
}

fn toolchain() -> Toolchain {
    let (compiler, library, header) = (file("cc"), file("libc"), file("stdio.h"));
    let bin = dir(&[&compiler], &[]);
    let lib = dir(&[&library], &[]);
    let tc = dir(&[&header], &[&bin, &lib]);
    Toolchain {
        blobs: vec![compiler, library, header, bin, lib, tc.clone()],
        tc,
    }
}

/// A root holding the toolchain and one source file of its own: four directories read
/// the first time, one more for each further root.
fn root_over(tool: &Toolchain, source: &Blob) -> Blob {
    dir(&[source], &[&tool.tc])
}

async fn walk(cache: &Arc<TestCache>, root: &Blob) -> Vec<Digest> {
    cache.missing_inputs(root.digest).await.expect("walked")
}

fn read(cache: &TestCache) -> u64 {
    cache.metrics().execute_dir_reads.get()
}

fn skipped(cache: &TestCache) -> u64 {
    cache.metrics().input_dirs_skipped.get()
}

fn cached(cache: &TestCache) -> u64 {
    cache.metrics().input_dirs_cached.get()
}

fn millis(n: u64) -> Duration {
    Duration::from_millis(n)
}

/// Catches: a second Execute over the same toolchain reading its directories again, or
/// the first walk not reading each directory exactly once.
#[tokio::test]
async fn a_toolchain_seen_once_is_never_read_again() {
    let (cache, store) = cache().await;
    let tool = toolchain();
    let (one, two) = (file("one.c"), file("two.c"));
    let (root_one, root_two) = (root_over(&tool, &one), root_over(&tool, &two));
    put(&cache, &tool.blobs.iter().collect::<Vec<_>>()).await;
    put(&cache, &[&one, &two, &root_one, &root_two]).await;
    store.reset();

    assert!(walk(&cache, &root_one).await.is_empty());
    assert_eq!((read(&cache), store.gets()), (4, 4), "root, tc, bin, lib");

    // Another action over the same toolchain: only its own root is read.
    assert!(walk(&cache, &root_two).await.is_empty());
    assert_eq!((read(&cache), store.gets()), (5, 5));
    assert_eq!(skipped(&cache), 1, "the toolchain subtree is skipped whole");

    // The same action again: nothing is read, the root itself is skipped.
    assert!(walk(&cache, &root_one).await.is_empty());
    assert_eq!(store.gets(), 5);
    assert_eq!(skipped(&cache), 2);
}

/// Catches: `verified_until` honored past its time (the boundary included), or a
/// re-walk that reads the store again when the decoded directory is in memory, or one
/// that does not look at the blobs.
#[tokio::test]
async fn an_expired_verification_is_checked_again() {
    let (cache, store) = cache().await;
    let tool = toolchain();
    let source = file("main.c");
    let root = root_over(&tool, &source);
    put(&cache, &tool.blobs.iter().collect::<Vec<_>>()).await;
    put(&cache, &[&source, &root]).await;
    store.reset();
    let min_ttl = Retention::default().min_ttl;

    assert!(walk(&cache, &root).await.is_empty());
    let until = FarmTime::from_millis(0).saturating_add(min_ttl);

    // One millisecond before the guarantee runs out: skipped.
    cache
        .tick(FarmTime::from_millis(until.as_millis() - 1))
        .await
        .expect("tick");
    assert!(walk(&cache, &root).await.is_empty());
    assert_eq!(skipped(&cache), 1);

    // At the time itself: not skipped. Every directory is taken from memory (no read)
    // and its blob is looked at.
    cache.tick(until).await.expect("tick");
    assert!(walk(&cache, &root).await.is_empty());
    assert_eq!(skipped(&cache), 1, "no further skip at `until`");
    assert_eq!(cached(&cache), 4);
    assert_eq!(store.gets(), 4, "nothing read again");

    // And the new verification runs for another min_ttl from that walk.
    cache.tick(until).await.expect("tick");
    assert!(walk(&cache, &root).await.is_empty());
    assert_eq!(skipped(&cache), 2);
}

/// Catches: a blob collected while a verification still claims it, a verification
/// outliving the blob once its time has passed, and a directory taken from memory
/// without touching its blob (so the retention clock never restarts for it).
#[tokio::test]
async fn an_evicted_blob_is_caught_after_and_never_before_the_verification_ends() {
    let (cache, store) = cache().await;
    let tool = toolchain();
    let source = file("main.c");
    let root = root_over(&tool, &source);
    put(&cache, &tool.blobs.iter().collect::<Vec<_>>()).await;
    put(&cache, &[&source, &root]).await;
    store.reset();
    let min_ttl = Retention::default().min_ttl;

    // Day 3: verified until day 3 + min_ttl; the touch is committed at once.
    cache.tick(days(3)).await.expect("tick");
    assert!(walk(&cache, &root).await.is_empty());
    assert!(cache.collect().await.expect("collect").blobs.is_empty());
    let until = days(3).saturating_add(min_ttl);

    // Just before `until`: the collection keeps everything, the skip is right.
    cache
        .tick(FarmTime::from_millis(until.as_millis() - 1))
        .await
        .expect("tick");
    assert!(
        cache.collect().await.expect("collect").blobs.is_empty(),
        "nothing a verification still claims is collected"
    );
    let reads = store.gets();
    assert!(walk(&cache, &root).await.is_empty());
    assert_eq!(store.gets(), reads);

    // Past the retention: the blobs go, and the next check says so rather than
    // trusting the old verification.
    cache.tick(days(12)).await.expect("tick");
    let gone = cache.collect().await.expect("collect").blobs;
    assert_eq!(gone.len(), tool.blobs.len() + 2);
    assert_eq!(walk(&cache, &root).await, [root.digest]);
}

/// Catches: a directory skipped on the strength of a subtree that was verified earlier
/// than the directory's own walk: its guarantee ends with the subtree's.
#[tokio::test]
async fn a_root_resting_on_a_skipped_subtree_is_verified_only_as_long_as_it() {
    let (cache, _store) = cache().await;
    let tool = toolchain();
    let source = file("main.c");
    let root = root_over(&tool, &source);
    put(&cache, &tool.blobs.iter().collect::<Vec<_>>()).await;
    put(&cache, &[&source, &root]).await;

    // Day 0: the toolchain alone is verified, until day 7.
    assert!(walk(&cache, &tool.tc).await.is_empty());
    // Day 5: a root over it skips the toolchain; its own verification ends at day 7
    // too, not day 12.
    cache.tick(days(5)).await.expect("tick");
    assert!(walk(&cache, &root).await.is_empty());
    assert_eq!(skipped(&cache), 1);
    assert!(cache.collect().await.expect("collect").blobs.is_empty());
    // Day 8.5: the toolchain's blobs (last touched day 0) are past their retention.
    cache
        .tick(days(8).saturating_add(DAY / 2))
        .await
        .expect("tick");
    let gone = cache.collect().await.expect("collect").blobs;
    assert_eq!(
        gone.len(),
        tool.blobs.len(),
        "the toolchain, not the root's own"
    );
    let absent = walk(&cache, &root).await;
    assert_eq!(absent, [tool.tc.digest], "the root was not skipped");
}

/// Catches: a directory taken from memory without touching its blob, which lets the
/// blob be collected while its files, touched, remain.
#[tokio::test]
async fn a_directory_taken_from_memory_is_touched() {
    let (cache, _store) = cache().await;
    let tool = toolchain();
    put(&cache, &tool.blobs.iter().collect::<Vec<_>>()).await;
    let min_ttl = Retention::default().min_ttl;

    assert!(walk(&cache, &tool.tc).await.is_empty());
    // Past the verification, inside the retention: the walk is repeated from memory.
    cache
        .tick(FarmTime::from_millis(0).saturating_add(min_ttl))
        .await
        .expect("tick");
    assert!(walk(&cache, &tool.tc).await.is_empty());
    assert_eq!(cached(&cache), 3);
    // The touches of that walk commit now, at day 7. The blobs first touched at day 0
    // are past their retention a day later; the ones the second walk touched are not.
    assert!(cache.collect().await.expect("collect").blobs.is_empty());
    cache
        .tick(days(8).saturating_add(millis(1)))
        .await
        .expect("tick");
    assert!(cache.collect().await.expect("collect").blobs.is_empty());
}

/// Catches: a missing directory or file not reported (with a file under an absent
/// directory reported too), a failed walk remembered as verified, and a report that
/// outlives the upload that fixes it.
#[tokio::test]
async fn missing_inputs_are_reported_and_never_remembered() {
    let (cache, store) = cache().await;
    let (present, lost) = (file("present"), file("lost"));
    let (hidden, vanished) = (file("under an absent dir"), file("never uploaded dir"));
    let absent_dir = dir(&[&hidden], &[]);
    let deep = dir(&[&lost], &[]);
    let root = dir(&[&present], &[&deep, &absent_dir, &dir(&[&vanished], &[])]);
    let vanished_dir = dir(&[&vanished], &[]);
    put(&cache, &[&present, &deep, &root]).await;

    let want = {
        let mut dirs = vec![absent_dir.digest, vanished_dir.digest];
        dirs.sort();
        dirs.push(lost.digest);
        dirs
    };
    assert_eq!(walk(&cache, &root).await, want);
    // Again: not skipped, and still reported.
    assert_eq!(walk(&cache, &root).await, want);
    assert_eq!(skipped(&cache), 0);

    // Uploading what is missing is seen at once.
    put(
        &cache,
        &[&lost, &absent_dir, &vanished_dir, &hidden, &vanished],
    )
    .await;
    assert!(walk(&cache, &root).await.is_empty());
    let reads = store.gets();
    assert!(walk(&cache, &root).await.is_empty());
    assert_eq!(store.gets(), reads, "now verified");
}

/// Catches: a directory that does not decode being treated as absent or empty.
#[tokio::test]
async fn a_directory_that_does_not_decode_is_invalid() {
    let (cache, _store) = cache().await;
    let junk = Blob::new(vec![0xff_u8; 9]);
    put(&cache, &[&junk]).await;
    let error = cache.missing_inputs(junk.digest).await.expect_err("junk");
    assert!(matches!(error, CacheError::Invalid(_)), "{error:?}");
}

/// Catches: every request walking a shared subtree itself: eight roots over one
/// toolchain, started together, read each directory once.
#[tokio::test]
async fn requests_sharing_a_subtree_read_it_once() {
    let (cache, store) = cache().await;
    let tool = toolchain();
    put(&cache, &tool.blobs.iter().collect::<Vec<_>>()).await;
    let mut roots = Vec::new();
    for n in 0..8 {
        let source = file(&format!("source {n}"));
        let root = root_over(&tool, &source);
        put(&cache, &[&source, &root]).await;
        roots.push(root);
    }
    store.reset();
    let walks = roots.iter().map(|r| cache.missing_inputs(r.digest));
    for absent in futures_join(walks).await {
        assert!(absent.expect("walked").is_empty());
    }
    // Eight roots, and the toolchain's three directories once.
    assert_eq!(store.gets(), 8 + 3);
    assert_eq!(cache.metrics().input_dirs_joined.get(), 7);
}

/// Catches: store reads unbounded by `reads_in_flight`, and bounded so tightly that a
/// wide directory is read one at a time.
#[tokio::test]
async fn directory_reads_are_concurrent_up_to_their_bound() {
    let config = InputTreeConfig {
        reads_in_flight: 3.try_into().expect("nonzero"),
        ..InputTreeConfig::DEFAULT
    };
    let (cache, store) = cache_with(config).await;
    let files: Vec<Blob> = (0..12).map(|n| file(&format!("leaf {n}"))).collect();
    let leaves: Vec<Blob> = files.iter().map(|f| dir(&[f], &[])).collect();
    let root = dir(&[], &leaves.iter().collect::<Vec<_>>());
    put(&cache, &files.iter().collect::<Vec<_>>()).await;
    put(&cache, &leaves.iter().collect::<Vec<_>>()).await;
    put(&cache, &[&root]).await;
    store.reset();

    assert!(walk(&cache, &root).await.is_empty());
    assert_eq!(store.gets(), 13);
    // The root is read alone, then the twelve leaves three at a time.
    assert_eq!(store.peak(), 3);
}

/// Catches: a cache that outgrows its entry bound, one that evicts the wrong directory
/// (least recently used goes first), and an oversized directory kept anyway.
#[tokio::test]
async fn the_cache_is_bounded_and_evicts_the_least_recently_used() {
    let config = InputTreeConfig {
        max_entries: 2.try_into().expect("nonzero"),
        ..InputTreeConfig::DEFAULT
    };
    let (cache, store) = cache_with(config).await;
    let files: Vec<Blob> = (0..3).map(|n| file(&format!("f{n}"))).collect();
    let roots: Vec<Blob> = files.iter().map(|f| dir(&[f], &[])).collect();
    put(&cache, &files.iter().collect::<Vec<_>>()).await;
    put(&cache, &roots.iter().collect::<Vec<_>>()).await;
    store.reset();

    for r in [&roots[0], &roots[1], &roots[0], &roots[2]] {
        assert!(walk(&cache, r).await.is_empty());
    }
    assert_eq!(cache.input_tree_cache().0, 2);
    assert_eq!(store.gets(), 3);
    // roots[1] was the least recently used, so it alone is gone.
    assert!(walk(&cache, &roots[0]).await.is_empty());
    assert!(walk(&cache, &roots[2]).await.is_empty());
    assert_eq!(store.gets(), 3);
    assert!(walk(&cache, &roots[1]).await.is_empty());
    assert_eq!(store.gets(), 4);

    // A byte bound: room for two single-file directories, not three.
    let one = ENTRY_OVERHEAD + size_of::<Digest>() as u64;
    let bytes = InputTreeConfig {
        max_bytes: 2 * one + one / 2,
        ..InputTreeConfig::DEFAULT
    };
    let (cache, _) = cache_with(bytes).await;
    put(&cache, &files.iter().collect::<Vec<_>>()).await;
    put(&cache, &roots.iter().collect::<Vec<_>>()).await;
    for r in &roots {
        assert!(walk(&cache, r).await.is_empty());
    }
    let (entries, held) = cache.input_tree_cache();
    assert!(held <= bytes.max_bytes, "{held} bytes");
    assert_eq!(entries, 2);

    // A directory dearer than the whole bound is checked, not kept.
    let tiny = InputTreeConfig {
        max_bytes: ENTRY_OVERHEAD,
        ..InputTreeConfig::DEFAULT
    };
    let (cache, store) = cache_with(tiny).await;
    put(&cache, &files.iter().collect::<Vec<_>>()).await;
    put(&cache, &roots.iter().collect::<Vec<_>>()).await;
    store.reset();
    assert!(walk(&cache, &roots[0]).await.is_empty());
    assert!(walk(&cache, &roots[0]).await.is_empty());
    assert_eq!(cache.input_tree_cache().0, 0);
    assert_eq!(store.gets(), 2);
}

/// Catches: a directory whose object cannot be read answered as missing or empty (the
/// input exists, so the call must fail, not tell the client to upload it), at the root
/// and under it, and a directory naming a digest that does not parse accepted.
#[tokio::test]
async fn unreadable_and_malformed_directories_fail_the_check() {
    let (cache, store) = cache().await;
    let leaf_file = file("leaf");
    let leaf = dir(&[&leaf_file], &[]);
    put(&cache, &[&leaf_file, &leaf]).await;
    let leaf_objects = objects(&store).await;
    let root = dir(&[], &[&leaf]);
    put(&cache, &[&root]).await;
    for key in &leaf_objects {
        store.delete(key).await.expect("deleted");
    }
    let error = cache
        .missing_inputs(root.digest)
        .await
        .expect_err("unreadable");
    assert!(
        matches!(error, CacheError::Unreachable(d) if d == leaf.digest),
        "{error:?}"
    );
    // The same, again: the object stays marked, and the failure is not remembered as
    // anything else.
    let error = cache
        .missing_inputs(leaf.digest)
        .await
        .expect_err("unreadable");
    assert!(matches!(error, CacheError::Unreachable(_)), "{error:?}");

    let bad = Blob::of(&Directory {
        files: vec![FileNode {
            name: "f".to_owned(),
            digest: Some(kbf_proto::reapi::Digest {
                hash: "not a hash".to_owned(),
                size_bytes: 3,
            }),
            ..FileNode::default()
        }],
        ..Directory::default()
    });
    put(&cache, &[&bad]).await;
    let error = cache
        .missing_inputs(bad.digest)
        .await
        .expect_err("bad digest");
    assert!(matches!(error, CacheError::Invalid(_)), "{error:?}");
}

/// Catches: a verification recorded against a directory the bound already evicted
/// (it must be dropped, not resurrect an entry), and a walk that depends on keeping
/// anything.
#[tokio::test]
async fn a_cache_of_one_still_walks_a_tree() {
    let config = InputTreeConfig {
        max_entries: 1.try_into().expect("nonzero"),
        ..InputTreeConfig::DEFAULT
    };
    let (cache, _store) = cache_with(config).await;
    let tool = toolchain();
    put(&cache, &tool.blobs.iter().collect::<Vec<_>>()).await;
    assert!(walk(&cache, &tool.tc).await.is_empty());
    assert!(walk(&cache, &tool.tc).await.is_empty());
    assert_eq!(cache.input_tree_cache().0, 1);
}

/// The keys of every object in the store.
async fn objects(store: &Counting) -> Vec<ObjectKey> {
    let page = store
        .list(&KeyPrefix::default(), None, PageSize::MAX)
        .await
        .expect("listed");
    page.objects.into_iter().map(|o| o.key).collect()
}

/// Every future to completion on this task, interleaved, in order.
async fn futures_join<F: std::future::Future>(
    futures: impl IntoIterator<Item = F>,
) -> Vec<F::Output> {
    futures::future::join_all(futures).await
}
