//! The `Directory` cache of the put-directory closure walk, and `PutDirPrep::mint_put_dir`.
//!
//! A walk through a cache reads no `Directory` from the store once the cache holds it (in memory,
//! or on its disk after a restart), a disk entry that is not the blob is never used, and the
//! walk's answer, its errors and its asks of the store for the files and the Command are the
//! same with a cache as without one.

use kbf_front::prepare_run::{
    DIRECTORY_MEMORY_BYTES, DetachedWrites, DirectoryCache, KeptClosure, PutDirPrep,
};
use kbf_pack::ReadCache;
use kbf_pack_index::BlobDigest;
use kbf_testkit::dir::TestDir;

use super::*;

fn prep_over(store: &Arc<Watched>, cache: &Arc<DirectoryCache>) -> PutDirPrep {
    let storage: Arc<dyn Storage> = store.clone();
    PutDirPrep::new(
        Backend::remote(storage),
        PackKeyMint::new(MINT_SECRET).expect("a mint"),
        SLACK,
        ClosureLimits::DEFAULT,
    )
    .with_directories(Arc::clone(cache))
    .with_detached_writes(DetachedWrites::new(8))
}

fn memory() -> Arc<DirectoryCache> {
    Arc::new(DirectoryCache::in_memory(DIRECTORY_MEMORY_BYTES))
}

fn scratch(name: &str) -> TestDir {
    TestDir::under(env!("CARGO_TARGET_TMPDIR"), name)
}

/// The closure of `action` through `prep`, once the writes the preparation left running have
/// ended.
async fn walk(prep: &PutDirPrep, action: &Digest) -> KeptClosure {
    let run = prep
        .prepare_put_dir(LEASE, action, now(), WALL)
        .await
        .expect("prepared");
    prep.detached_writes().settled().await;
    run.kept
}

/// Forgets the blobs read so far.
fn forget_reads(store: &Watched) {
    store
        .reads
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clear();
}

/// The standard tree's blobs, sorted, as a walk reads them: the Action and the Directories
/// of `directories`.
fn read_set(parts: &Parts, directories: &[Digest]) -> Vec<Digest> {
    let mut reads = vec![parts.action];
    reads.extend_from_slice(directories);
    reads.sort();
    reads
}

fn all_directories(parts: &Parts) -> [Digest; 4] {
    let tree = &parts.tree;
    [tree.root, tree.lib, tree.lib2, tree.deep]
}

/// Asserts that a second walk of an Action through a cache reads no `Directory` (only the
/// Action), gives an equal kept closure, and still asks the store about the files and the
/// Command.
#[tokio::test]
async fn a_second_walk_reads_no_directory_from_the_store() {
    let (parts, store, _) = standard().await;
    let prep = prep_over(&store, &memory());
    let first = walk(&prep, &parts.action).await;
    assert_eq!(store.reads(), read_set(&parts, &all_directories(&parts)));
    forget_reads(&store);
    let asks = store.asks.load(SeqCst);
    let second = walk(&prep, &parts.action).await;
    assert_eq!(store.reads(), [parts.action], "only the Action is read");
    assert!(store.asks.load(SeqCst) > asks, "the files are still asked");
    assert_eq!(second, first);
    assert!(format!("{prep:?}").contains("DirectoryCache"));
}

/// Asserts that a cache that is not given to the preparer changes nothing: two walks without
/// one read every `Directory` twice.
#[tokio::test]
async fn a_walk_without_a_cache_reads_every_directory_each_time() {
    let (parts, store, _) = standard().await;
    let storage: Arc<dyn Storage> = store.clone();
    let prep = PutDirPrep::new(
        Backend::remote(storage),
        PackKeyMint::new(MINT_SECRET).expect("a mint"),
        SLACK,
        ClosureLimits::DEFAULT,
    );
    walk(&prep, &parts.action).await;
    forget_reads(&store);
    walk(&prep, &parts.action).await;
    assert_eq!(store.reads(), read_set(&parts, &all_directories(&parts)));
}

/// Asserts the restart case: a cache opened again on the directory a first one filled serves the
/// `Directory` messages with no read of the store.
#[tokio::test]
async fn a_reopened_disk_cache_serves_the_directories() {
    let (parts, store, _) = standard().await;
    let dir = scratch("front-directories-restart");
    let first = {
        let cache = DirectoryCache::open(dir.path(), DIRECTORY_MEMORY_BYTES, 1 << 20);
        let prep = prep_over(&store, &Arc::new(cache.expect("a cache")));
        walk(&prep, &parts.action).await
    };
    forget_reads(&store);
    let cache = DirectoryCache::open(dir.path(), DIRECTORY_MEMORY_BYTES, 1 << 20);
    let prep = prep_over(&store, &Arc::new(cache.expect("a cache")));
    let second = walk(&prep, &parts.action).await;
    assert_eq!(store.reads(), [parts.action], "only the Action is read");
    assert_eq!(second, first);
}

/// Asserts that a disk entry whose bytes are not the blob (the file has the right length) is not
/// used: the `Directory` is read from the store again, the closure is equal, and the entry is
/// written again so that the next restart serves it.
#[tokio::test]
async fn a_corrupted_disk_entry_is_not_used() {
    let (parts, store, _) = standard().await;
    let dir = scratch("front-directories-corrupt");
    let open = || DirectoryCache::open(dir.path(), DIRECTORY_MEMORY_BYTES, 1 << 20);
    let prep = prep_over(&store, &Arc::new(open().expect("a cache")));
    let first = walk(&prep, &parts.action).await;
    for entry in std::fs::read_dir(dir.path()).expect("the cache directory") {
        let path = entry.expect("an entry").path();
        let length = std::fs::metadata(&path).expect("a file").len();
        let zeros = vec![0_u8; usize::try_from(length).expect("a small file")];
        std::fs::write(&path, zeros).expect("overwritten");
    }
    forget_reads(&store);
    let prep = prep_over(&store, &Arc::new(open().expect("a cache")));
    let second = walk(&prep, &parts.action).await;
    assert_eq!(store.reads(), read_set(&parts, &all_directories(&parts)));
    assert_eq!(second, first);
    forget_reads(&store);
    let prep = prep_over(&store, &Arc::new(open().expect("a cache")));
    assert_eq!(walk(&prep, &parts.action).await, first);
    assert_eq!(store.reads(), [parts.action], "served from the disk again");
}

/// Asserts that a memory bound smaller than a tree drops the least recently used `Directory`:
/// the standard tree's four Directories cost 586 (root), 454 (lib), and 322 (lib2 and deep)
/// bytes, so with room for 1000 the root, the first kept, is out when the walk ends, and the next
/// walk reads it again; the closure is the same.
#[tokio::test]
async fn a_memory_bound_under_a_tree_drops_the_least_recently_used() {
    let (parts, store, _) = standard().await;
    let cache = Arc::new(DirectoryCache::in_memory(1000));
    let prep = prep_over(&store, &cache);
    let first = walk(&prep, &parts.action).await;
    forget_reads(&store);
    let second = walk(&prep, &parts.action).await;
    assert!(store.reads().contains(&parts.tree.root));
    assert_eq!(second, first);
}

/// Asserts that a bound under the cost of every `Directory` keeps none: every walk reads them
/// all.
#[tokio::test]
async fn a_directory_over_the_memory_bound_is_not_kept() {
    let (parts, store, _) = standard().await;
    let prep = prep_over(&store, &Arc::new(DirectoryCache::in_memory(300)));
    walk(&prep, &parts.action).await;
    forget_reads(&store);
    walk(&prep, &parts.action).await;
    assert_eq!(store.reads(), read_set(&parts, &all_directories(&parts)));
}

/// Asserts that a `Directory` the store does not hold is the same error with a cache, and
/// nothing is kept for it: a walk over a store that holds it reads it.
#[tokio::test]
async fn a_missing_directory_is_the_same_error_and_is_not_cached() {
    let parts = Parts::new();
    let cache = memory();
    let holed = Watched::holding(parts.without(&[parts.tree.lib])).await;
    let error = prep_over(&holed, &cache)
        .prepare_put_dir(LEASE, &parts.action, now(), WALL)
        .await
        .expect_err("refused");
    assert!(
        matches!(
            &error,
            PrepareError::Closure(ClosureError::Missing { what: "a Directory", digest })
                if *digest == parts.tree.lib
        ),
        "{error}"
    );
    assert_eq!(error.fault(), PrepareFault::Refused);
    let whole = Watched::holding(parts.blobs.clone()).await;
    walk(&prep_over(&whole, &cache), &parts.action).await;
    let tree = &parts.tree;
    let wanted = read_set(&parts, &[tree.lib, tree.lib2, tree.deep]);
    assert_eq!(
        whole.reads(),
        wanted,
        "the root was cached, the rest was not"
    );
}

/// Asserts that a `Directory` that does not decode is the same error with a cache every time it
/// is asked for: it is not kept.
#[tokio::test]
async fn an_undecodable_directory_is_the_same_error_and_is_not_cached() {
    let parts = Parts::new();
    let bytes = Bytes::from_static(&[0xff, 0xff, 0xff]);
    let root = kbf_segments::sha256(&bytes);
    let action = action_of(Some(&parts.command), Some(&root));
    let store = Watched::holding(vec![
        (root, bytes),
        action.clone(),
        parts.blob(&parts.command),
    ])
    .await;
    let prep = prep_over(&store, &memory());
    for _ in 0..2 {
        forget_reads(&store);
        let error = prep
            .prepare_put_dir(LEASE, &action.0, now(), WALL)
            .await
            .expect_err("refused");
        assert!(
            matches!(
                &error,
                PrepareError::Closure(ClosureError::Invalid { what: "a Directory", digest, .. })
                    if *digest == root
            ),
            "{error}"
        );
        assert_eq!(store.reads(), read_set_of(&action.0, &root));
    }
}

/// Asserts that a `Directory` naming a subdirectory with no digest is the same error with a
/// cache every time it is asked for, and is kept neither in memory nor on the disk.
#[tokio::test]
async fn a_directory_with_a_malformed_digest_is_the_same_error_and_is_not_cached() {
    let parts = Parts::new();
    let message = kbf_proto::reapi::Directory {
        directories: vec![kbf_proto::reapi::DirectoryNode {
            name: "d".to_owned(),
            digest: None,
        }],
        ..Default::default()
    };
    let bytes = Bytes::from(prost::Message::encode_to_vec(&message));
    let root = kbf_segments::sha256(&bytes);
    let action = action_of(Some(&parts.command), Some(&root));
    let store = Watched::holding(vec![
        (root, bytes),
        action.clone(),
        parts.blob(&parts.command),
    ])
    .await;
    let dir = scratch("malformed-digest");
    let cache = DirectoryCache::open(dir.path(), DIRECTORY_MEMORY_BYTES, 1 << 20);
    let prep = prep_over(&store, &Arc::new(cache.expect("a cache")));
    for _ in 0..2 {
        forget_reads(&store);
        let error = prep
            .prepare_put_dir(LEASE, &action.0, now(), WALL)
            .await
            .expect_err("refused");
        assert!(
            matches!(
                &error,
                PrepareError::Closure(ClosureError::Invalid { what: "a subdirectory", digest, .. })
                    if *digest == root
            ),
            "{error}"
        );
        assert_eq!(store.reads(), read_set_of(&action.0, &root));
    }
    let stored = std::fs::read_dir(dir.path()).expect("the cache directory");
    assert_eq!(stored.count(), 0, "nothing is stored on the disk");
}

fn read_set_of(action: &Digest, directory: &Digest) -> Vec<Digest> {
    let mut reads = vec![*action, *directory];
    reads.sort();
    reads
}

/// Asserts that a disk entry that verifies against its digest but does not decode is not
/// served: the `Directory` is read from the store, and its error is the walk's.
#[tokio::test]
async fn an_undecodable_disk_entry_falls_back_to_the_store() {
    let parts = Parts::new();
    let bytes = Bytes::from_static(&[0xff, 0xff, 0xff]);
    let root = kbf_segments::sha256(&bytes);
    let dir = scratch("front-directories-undecodable");
    let disk = ReadCache::open(dir.path(), 1 << 20).expect("a disk cache");
    let key = BlobDigest {
        hash: root.hash,
        size: root.size_bytes,
    };
    disk.put(&key, &bytes).expect("kept");
    let action = action_of(Some(&parts.command), Some(&root));
    let store = Watched::holding(vec![
        (root, bytes),
        action.clone(),
        parts.blob(&parts.command),
    ])
    .await;
    let cache = DirectoryCache::open(dir.path(), DIRECTORY_MEMORY_BYTES, 1 << 20);
    let error = prep_over(&store, &Arc::new(cache.expect("a cache")))
        .prepare_put_dir(LEASE, &action.0, now(), WALL)
        .await
        .expect_err("refused");
    assert!(
        matches!(
            &error,
            PrepareError::Closure(ClosureError::Invalid {
                what: "a Directory",
                ..
            })
        ),
        "{error}"
    );
    assert_eq!(store.reads(), read_set_of(&action.0, &root));
}

/// Asserts that a disk half that cannot be written is not an error of the walk: its directory is
/// gone, the walk gives the same closure, and the second walk is served from memory.
#[tokio::test]
async fn a_disk_that_cannot_be_written_does_not_fail_the_walk() {
    let (parts, store, _) = standard().await;
    let dir = scratch("front-directories-unwritable");
    let path = dir.path().join("cache");
    let cache = DirectoryCache::open(&path, DIRECTORY_MEMORY_BYTES, 1 << 20);
    let prep = prep_over(&store, &Arc::new(cache.expect("a cache")));
    std::fs::remove_dir_all(&path).expect("removed");
    let first = walk(&prep, &parts.action).await;
    forget_reads(&store);
    let second = walk(&prep, &parts.action).await;
    assert_eq!(store.reads(), [parts.action], "served from memory");
    assert_eq!(second, first);
}

/// Asserts that a cache that cannot be opened is an error naming the directory.
#[test]
fn a_cache_directory_that_cannot_be_made_is_an_error() {
    let dir = scratch("front-directories-open");
    let file = dir.path().join("a-file");
    std::fs::write(&file, b"not a directory").expect("written");
    let error = DirectoryCache::open(file.join("cache"), 1, 1).expect_err("refused");
    assert!(error.to_string().contains("a-file"), "{error}");
}

/// Asserts that the empty `Directory` is never read or stored: a tree holding an empty
/// subdirectory reads only the Directories that have a blob, with a cache and without.
#[tokio::test]
async fn the_empty_directory_is_never_read() {
    let parts = Parts::new();
    let (root, root_bytes) = dir(&[], &[empty()]);
    let action = action_of(Some(&parts.command), Some(&root));
    let store = Watched::holding(vec![
        (root, root_bytes),
        action.clone(),
        parts.blob(&parts.command),
    ])
    .await;
    let dir = scratch("front-directories-empty");
    let cache = DirectoryCache::open(dir.path(), DIRECTORY_MEMORY_BYTES, 1 << 20);
    let prep = prep_over(&store, &Arc::new(cache.expect("a cache")));
    let kept = walk(&prep, &action.0).await;
    assert_eq!(kept.directory_count(), 2);
    assert_eq!(store.reads(), read_set_of(&action.0, &root));
    let entries = std::fs::read_dir(dir.path())
        .expect("the directory")
        .count();
    assert_eq!(entries, 1, "only the root is on the disk");
}

fn only(store: &Arc<Watched>) -> PutDirPrep {
    let storage: Arc<dyn Storage> = store.clone();
    PutDirPrep::new(
        Backend::remote(storage),
        PackKeyMint::new(MINT_SECRET).expect("a mint"),
        SLACK,
        ClosureLimits::DEFAULT,
    )
}

/// Asserts that `mint_put_dir` gives the directory and expiry `prepare_put_dir` gives, which the
/// mint verifies, and reads nothing from the store and asks it nothing.
#[tokio::test]
async fn mint_put_dir_gives_the_dir_the_mint_verifies_and_reads_nothing() {
    let (parts, store, _) = standard().await;
    let prep = only(&store);
    let minted = prep
        .mint_put_dir(LEASE, &parts.action, now(), WALL)
        .await
        .expect("minted");
    let run = prep
        .prepare_put_dir(LEASE, &parts.action, now(), WALL)
        .await
        .expect("prepared");
    assert_eq!(minted.dir, run.dir);
    assert_eq!(minted.not_after, run.not_after);
    assert_eq!(
        minted.not_after,
        NOW_SECS + WALL.as_secs() + SLACK.as_secs()
    );
    assert_eq!(
        prep.mint().verify_put_dir(&minted.dir, &parts.action),
        Ok(now())
    );
    let (_, untouched, _) = standard().await;
    only(&untouched)
        .mint_put_dir(LEASE, &parts.action, now(), WALL)
        .await
        .expect("minted");
    assert!(untouched.reads().is_empty());
    assert_eq!(untouched.asks.load(SeqCst), 0);
}

/// Asserts that `mint_put_dir` refuses what `prepare_put_dir` refuses before it reads: a lease
/// that is no build id, a wall over the ceiling, and a time before the epoch.
#[tokio::test]
async fn mint_put_dir_refuses_a_bad_lease_a_long_wall_and_an_early_clock() {
    let (parts, store, _) = standard().await;
    let prep = only(&store);
    let bad_lease = prep
        .mint_put_dir("a.b", &parts.action, now(), WALL)
        .await
        .expect_err("refused");
    assert!(matches!(bad_lease, PrepareError::Lease(_)), "{bad_lease}");
    let long = prep
        .mint_put_dir(LEASE, &parts.action, now(), MAX_EXPIRY)
        .await
        .expect_err("refused");
    assert!(matches!(long, PrepareError::Expiry(_)), "{long}");
    let early = UNIX_EPOCH - Duration::from_secs(1);
    let clock = prep
        .mint_put_dir(LEASE, &parts.action, early, WALL)
        .await
        .expect_err("refused");
    assert!(matches!(clock, PrepareError::Clock), "{clock}");
    assert!(store.reads().is_empty());
}

/// Asserts that `mint_put_dir` refuses a time past the last minute a key can name, with the
/// mint's own error.
#[tokio::test]
async fn mint_put_dir_refuses_a_time_the_mint_cannot_name() {
    let (parts, store, _) = standard().await;
    let far = UNIX_EPOCH + Duration::from_secs(1 << 40);
    let error = only(&store)
        .mint_put_dir(LEASE, &parts.action, far, WALL)
        .await
        .expect_err("refused");
    assert!(matches!(error, PrepareError::Mint(_)), "{error}");
}

/// Asserts that with no permit free the disk half is not written (and the skip is counted), the
/// preparation still gives the same closure, and a cache opened again on the directory reads the
/// Directories from the store.
#[tokio::test]
async fn a_disk_write_with_no_permit_free_is_skipped_and_counted() {
    let (parts, store, _) = standard().await;
    let dir = scratch("front-directories-no-permit");
    let open = || DirectoryCache::open(dir.path(), DIRECTORY_MEMORY_BYTES, 1 << 20);
    let first = {
        let prep = prep_over(&store, &Arc::new(open().expect("a cache")))
            .with_detached_writes(DetachedWrites::new(0));
        let kept = walk(&prep, &parts.action).await;
        // The record of the rows and the write of the disk half each find no permit.
        assert_eq!(prep.detached_writes().skipped(), 2);
        kept
    };
    forget_reads(&store);
    let prep = prep_over(&store, &Arc::new(open().expect("a cache")));
    let second = walk(&prep, &parts.action).await;
    assert_eq!(store.reads(), read_set(&parts, &all_directories(&parts)));
    assert_eq!(second, first);
    assert_eq!(prep.detached_writes().skipped(), 0);
}
