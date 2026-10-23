//! The node's side of the handoff fill: [`FillState`] and a [`round`] over a real cache, a
//! bucket in memory and a map the node holds.

use std::num::NonZeroU64;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::SystemTime;

use bytes::Bytes;
use kbf_front::{
    Cache, FlushReport, MemoryMetaLog, ObjectKeys, RebuildConfig, TailState, VerifiedBlob,
};
use kbf_map::{Map, Member, MemberId, MemberState, Rule};
use kbf_mapsync::MapHolder;
use kbf_meta::{NodeWriter, Retention};
use kbf_objstore::footers::FooterReads;
use kbf_objstore::{
    ByteRange, Capabilities, KeyPrefix, ListPage, ListToken, MemoryStore, ObjectKey, ObjectStore,
    ObjectStoreError, PageSize,
};
use kbf_storage::flush::FlushGate;
use kbf_storage::service::CatchUp;
use kbf_types::Digest;

use super::{FillState, FlushesAll, own_fp, round};
use crate::members::fill_fp;
use crate::node_map::NodeMap;

const ROOT: &str = "kbf/";

type Hook = Box<dyn FnOnce() + Send>;

struct Inner {
    store: MemoryStore,
    fail_list: AtomicBool,
    /// An object whose reads find it gone, however often it is listed.
    ghost: Mutex<Option<ObjectKey>>,
    /// Runs once, when the next listing starts.
    on_list: Mutex<Option<Hook>>,
}

/// One bucket for every cache of a test.
#[derive(Clone)]
struct Bucket(Arc<Inner>);

impl Bucket {
    fn new() -> Self {
        Self(Arc::new(Inner {
            store: MemoryStore::new(Capabilities::default()),
            fail_list: AtomicBool::new(false),
            ghost: Mutex::new(None),
            on_list: Mutex::new(None),
        }))
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
        let gone = self
            .0
            .ghost
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            == Some(key);
        if gone {
            return Err(ObjectStoreError::NotFound(key.clone()));
        }
        self.0.store.get_range(key, range).await
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
        let hook = self
            .0
            .on_list
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some(hook) = hook {
            hook();
        }
        if self.0.fail_list.load(Ordering::SeqCst) {
            return Err(ObjectStoreError::Protocol(
                "a scripted listing failure".into(),
            ));
        }
        self.0.store.list(prefix, after, max_keys).await
    }
}

fn keys(log: u64) -> ObjectKeys {
    ObjectKeys {
        legacy: KeyPrefix::new(format!("{ROOT}{log:016x}/")).expect("prefix"),
        root: KeyPrefix::new(ROOT).expect("root"),
    }
}

fn writer(member: u64) -> NodeWriter {
    let n = |v| NonZeroU64::new(v).expect("not zero");
    NodeWriter::new(n(member), n(1))
}

/// A gate that holds map generation `at` and flushes everything.
#[derive(Debug)]
struct At(u64);

impl FlushGate for At {
    fn flushes(&self, _: &Digest) -> bool {
        true
    }

    fn generation(&self) -> Option<u64> {
        Some(self.0)
    }
}

fn member(id: u64, state: MemberState) -> Member {
    Member {
        id: MemberId(id),
        addr: format!("192.0.2.{id}:8982"),
        zone: String::new(),
        weight: 1,
        state,
        boot: 1,
    }
}

fn map_of(generation: u64, ids: &[u64]) -> Map {
    let members = ids
        .iter()
        .map(|id| member(*id, MemberState::Active))
        .collect();
    Map::new(generation, members, Rule::Rendezvous, 1, 1, Vec::new()).expect("a map")
}

/// The node under test: member 7, a rebuilt index over an empty bucket, flushing under map 5.
struct Node {
    bucket: Bucket,
    cache: Cache<MemoryMetaLog, Bucket>,
    holder: Arc<MapHolder>,
    node: Arc<NodeMap>,
    fill: Arc<FillState>,
    tail: TailState,
}

async fn node() -> Node {
    let bucket = Bucket::new();
    let cache = Cache::open_node(
        MemoryMetaLog::new(Retention::default()),
        bucket.clone(),
        keys(0x33),
        writer(7),
    )
    .await
    .expect("opens")
    .with_flush_gate(Arc::new(At(5)));
    cache
        .rebuild_index(RebuildConfig::default())
        .await
        .expect("rebuild");
    let holder = Arc::new(MapHolder::new());
    let node = Arc::new(NodeMap::new(Arc::clone(&holder)));
    holder.offer(map_of(1, &[7, 8]));
    node.registered(MemberId(7));
    Node {
        bucket,
        cache,
        holder,
        node,
        fill: FillState::new(),
        tail: TailState::default(),
    }
}

impl Node {
    async fn round(&mut self, catch_up: &CatchUp) {
        round(
            &self.cache,
            &self.fill,
            &self.node,
            catch_up,
            &mut self.tail,
            FooterReads::default(),
        )
        .await;
    }

    /// Member 8 flushes `n` blobs to the bucket after this node's rebuild.
    async fn another_member_writes(&self, n: usize) -> Vec<Digest> {
        let other = Cache::open_node(
            MemoryMetaLog::new(Retention::default()),
            self.bucket.clone(),
            keys(0xbb),
            writer(8),
        )
        .await
        .expect("opens");
        let mut digests = Vec::new();
        for i in 0..n {
            let blob = VerifiedBlob::hashed(Bytes::from(format!("member 8 blob {i}")));
            digests.push(blob.digest());
            other.store_blobs(vec![blob]).await.expect("store");
        }
        digests
    }

    async fn missing(&self, digests: &[Digest]) -> usize {
        self.cache.find_missing(digests).await.expect("find").len()
    }
}

/// Catches: a node whose flush report never reaches its heartbeat (the cache is not asked),
/// or one that says a generation it was not asked about: with a gate holding map 5 and
/// nothing staged, a round makes the beat say it is clear to 5 and no backlog.
#[tokio::test]
async fn a_round_reports_the_flush_the_cache_says() {
    let mut n = node().await;
    assert_eq!(n.fill.beat().flush_clear_gen, None, "before any round");
    n.round(&CatchUp::caught_up()).await;
    let beat = n.fill.beat();
    assert_eq!(beat.flush_clear_gen, Some(5));
    assert_eq!((beat.staged_objects, beat.staged_bytes), (0, 0));
}

/// Catches: a node that acknowledges an epoch without having read what other members
/// flushed since its rebuild (its index lacks their blobs, and a promotion would make them
/// read as missing here), or one that forgets which placement it did it under. Member 8
/// flushes three blobs after the rebuild; the frontend announces epoch 2; after a round the
/// blobs are indexed, the acknowledgement is 2 and the fingerprint is that of the map with
/// this node assumed `Active`. Nothing is owed after, and the next beat repeats it.
#[tokio::test]
async fn a_round_makes_the_pass_it_is_owed_and_acknowledges_under_its_placement() {
    let mut n = node().await;
    let theirs = n.another_member_writes(3).await;
    assert_eq!(n.missing(&theirs).await, 3);
    n.fill.learn(2);
    assert_eq!(n.fill.owed(), Some(2));
    n.round(&CatchUp::caught_up()).await;
    assert_eq!(n.missing(&theirs).await, 0);
    let beat = n.fill.beat();
    let held = n.holder.current().expect("a map");
    assert_eq!(beat.tail_epoch_done, 2);
    assert_eq!(Some(beat.fill_fp), fill_fp(&held, MemberId(7)));
    assert_eq!(n.fill.owed(), None);
}

/// Catches: a node acknowledging a pass over a half-built index while it is rebuilding: the
/// objects the rebuild has yet to adopt would be reported as filled. The round after the
/// rebuild is done acknowledges.
#[tokio::test]
async fn no_pass_is_made_while_the_index_is_being_rebuilt() {
    let mut n = node().await;
    let theirs = n.another_member_writes(1).await;
    n.fill.learn(1);
    let rebuilding = CatchUp::rebuilding();
    n.round(&rebuilding).await;
    assert_eq!(n.fill.beat().tail_epoch_done, 0);
    assert_eq!(n.missing(&theirs).await, 1);
    // The flush is reported all the same: it does not depend on the rebuild.
    assert_eq!(n.fill.beat().flush_clear_gen, Some(5));
    rebuilding.finish();
    n.round(&rebuilding).await;
    assert_eq!(n.fill.beat().tail_epoch_done, 1);
}

/// Catches: an acknowledgement made by a node that does not know which placement it is in:
/// no map yet, or no member id yet (it has not registered). Neither is made.
#[tokio::test]
async fn no_pass_is_acknowledged_without_a_map_or_an_identity() {
    let mut n = node().await;
    n.fill.learn(1);
    let without_id = Arc::new(NodeMap::new(Arc::clone(&n.holder)));
    assert_eq!(own_fp(&without_id), None);
    n.node = without_id;
    n.round(&CatchUp::caught_up()).await;
    assert_eq!(n.fill.beat().tail_epoch_done, 0);
    let without_map = Arc::new(NodeMap::new(Arc::new(MapHolder::new())));
    without_map.registered(MemberId(7));
    assert_eq!(own_fp(&without_map), None);
    n.node = without_map;
    n.round(&CatchUp::caught_up()).await;
    assert_eq!(n.fill.beat().tail_epoch_done, 0);
}

/// Catches: a failed pass taken as a done one (the listing fails), and a node that gives up
/// after one: the next round, with the listing back, makes it and acknowledges.
#[tokio::test]
async fn a_failed_pass_is_not_acknowledged_and_is_made_again() {
    let mut n = node().await;
    n.fill.learn(1);
    n.bucket.0.fail_list.store(true, Ordering::SeqCst);
    n.round(&CatchUp::caught_up()).await;
    assert_eq!(n.fill.beat().tail_epoch_done, 0);
    assert_eq!(n.fill.owed(), Some(1));
    n.bucket.0.fail_list.store(false, Ordering::SeqCst);
    n.round(&CatchUp::caught_up()).await;
    assert_eq!(n.fill.beat().tail_epoch_done, 1);
}

/// Catches: an acknowledgement when no pass is quiet: an object that is listed and gone at
/// every read (a store that lags a delete) keeps every pass finding something new, so the
/// node never knows the listing stopped missing puts, and says nothing. Catches:
/// the first pass acknowledged without the repeat.
#[tokio::test]
async fn a_pass_that_never_goes_quiet_is_not_acknowledged() {
    let mut n = node().await;
    n.another_member_writes(1).await;
    let key = n
        .bucket
        .0
        .store
        .list(
            &KeyPrefix::default(),
            None,
            PageSize::new(10).expect("size"),
        )
        .await
        .expect("list")
        .objects
        .first()
        .expect("an object")
        .key
        .clone();
    *n.bucket.0.ghost.lock().expect("lock") = Some(key);
    n.fill.learn(1);
    n.round(&CatchUp::caught_up()).await;
    assert_eq!(n.fill.beat().tail_epoch_done, 0);
}

/// Catches: an acknowledgement for a placement that moved under the pass: a member joined
/// while the pass listed, so what the node held when it began says nothing about the map the
/// frontend judges by. Nothing is acknowledged that round; the next, under the new map,
/// acknowledges with the new fingerprint.
#[tokio::test]
async fn a_pass_begun_under_another_placement_is_not_acknowledged() {
    let mut n = node().await;
    n.fill.learn(1);
    let before = own_fp(&n.node).expect("a placement");
    let holder = Arc::clone(&n.holder);
    *n.bucket.0.on_list.lock().expect("lock") = Some(Box::new(move || {
        holder.offer(map_of(2, &[7, 8, 9]));
    }));
    n.round(&CatchUp::caught_up()).await;
    assert_eq!(n.fill.beat().tail_epoch_done, 0);
    let after = own_fp(&n.node).expect("a placement");
    assert_ne!(before, after);
    n.round(&CatchUp::caught_up()).await;
    let beat = n.fill.beat();
    assert_eq!((beat.tail_epoch_done, beat.fill_fp), (1, after));
}

/// Catches: a flush report that cannot be had (the metadata log failed) taken as the last
/// good one: the node must say it cannot say. `FillState` is told `None` after a report.
#[test]
fn a_flush_report_that_cannot_be_had_is_not_the_last_one() {
    let fill = FillState::new();
    fill.flushed(Some(FlushReport {
        clear_gen: Some(4),
        staged_objects: 2,
        staged_bytes: 20,
    }));
    let beat = fill.beat();
    assert_eq!(
        (beat.flush_clear_gen, beat.staged_objects, beat.staged_bytes),
        (Some(4), 2, 20)
    );
    fill.flushed(None);
    assert_eq!(fill.beat().flush_clear_gen, None);
    assert_eq!(fill.beat().staged_objects, 0);
}

/// Catches: the acknowledgement kept past what justifies it: a registration (a new boot has
/// done no pass), and an acknowledgement above the epoch the frontend now says (a frontend
/// whose state was reset counts from 1 again). And one kept too little: an older epoch never
/// replaces a newer acknowledgement.
#[test]
fn the_acknowledgement_goes_with_the_registration_and_with_a_reset_frontend() {
    let fill = FillState::new();
    fill.learn(5);
    fill.finish(5, 99);
    fill.finish(3, 77);
    let beat = fill.beat();
    assert_eq!((beat.tail_epoch_done, beat.fill_fp), (5, 99));
    fill.learn(2);
    assert_eq!(fill.beat().tail_epoch_done, 0, "above the frontend's epoch");
    assert_eq!(fill.owed(), None);
    fill.learn(5);
    assert_eq!(fill.beat().tail_epoch_done, 5);
    fill.registered();
    let beat = fill.beat();
    assert_eq!((beat.tail_epoch_done, beat.fill_fp), (0, 0));
    assert_eq!(fill.owed(), Some(5));
}

/// Catches: a registered node's gate that leaves some objects unflushed (it would be a
/// one-flusher gate, which no node installs yet: a replica would keep its staged copy for
/// good), or one that says a generation before it holds a map (the cache would date objects
/// by a map nobody held): it flushes every digest, and its generation is the held map's.
#[test]
fn the_registered_nodes_gate_flushes_everything_and_says_the_map_it_holds() {
    let holder = Arc::new(MapHolder::new());
    let gate = FlushesAll::new(Arc::new(NodeMap::new(Arc::clone(&holder))));
    let digest = kbf_segments::sha256(b"anything");
    assert!(gate.flushes(&digest));
    assert_eq!(gate.generation(), None);
    holder.offer(map_of(4, &[7]));
    assert_eq!(gate.generation(), Some(4));
    holder.offer(map_of(6, &[7, 8]));
    assert_eq!(gate.generation(), Some(6));
}

/// Catches: a round that does not ask the cache for its tier's health (the beat would say
/// nothing, or a value kept from before), and a cache with no tier read as numbers: the
/// node here has no tier, so after a round the beat's health is unknown, even when a test
/// had set values before it.
#[tokio::test]
async fn a_round_reports_the_tier_health_the_cache_says() {
    use crate::vitals::Vitals;

    let mut n = node().await;
    assert_eq!(n.fill.beat().vitals, Vitals::NONE, "before any round");
    n.fill.tiered(Vitals {
        used_bytes: Some(9),
        ..Vitals::NONE
    });
    assert_eq!(n.fill.beat().vitals.used_bytes, Some(9));
    n.round(&CatchUp::caught_up()).await;
    assert_eq!(n.fill.beat().vitals, Vitals::NONE, "a cache with no tier");
}
