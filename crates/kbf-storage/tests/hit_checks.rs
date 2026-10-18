//! The store check an action-cache hit runs before it is served
//! ([`Cache::action_result`] asks [`ObjectStore::may_hold`] of every stored object the
//! entry needs): the checks run side by side, at most [`HIT_CHECKS_IN_FLIGHT`] at once,
//! every object found gone is marked unreachable, and a failed check fails the call.
//!
//! The store here counts the checks in flight at the store itself, and can hold each
//! check at a barrier until a given number are in flight, so "the checks overlap" is
//! proven by the store seeing them overlap, not inferred from a timing.

mod common;

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use common::Blob;
use kbf_meta::{ObjectId, Retention, Role, UnreachableReason};
use kbf_objstore::{
    ByteRange, Capabilities, KeyPrefix, ListPage, ListToken, MemoryStore, ObjectKey, ObjectStore,
    ObjectStoreError, PageSize,
};
use kbf_proto::reapi::{ActionResult, OutputFile};
use kbf_storage::{Cache, CacheError, HIT_CHECKS_IN_FLIGHT, MemoryMetaLog, MetaLog, VerifiedBlob};
use kbf_types::Digest;
use tokio::sync::Barrier;

/// Long enough that only a check that never comes back reaches it.
const PATIENCE: Duration = Duration::from_secs(10);

/// A memory store that counts the `may_hold` checks in flight, can hold each one until
/// `gate` checks are waiting together, and can fail the check of one key.
#[derive(Clone)]
struct Checks(Arc<State>);

struct State {
    inner: MemoryStore,
    gate: Option<Barrier>,
    fail: Mutex<Option<ObjectKey>>,
    active: AtomicUsize,
    peak: AtomicUsize,
}

impl Checks {
    fn new(gate: Option<usize>) -> Self {
        Self(Arc::new(State {
            inner: MemoryStore::new(Capabilities::default()),
            gate: gate.map(Barrier::new),
            fail: Mutex::new(None),
            active: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
        }))
    }

    fn peak(&self) -> usize {
        self.0.peak.load(Ordering::SeqCst)
    }

    fn fail(&self, key: ObjectKey) {
        *self.0.fail.lock().expect("lock") = Some(key);
    }
}

impl ObjectStore for Checks {
    fn capabilities(&self) -> Capabilities {
        self.0.inner.capabilities()
    }

    async fn put_new(
        &self,
        key: &ObjectKey,
        body: Bytes,
        retain_until: Option<std::time::SystemTime>,
    ) -> Result<(), ObjectStoreError> {
        self.0.inner.put_new(key, body, retain_until).await
    }

    async fn get_range(
        &self,
        key: &ObjectKey,
        range: ByteRange,
    ) -> Result<Bytes, ObjectStoreError> {
        self.0.inner.get_range(key, range).await
    }

    async fn may_hold(&self, key: &ObjectKey) -> Result<bool, ObjectStoreError> {
        let now = self.0.active.fetch_add(1, Ordering::SeqCst) + 1;
        self.0.peak.fetch_max(now, Ordering::SeqCst);
        if let Some(gate) = &self.0.gate {
            gate.wait().await;
        }
        let failed = self.0.fail.lock().expect("lock").as_ref() == Some(key);
        let held = if failed {
            Err(ObjectStoreError::Service {
                status: 503,
                code: "SlowDown".into(),
                message: "the check fails".into(),
            })
        } else {
            self.0.inner.may_hold(key).await
        };
        self.0.active.fetch_sub(1, Ordering::SeqCst);
        held
    }

    async fn delete(&self, key: &ObjectKey) -> Result<(), ObjectStoreError> {
        self.0.inner.delete(key).await
    }

    async fn list(
        &self,
        prefix: &KeyPrefix,
        after: Option<&ListToken>,
        max_keys: PageSize,
    ) -> Result<ListPage, ObjectStoreError> {
        self.0.inner.list(prefix, after, max_keys).await
    }
}

type CheckCache = Cache<MemoryMetaLog, Checks>;

async fn cache(store: &Checks) -> CheckCache {
    Cache::open(
        MemoryMetaLog::new(Retention::default()),
        store.clone(),
        KeyPrefix::default(),
    )
    .await
    .expect("open the cache")
}

/// Writes an entry needing `objects` distinct stored objects (`objects - 1` outputs,
/// each stored on its own, plus the result blob) and returns its action digest and
/// those objects, in order.
async fn entry(cache: &CheckCache, objects: usize) -> (Digest, Vec<ObjectId>) {
    let mut output_files = Vec::with_capacity(objects - 1);
    for o in 0..objects - 1 {
        // Even seeds only: `pseudo_random` sets the low bit, so 2k and 2k+1 match.
        let seed = u64::try_from(o * 2).expect("seed");
        let blob = Blob::new(common::pseudo_random(256, seed));
        cache
            .store_blobs(vec![VerifiedBlob::hashed(Bytes::from(blob.data))])
            .await
            .expect("store an output");
        output_files.push(OutputFile {
            path: format!("out/{o}"),
            digest: Some(blob.proto),
            ..Default::default()
        });
    }
    let action = Blob::new(format!("an action needing {objects} objects")).digest;
    let result = ActionResult {
        output_files,
        ..Default::default()
    };
    cache
        .write_action_result(Role::Daemon, action, &result)
        .await
        .expect("write the entry");
    let stored: BTreeSet<ObjectId> = cache
        .meta()
        .query(move |s| s.stored_objects(&action))
        .await
        .expect("query");
    assert_eq!(stored.len(), objects, "the entry needs {objects} objects");
    (action, stored.into_iter().collect())
}

async fn unreachable(cache: &CheckCache, object: ObjectId) -> Option<UnreachableReason> {
    cache
        .meta()
        .query(move |s| s.unreachable(object))
        .await
        .expect("query")
}

async fn hit(cache: &CheckCache, action: Digest) -> Result<bool, CacheError> {
    tokio::time::timeout(PATIENCE, cache.action_result(&action))
        .await
        .expect("the hit's checks never all came back")
        .map(|served| served.is_some())
}

/// Two of six objects (neither the first) are gone: the hit is a miss, and both are
/// marked missing while the four held ones are left alone. Catches a check that stops
/// at the first object (or the first answer) and leaves a later lost object unmarked,
/// so FindMissing would report its blobs present.
#[tokio::test]
async fn every_object_found_gone_is_marked_and_the_hit_is_a_miss() {
    let store = Checks::new(None);
    let cache = cache(&store).await;
    let (action, objects) = entry(&cache, 6).await;
    assert!(
        hit(&cache, action).await.expect("hit"),
        "served while whole"
    );
    let gone = [objects[2], objects[4]];
    for object in gone {
        let key = cache.object_key(object).expect("key");
        cache.objects().delete(&key).await.expect("delete");
    }

    assert!(!hit(&cache, action).await.expect("hit"), "a miss");
    for object in objects {
        let want = gone.contains(&object).then_some(UnreachableReason::Missing);
        assert_eq!(unreachable(&cache, object).await, want, "{object:?}");
    }
}

/// A hit needing one object (its result blob's), checked on its own: gone, it is a
/// miss and marked. Catches the one-check path answering "held" without asking.
#[tokio::test]
async fn a_hit_on_one_gone_object_is_a_miss_and_marks_it() {
    let store = Checks::new(None);
    let cache = cache(&store).await;
    let (action, objects) = entry(&cache, 1).await;
    let key = cache.object_key(objects[0]).expect("key");
    cache.objects().delete(&key).await.expect("delete");

    assert!(!hit(&cache, action).await.expect("hit"), "a miss");
    assert_eq!(
        unreachable(&cache, objects[0]).await,
        Some(UnreachableReason::Missing)
    );
}

/// The store holds every check until four are waiting, and the hit needs four objects:
/// the hit is served only if all four checks are in flight at once. A sequential loop
/// never gets a second check in flight and times out.
#[tokio::test]
async fn the_checks_of_one_hit_overlap() {
    const OBJECTS: usize = 4;
    const { assert!(OBJECTS <= HIT_CHECKS_IN_FLIGHT) };
    let store = Checks::new(Some(OBJECTS));
    let cache = cache(&store).await;
    let (action, _) = entry(&cache, OBJECTS).await;

    assert!(hit(&cache, action).await.expect("hit"), "served");
    assert_eq!(store.peak(), OBJECTS);
}

/// A hit needing 64 objects, over a store that holds each check until
/// `HIT_CHECKS_IN_FLIGHT` are waiting: the most in flight is exactly the bound. Catches
/// an unbounded fan-out (64 at once) or a wider bound; a narrower one never fills the
/// barrier and times out.
#[tokio::test]
async fn at_most_the_bound_of_checks_are_in_flight() {
    const OBJECTS: usize = 64;
    const { assert!(OBJECTS.is_multiple_of(HIT_CHECKS_IN_FLIGHT) && OBJECTS > HIT_CHECKS_IN_FLIGHT) };
    let store = Checks::new(Some(HIT_CHECKS_IN_FLIGHT));
    let cache = cache(&store).await;
    let (action, _) = entry(&cache, OBJECTS).await;

    assert!(hit(&cache, action).await.expect("hit"), "served");
    assert_eq!(store.peak(), HIT_CHECKS_IN_FLIGHT);
}

/// A check that fails, with every other object held, fails the hit with the store's
/// error and marks nothing. Catches a failed check taken for "held" (a hit served
/// unchecked) or for "gone" (a held object marked lost, or a miss with no cause).
#[tokio::test]
async fn a_failed_check_fails_the_hit_and_marks_nothing() {
    let store = Checks::new(None);
    let cache = cache(&store).await;
    let (action, objects) = entry(&cache, 4).await;
    store.fail(cache.object_key(objects[2]).expect("key"));

    let err = hit(&cache, action).await.expect_err("the check failed");
    assert!(matches!(err, CacheError::Store(_)), "{err:?}");
    for object in objects {
        assert_eq!(unreachable(&cache, object).await, None, "{object:?}");
    }
}

/// A failed check on a hit that also needs a gone object: the answer is a miss, the
/// gone object is marked, and the object whose check failed is not. Catches a failed
/// check taken for "gone" (a held object marked lost) and an error that hides a lost
/// object, leaving it unmarked so FindMissing reports its blobs present.
#[tokio::test]
async fn a_failed_check_beside_a_gone_object_is_a_miss_marking_only_the_gone_one() {
    let store = Checks::new(None);
    let cache = cache(&store).await;
    let (action, objects) = entry(&cache, 4).await;
    let gone = cache.object_key(objects[3]).expect("key");
    cache.objects().delete(&gone).await.expect("delete");
    store.fail(cache.object_key(objects[1]).expect("key"));

    assert!(!hit(&cache, action).await.expect("a miss, not an error"));
    for &object in &objects {
        let want = (object == objects[3]).then_some(UnreachableReason::Missing);
        assert_eq!(unreachable(&cache, object).await, want, "{object:?}");
    }
}
