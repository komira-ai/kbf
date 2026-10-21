//! The drain a node runs for the blobs it holds in RAM: the task while it serves, and the
//! bounded drain when it stops. The blobs are put in with `Cache::hold`, the producer's
//! entry (nothing in a running node calls it yet). Time is tokio's paused clock, on a
//! current-thread runtime, where a spawned task runs only when the test awaits: so a test
//! can tell the drain `after` makes from the background task's.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime};

use bytes::Bytes;
use kbf_front::{Cache, Hold, MemoryMetaLog, VerifiedBlob};
use kbf_meta::Retention;
use kbf_objstore::{
    ByteRange, Capabilities, KeyPrefix, ListPage, ListToken, MemoryStore, ObjectKey, ObjectStore,
    ObjectStoreError, PageSize,
};

use super::RamDrain;

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

fn blob(seed: u8) -> VerifiedBlob {
    VerifiedBlob::hashed(Bytes::from(vec![seed; 64]))
}

const DEADLINE: Duration = Duration::from_secs(5);

/// The task started with a budget writes what is held, with no call from the test but the
/// hold: `durable_waiting` returns with nothing missing, by notification, once the entry
/// has left. Catches a task that is not started (mutant: `start` never spawns it: the wait
/// ends missing after its bound) or one that is not woken by a hold.
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
/// not drain (mutant: `after` returns `served`'s output at once: one blob is still held).
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
