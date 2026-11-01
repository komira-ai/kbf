//! The drain a node runs for the blobs it holds in RAM: the task while it serves, and the
//! bounded drain when it stops. The blobs are put in with `Cache::hold`, the producer's
//! entry (nothing in a running node calls it yet). Time is tokio's paused clock, on a
//! current-thread runtime, where a spawned task runs only when the test awaits: so a test
//! can tell the drain `after` makes from the background task's.
//!
//! The tests of the flush a stop makes of the blocks staged on the hot store run on the
//! real clock instead: that flush reads and hashes on threads of the process, which a
//! paused clock does not wait for.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime};

use bytes::Bytes;
use kbf_front::{Cache, Hold, MemoryMetaLog, VerifiedBlob};
use kbf_meta::{Retention, ServerId};
use kbf_objstore::{
    ByteRange, Capabilities, KeyPrefix, ListPage, ListToken, MemoryStore, ObjectKey, ObjectStore,
    ObjectStoreError, PageSize,
};
use kbf_store::FaultFs;
use kbf_tier::TierOptions;

use super::RamDrain;
use crate::test_log::capture;

/// A memory store that refuses every PUT while `refuse` is set.
struct Refusing {
    inner: MemoryStore,
    refuse: Arc<AtomicBool>,
}

impl ObjectStore for Refusing {
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
                status: 503,
                code: "SlowDown".to_owned(),
                message: "refused by the test".to_owned(),
            });
        }
        self.inner.put_new(key, body, retain_until).await
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

type TestCache = Cache<MemoryMetaLog, Refusing>;

/// A cache with `budget` bytes of RAM and a store that refuses PUTs while the returned
/// flag is set.
fn cache(budget: u64) -> (Arc<TestCache>, Arc<AtomicBool>) {
    let meta = MemoryMetaLog::new(Retention::default());
    let epoch = meta.alloc_epoch();
    let refuse = Arc::new(AtomicBool::new(false));
    let store = Refusing {
        inner: MemoryStore::new(Capabilities::default()),
        refuse: Arc::clone(&refuse),
    };
    let cache = Cache::new(meta, store, KeyPrefix::default(), epoch).with_ram_ack_bytes(budget);
    (Arc::new(cache), refuse)
}

/// [`cache`] with a write tier on an in-memory disk, so that a blob stored is staged
/// (in the open block, which the flusher seals only 30 s after it opened), and no RAM
/// budget: the default of a node.
async fn tiered() -> (Arc<TestCache>, Arc<AtomicBool>) {
    let meta = MemoryMetaLog::new(Retention::default());
    let epoch = meta.alloc_epoch();
    let refuse = Arc::new(AtomicBool::new(false));
    let store = Refusing {
        inner: MemoryStore::new(Capabilities::default()),
        refuse: Arc::clone(&refuse),
    };
    let cache = Cache::new(meta, store, KeyPrefix::default(), epoch);
    let options = TierOptions::new(1 << 20);
    let (cache, _) = cache
        .attach_tier(FaultFs::new(), options, ServerId::new(1), None)
        .await
        .expect("attach");
    (Arc::new(cache), refuse)
}

/// The blocks of `cache`'s tier that hold acknowledged bytes not yet in the store.
fn unflushed(cache: &TestCache) -> u64 {
    cache.tier_health().expect("a tier").unflushed_blocks
}

/// The 64 bytes of `seed`'s blob.
fn bytes(seed: u8) -> Bytes {
    Bytes::from(vec![seed; 64])
}

fn blob(seed: u8) -> VerifiedBlob {
    VerifiedBlob::hashed(bytes(seed))
}

const DEADLINE: Duration = Duration::from_secs(5);

/// The task started with a budget writes what is held, with no call from the test but the
/// hold: `durable_waiting` returns with nothing missing, by notification, once the entry
/// has left. Catches a task that is not started or one that is not woken by a hold.
#[tokio::test(start_paused = true)]
async fn the_task_writes_what_is_held_while_the_node_serves() {
    let (cache, _) = cache(1 << 20);
    let ram = RamDrain::start(Arc::clone(&cache), 1 << 20, DEADLINE);
    let held = blob(1);
    assert_eq!(cache.hold(&held), Hold::Held);
    let missing = cache
        .durable_waiting(&[held.digest()])
        .await
        .expect("asked");
    assert!(missing.is_empty(), "{missing:?}");
    assert_eq!(cache.unwritten().blobs, 0);
    drop(ram);
}

/// A stop drains after the node's calls have ended and not before: `served` holds a blob
/// and ends without yielding, so the background task (which runs only when the test
/// yields) has not run, and the blob is in the index when `after` returns only because the
/// stop's drain wrote it. The output of `served` comes through. Catches a stop that does
/// not drain.
#[tokio::test(start_paused = true)]
async fn the_stop_drains_what_is_held_after_serving_ends() {
    let (cache, _) = cache(1 << 20);
    let ram = RamDrain::start(Arc::clone(&cache), 1 << 20, DEADLINE);
    let held = blob(2);
    let served = {
        let cache = Arc::clone(&cache);
        let held = held.clone();
        async move {
            assert_eq!(cache.hold(&held), Hold::Held);
            "served"
        }
    };
    assert_eq!(ram.after(served).await, "served");
    assert_eq!(cache.unwritten().blobs, 0, "written by the stop's drain");
    assert!(
        cache
            .durable(&[held.digest()])
            .await
            .expect("asked")
            .is_empty(),
        "and in the index"
    );
}

/// A stop whose disk keeps refusing ends at the deadline, with the blob still held, and the
/// output of `served` comes through. Catches a stop that waits without end (the test
/// would hang) or drops what it could not write.
#[tokio::test(start_paused = true)]
async fn a_stop_that_cannot_write_ends_at_its_deadline() {
    let (cache, refuse) = cache(1 << 20);
    refuse.store(true, Ordering::SeqCst);
    let ram = RamDrain::start(Arc::clone(&cache), 1 << 20, DEADLINE);
    let held = blob(3);
    let served = {
        let cache = Arc::clone(&cache);
        let held = held.clone();
        async move {
            assert_eq!(cache.hold(&held), Hold::Held);
            7
        }
    };
    assert_eq!(ram.after(served).await, 7);
    assert_eq!(cache.unwritten().blobs, 1, "never dropped");
}

/// With no budget nothing is held and nothing runs: a stop returns `served`'s output and
/// the cache refuses a hold. Catches a stop that drains a cache with no budget (it has no
/// deadline to drain by).
#[tokio::test(start_paused = true)]
async fn with_no_budget_a_stop_drains_nothing() {
    let (cache, _) = cache(0);
    let ram = RamDrain::start(Arc::clone(&cache), 0, DEADLINE);
    assert_eq!(cache.hold(&blob(4)), Hold::Budget);
    assert_eq!(ram.after(async { "served" }).await, "served");
    assert_eq!(cache.unwritten().blobs, 0);
}

/// A stop flushes the blocks staged on the hot store after the node's calls have ended and
/// not before, with no RAM budget: `served` stages a blob (a call that was in flight) and
/// ends, and the block is in the store when `after` returns only because the stop's flush
/// took it, with the 30 s seal delay of the open block not passed. The output of `served`
/// comes through. Catches a stop that does not flush the tier (defect: `after` returns
/// `served`'s output at once: one block is still staged), one that flushes before `served`
/// ends (defect: the flush is awaited first: the blob staged by `served` is missed), and one
/// that flushes only with a RAM budget (defect: the flush is inside `if self.holds`).
#[tokio::test]
async fn the_stop_flushes_what_is_staged_after_serving_ends() {
    let (cache, _) = tiered().await;
    let ram = RamDrain::start(Arc::clone(&cache), 0, DEADLINE);
    let staged = blob(5);
    let served = {
        let cache = Arc::clone(&cache);
        let staged = staged.clone();
        async move {
            assert_eq!(unflushed(&cache), 0, "nothing is staged before the call");
            cache.store_blobs(vec![staged]).await.expect("staged");
            assert_eq!(unflushed(&cache), 1, "the call staged its blob");
            "served"
        }
    };
    assert_eq!(ram.after(served).await, "served");
    assert_eq!(unflushed(&cache), 0, "flushed by the stop");
    let read = cache.read_blob(&staged.digest()).await.expect("read");
    assert_eq!(read, bytes(5));
}

/// A stop whose store keeps refusing ends at its deadline with the block still staged,
/// says how much was left, and the output of `served` comes through; the block is not
/// dropped: it is flushed when the store answers again. Catches a stop that waits without
/// end (the test would hang), one that drops what it could not flush (defect: the block
/// is discarded when the deadline passes: it is gone, and so is the blob) and one that
/// ends without a line saying what was left (defect: the timed-out arm does not log).
#[tokio::test]
async fn a_stop_that_cannot_flush_ends_at_its_deadline_and_says_what_is_left() {
    let log = capture();
    let (cache, refuse) = tiered().await;
    refuse.store(true, Ordering::SeqCst);
    let ram = RamDrain::start(Arc::clone(&cache), 0, Duration::from_millis(500));
    let staged = blob(6);
    let served = {
        let cache = Arc::clone(&cache);
        let staged = staged.clone();
        async move {
            cache.store_blobs(vec![staged]).await.expect("staged");
            7
        }
    };
    assert_eq!(ram.after(served).await, 7);
    assert_eq!(unflushed(&cache), 1, "never dropped");
    let said = log.text();
    assert!(said.contains("still staged on the hot store"), "{said}");
    assert!(said.contains("blocks=1"), "{said}");

    refuse.store(false, Ordering::SeqCst);
    let later = Instant::now() + Duration::from_secs(120);
    let round = cache.flush_tier(later).await.expect("round");
    assert_eq!(round.flushed.len(), 1);
    assert_eq!(unflushed(&cache), 0);
    let read = cache.read_blob(&staged.digest()).await.expect("read");
    assert_eq!(read, bytes(6));
}

/// A flush that works logs one line saying so and nothing at ERROR. Catches a stop that
/// reports a flush that finished as one that did not (defect: the arms are swapped).
#[tokio::test]
async fn a_stop_that_flushes_says_so() {
    let log = capture();
    let (cache, _) = tiered().await;
    let ram = RamDrain::start(Arc::clone(&cache), 0, DEADLINE);
    let staged = blob(8);
    let served = {
        let cache = Arc::clone(&cache);
        async move {
            cache.store_blobs(vec![staged]).await.expect("staged");
            "served"
        }
    };
    assert_eq!(ram.after(served).await, "served");
    let said = log.text();
    assert!(said.contains("no block is left staged"), "{said}");
    assert!(!said.contains("ERROR"), "{said}");
}
