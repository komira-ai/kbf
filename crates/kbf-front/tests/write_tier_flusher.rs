//! One flusher (`docs/design/architecture-v2.md`, section 8) at the write tier: of the
//! two owners of a digest, only the primary PUTs its staged object. Two caches stand for
//! the two storage nodes of an R = 2 map, each with its own tier, log and store, each
//! holding a copy of the same blobs, each gated by `kbf_storage::flush::OneFlusher` over
//! the map it holds. A test's comment names the defect it catches.

mod common;
#[allow(dead_code)]
#[path = "write_tier/support.rs"]
mod support;

use std::sync::Arc;

use kbf_map::{Map, Member, MemberId, MemberState, Rule};
use kbf_meta::StagedState;
use kbf_storage::flush::OneFlusher;
use kbf_storage::replicate::MapSource;
use support::{Farm, SEGMENT, TestBlob, later};

const GIB: u64 = 1 << 30;

/// A node's view of the farm: its id and the map the test last gave it.
struct Node {
    me: MemberId,
    map: std::sync::Mutex<Option<Arc<Map>>>,
}

impl Node {
    fn new(me: u64, map: &Arc<Map>) -> Arc<Self> {
        Arc::new(Self {
            me: MemberId(me),
            map: std::sync::Mutex::new(Some(Arc::clone(map))),
        })
    }

    fn set(&self, map: &Arc<Map>) {
        *self.map.lock().expect("lock") = Some(Arc::clone(map));
    }
}

impl MapSource for Node {
    fn current(&self) -> Option<Arc<Map>> {
        self.map.lock().expect("lock").clone()
    }

    fn me(&self) -> Option<MemberId> {
        Some(self.me)
    }
}

fn member(id: u64, state: MemberState) -> Member {
    Member {
        id: MemberId(id),
        addr: format!("127.0.0.1:{id}"),
        zone: String::new(),
        weight: 1,
        state,
        boot: 0,
    }
}

fn two(generation: u64, state_of_one: MemberState) -> Arc<Map> {
    let members = vec![member(1, state_of_one), member(2, MemberState::Active)];
    Arc::new(Map::new(generation, members, Rule::Rendezvous, 2, 1, Vec::new()).expect("map"))
}

/// The two nodes of an R = 2 farm, and `n` blobs of one object each that member 1 is the
/// primary of, held by both.
struct Pair {
    primary: Farm,
    replica: Farm,
    primary_cache: support::TierCache,
    replica_cache: support::TierCache,
    replica_node: Arc<Node>,
    primary_node: Arc<Node>,
    primary_gate: Arc<OneFlusher>,
    blobs: Vec<TestBlob>,
}

impl Pair {
    async fn new(n: usize, map: &Arc<Map>) -> Self {
        let len = usize::try_from(SEGMENT).expect("size") + 1;
        let mut blobs = Vec::new();
        let mut seed = 1;
        while blobs.len() < n {
            let blob = TestBlob::new(seed, len);
            seed += 1;
            if map.write_owners(&blob.digest)[0] == MemberId(1) {
                blobs.push(blob);
            }
        }
        let (primary, replica) = (Farm::new(Default::default()), Farm::new(Default::default()));
        let primary_node = Node::new(1, map);
        let replica_node = Node::new(2, map);
        let primary_gate = OneFlusher::new(Arc::clone(&primary_node) as _);
        let primary_cache = primary
            .start(GIB)
            .await
            .with_flush_gate(Arc::clone(&primary_gate) as _);
        let replica_cache = replica
            .start(GIB)
            .await
            .with_flush_gate(OneFlusher::new(Arc::clone(&replica_node) as _));
        for cache in [&primary_cache, &replica_cache] {
            for b in &blobs {
                cache.store_blobs(vec![b.verified()]).await.expect("stored");
            }
        }
        Self {
            primary,
            replica,
            primary_cache,
            replica_cache,
            replica_node,
            primary_node,
            primary_gate,
            blobs,
        }
    }
}

/// Catches: the replica flushing its copy while the primary is live (a second PUT of
/// every blob into another object: double PUT bytes and cold-store cost). The replica
/// must PUT nothing, keep every object staged with its block, and still serve the blobs;
/// the primary PUTs each blob once.
#[tokio::test]
async fn a_replica_does_not_put_while_the_primary_is_live() {
    let map = two(5, MemberState::Active);
    let pair = Pair::new(3, &map).await;
    let round = pair.replica_cache.flush_tier(later()).await.expect("round");
    assert_eq!((round.flushed, round.retrying), (vec![], vec![]));
    assert_eq!(pair.replica.store.put_keys(), []);
    for b in &pair.blobs {
        let object = pair.replica.log.object_of(&b.digest);
        assert_eq!(
            pair.replica.log.staged_state(object),
            Some(StagedState::Sealed)
        );
        assert!(pair.replica.has_block(object) && !pair.replica.has_copy(object));
        let read = pair.replica_cache.read_blob(&b.digest).await.expect("read");
        assert_eq!(read, b.bytes);
    }
    let round = pair.primary_cache.flush_tier(later()).await.expect("round");
    assert_eq!(round.flushed.len(), 3);
    assert_eq!(pair.primary.store.put_keys().len(), 3);
    assert_eq!(pair.replica.store.put_keys(), []);
}

/// Catches: a replica that never takes over. The primary dies before it flushes; its
/// dwell passes and the map drops it; under the new map the replica is rank 1, flushes
/// what the primary never did, and no blob is lost. Inside the dwell (the primary only
/// `Down`) it still holds.
#[tokio::test]
async fn the_replica_flushes_when_the_primary_leaves_the_map() {
    let map = two(5, MemberState::Active);
    let pair = Pair::new(2, &map).await;
    // The primary is down but keeps its slot for the dwell.
    pair.replica_node.set(&two(6, MemberState::Down));
    let round = pair.replica_cache.flush_tier(later()).await.expect("round");
    assert_eq!(round.flushed, []);
    assert_eq!(pair.replica.store.put_keys(), []);
    // Past the dwell the member has left the map.
    let alone = Arc::new(
        Map::new(
            7,
            vec![member(2, MemberState::Active)],
            Rule::Rendezvous,
            1,
            1,
            vec![],
        )
        .expect("map"),
    );
    pair.replica_node.set(&alone);
    let round = pair.replica_cache.flush_tier(later()).await.expect("round");
    assert_eq!(round.flushed.len(), 2);
    assert_eq!(pair.replica.store.put_keys().len(), 2);
    for b in &pair.blobs {
        let object = pair.replica.log.object_of(&b.digest);
        assert_eq!(pair.replica.log.staged_state(object), None);
        assert_eq!(
            pair.replica_cache.read_blob(&b.digest).await.expect("read"),
            b.bytes
        );
    }
}

/// Catches: a node that is behind the map flushing as rank 1 beside the real primary.
/// A caller routed by generation 6 shows the primary (which holds generation 5) that its
/// map is old and cannot be fetched; its round PUTs nothing, though by the map it holds
/// it is the primary of every blob. Not one blob has two flushers at any point.
#[tokio::test]
async fn a_node_behind_the_map_does_not_become_a_second_flusher() {
    let map = two(5, MemberState::Active);
    let pair = Pair::new(2, &map).await;
    assert!(pair.primary_gate.refetch(6).await.is_none());
    let round = pair.primary_cache.flush_tier(later()).await.expect("round");
    assert_eq!(round.flushed, []);
    assert_eq!(pair.primary.store.put_keys(), []);
    // Generation 6 reaches it and places the blobs on the same primary again: it
    // flushes them, once.
    pair.primary_node.set(&two(6, MemberState::Active));
    let round = pair.primary_cache.flush_tier(later()).await.expect("round");
    assert_eq!(round.flushed.len(), 2);
    assert_eq!(pair.primary.store.put_keys().len(), 2);
}

/// A gate that flushes only the digests it was given.
#[derive(Debug)]
struct Only(Vec<kbf_types::Digest>);

impl kbf_storage::flush::FlushGate for Only {
    fn flushes(&self, d: &kbf_types::Digest) -> bool {
        self.0.contains(d)
    }
}

/// Catches: a flush queue that takes its round's oldest sealed objects before it drops
/// the held ones: with more held objects than a round takes (four), the one object this
/// node flushes would never be reached.
#[tokio::test]
async fn held_objects_do_not_starve_the_one_this_node_flushes() {
    let farm = Farm::new(Default::default());
    let len = usize::try_from(SEGMENT).expect("size") + 1;
    let blobs: Vec<TestBlob> = (1..=5).map(|s| TestBlob::new(s, len)).collect();
    let cache = farm
        .start(GIB)
        .await
        .with_flush_gate(Arc::new(Only(vec![blobs[4].digest])));
    for b in &blobs {
        cache.store_blobs(vec![b.verified()]).await.expect("stored");
    }
    let last = farm.log.object_of(&blobs[4].digest);
    let round = cache.flush_tier(later()).await.expect("round");
    assert_eq!(round.flushed, [last]);
    assert_eq!(farm.store.put_keys().len(), 1);
}

/// One staged object holds blobs of both ranks: it is flushed whole when this node
/// flushes any of its digests (the others ride along, a duplicate PUT bounded by the
/// object), and held when it flushes none. Not flushing it would leave this node's own
/// digests unflushed for good.
#[tokio::test]
async fn an_object_with_one_digest_to_flush_is_flushed_whole() {
    let farm = Farm::new(Default::default());
    let blobs = support::blobs(1, 2, 4096);
    let cache = farm
        .start(GIB)
        .await
        .with_flush_gate(Arc::new(Only(vec![blobs[1].digest])));
    cache
        .store_blobs(support::verified(&blobs))
        .await
        .expect("stored");
    let object = farm.log.object_of(&blobs[0].digest);
    assert_eq!(object, farm.log.object_of(&blobs[1].digest));
    let round = cache.flush_tier(later()).await.expect("round");
    assert_eq!(round.flushed, [object]);
}

/// Catches: a Draining node stranding what it staged. The map the node holds moves
/// member 1 (the primary of every blob, holding all of them staged) to Draining: it
/// owns none of them now, so its round flushes all of them (they were held for good
/// before), the blobs stay readable from the store afterwards, and the successor that
/// flushes the same blobs too only makes a second object of each: the node's PUT is
/// not undone by it, and both copies read back.
#[tokio::test]
async fn a_draining_node_flushes_what_it_staged() {
    let map = two(5, MemberState::Active);
    let pair = Pair::new(3, &map).await;
    pair.primary_node.set(&two(6, MemberState::Draining));
    let round = pair.primary_cache.flush_tier(later()).await.expect("round");
    assert_eq!(round.flushed.len(), 3, "an orphan flushes");
    assert_eq!(pair.primary.store.put_keys().len(), 3);
    for b in &pair.blobs {
        let object = pair.primary.log.object_of(&b.digest);
        assert_eq!(
            pair.primary.log.staged_state(object),
            None,
            "no longer staged"
        );
        let read = pair.primary_cache.read_blob(&b.digest).await.expect("read");
        assert_eq!(read, b.bytes);
    }
    // Member 2 is the sole writer under generation 6 and so rank 1: it flushes its own
    // copies as well, into its own objects; every blob reads back from both.
    pair.replica_node.set(&two(6, MemberState::Draining));
    let round = pair.replica_cache.flush_tier(later()).await.expect("round");
    assert_eq!(round.flushed.len(), 3);
    let (a, b) = (pair.primary.store.put_keys(), pair.replica.store.put_keys());
    assert_eq!((a.len(), b.len()), (3, 3));
    for b in &pair.blobs {
        let read = pair.replica_cache.read_blob(&b.digest).await.expect("read");
        assert_eq!(read, b.bytes);
    }
}

/// Catches: an r = 1 joiner stranding what it staged (it owns nothing at r = 1 while a
/// settled writer exists, so under the old gate nothing it staged was ever flushed), and
/// the settled owner losing its flush while the joiner gets its own. Both nodes hold
/// the same blobs staged; each flushes all of them.
#[tokio::test]
async fn an_r1_joiner_and_the_owner_both_flush() {
    let members = vec![
        member(1, MemberState::Active),
        member(2, MemberState::Joining),
    ];
    let map = Arc::new(Map::new(5, members, Rule::Rendezvous, 1, 1, Vec::new()).expect("map"));
    let pair = Pair::new(2, &map).await;
    // The joiner (node 2) holds blobs a caller routed to it by an older map.
    let round = pair.replica_cache.flush_tier(later()).await.expect("round");
    assert_eq!(round.flushed.len(), 2, "the joiner's orphans flush");
    assert_eq!(pair.replica.store.put_keys().len(), 2);
    let round = pair.primary_cache.flush_tier(later()).await.expect("round");
    assert_eq!(round.flushed.len(), 2, "rank 1 still flushes");
    assert_eq!(pair.primary.store.put_keys().len(), 2);
}

/// Catches: the replica, an owner but not rank 1, flushing once the owner set is
/// unchanged by the new gate: it keeps its objects staged, whole, and flushes none while
/// a node that owns none of the digests is told the same map and flushes all of them.
#[tokio::test]
async fn a_replica_holds_while_a_non_owner_flushes_the_same_blobs() {
    let map = two(5, MemberState::Active);
    let pair = Pair::new(2, &map).await;
    let round = pair.replica_cache.flush_tier(later()).await.expect("round");
    assert_eq!(round.flushed, []);
    // A map in which node 2 is not a member of the owner lists at all (member 3 is).
    let members = vec![
        member(1, MemberState::Active),
        member(3, MemberState::Active),
    ];
    let other = Arc::new(Map::new(6, members, Rule::Rendezvous, 2, 1, vec![]).expect("map"));
    pair.replica_node.set(&other);
    let round = pair.replica_cache.flush_tier(later()).await.expect("round");
    assert_eq!(round.flushed.len(), 2, "owning none of it, node 2 flushes");
}

/// A gate that flushes everything and holds whatever generation the test sets (0: none).
#[derive(Debug, Default)]
struct Switch(std::sync::atomic::AtomicU64);

impl Switch {
    fn set(&self, generation: u64) {
        self.0
            .store(generation, std::sync::atomic::Ordering::SeqCst);
    }
}

impl kbf_storage::flush::FlushGate for Switch {
    fn flushes(&self, _: &kbf_types::Digest) -> bool {
        true
    }

    fn generation(&self) -> Option<u64> {
        match self.0.load(std::sync::atomic::Ordering::SeqCst) {
            0 => None,
            g => Some(g),
        }
    }
}

/// A map in which member 1 alone writes, so it is rank 1 of every digest.
fn solo(generation: u64) -> Arc<Map> {
    let members = vec![member(1, MemberState::Active)];
    Arc::new(Map::new(generation, members, Rule::Rendezvous, 1, 1, Vec::new()).expect("map"))
}

fn big(seed: u64) -> TestBlob {
    TestBlob::new(seed, usize::try_from(SEGMENT).expect("size") + 1)
}

/// Catches: a clear generation that is not the held map's when nothing is staged (a
/// node with an empty tier would hold a promotion back, or claim a generation it has
/// not seen), and one that does not follow the map as it moves.
#[tokio::test]
async fn with_nothing_staged_the_clear_generation_is_the_held_one() {
    let farm = Farm::new(Default::default());
    let node = Node::new(1, &solo(5));
    let cache = farm
        .start(GIB)
        .await
        .with_flush_gate(OneFlusher::new(Arc::clone(&node) as _));
    let report = cache.flush_report().await.expect("report");
    assert_eq!(
        (report.clear_gen, report.staged_objects, report.staged_bytes),
        (Some(5), 0, 0)
    );
    node.set(&solo(7));
    assert_eq!(
        cache.flush_report().await.expect("report").clear_gen,
        Some(7)
    );
}

/// Catches: a clear generation that races ahead of the objects staged before it: it is
/// the generation the OLDEST unflushed object was staged under, not the held one, and
/// not the generation at which the report is asked. Two blobs staged under 5 hold it at
/// 5 while the map moves to 7 and a third is staged under 7; once the flush round has
/// flushed them all it is the held 7, with an empty backlog.
#[tokio::test]
async fn an_unflushed_object_holds_the_clear_generation_at_its_staging_generation() {
    let farm = Farm::new(Default::default());
    let node = Node::new(1, &solo(5));
    let cache = farm
        .start(GIB)
        .await
        .with_flush_gate(OneFlusher::new(Arc::clone(&node) as _));
    for seed in [1, 2] {
        cache
            .store_blobs(vec![big(seed).verified()])
            .await
            .expect("stored");
    }
    let report = cache.flush_report().await.expect("report");
    assert_eq!((report.clear_gen, report.staged_objects), (Some(5), 2));
    assert!(report.staged_bytes > 2 * SEGMENT, "{report:?}");
    node.set(&solo(7));
    assert_eq!(
        cache.flush_report().await.expect("report").clear_gen,
        Some(5)
    );
    cache
        .store_blobs(vec![big(3).verified()])
        .await
        .expect("stored");
    let report = cache.flush_report().await.expect("report");
    assert_eq!((report.clear_gen, report.staged_objects), (Some(5), 3));
    let round = cache.flush_tier(later()).await.expect("round");
    assert_eq!(round.flushed.len(), 3);
    let report = cache.flush_report().await.expect("report");
    assert_eq!(
        (report.clear_gen, report.staged_objects, report.staged_bytes),
        (Some(7), 0, 0)
    );
}

/// Catches: a replica's clear generation stuck for good behind objects only the primary
/// flushes (it can never flush them, so the fence would never close), and the opposite,
/// forgetting them once they become this node's to flush. The replica's three objects
/// are backlog but do not hold the generation while the primary is live; when the map
/// leaves the replica as the one owner they do, at the generation they were staged
/// under, until it flushes them.
#[tokio::test]
async fn objects_only_the_primary_flushes_do_not_hold_a_replicas_clear_generation() {
    let map = two(5, MemberState::Active);
    let pair = Pair::new(3, &map).await;
    let report = pair.replica_cache.flush_report().await.expect("report");
    assert_eq!((report.clear_gen, report.staged_objects), (Some(5), 3));
    assert!(report.staged_bytes > 0);
    pair.replica_node.set(&two(9, MemberState::Active));
    let report = pair.replica_cache.flush_report().await.expect("report");
    assert_eq!((report.clear_gen, report.staged_objects), (Some(9), 3));
    let alone = Arc::new(
        Map::new(
            11,
            vec![member(2, MemberState::Active)],
            Rule::Rendezvous,
            1,
            1,
            vec![],
        )
        .expect("map"),
    );
    pair.replica_node.set(&alone);
    let report = pair.replica_cache.flush_report().await.expect("report");
    assert_eq!((report.clear_gen, report.staged_objects), (Some(5), 3));
    pair.replica_cache.flush_tier(later()).await.expect("round");
    let report = pair.replica_cache.flush_report().await.expect("report");
    assert_eq!((report.clear_gen, report.staged_objects), (Some(11), 0));
}

/// Catches: an object staged while the node held no map (or one from before this
/// process started, which nothing dated) being counted as staged under the map the node
/// holds when asked: it is the oldest, generation 0, until it is flushed.
#[tokio::test]
async fn an_object_staged_with_no_known_generation_holds_the_clear_generation_at_zero() {
    let farm = Farm::new(Default::default());
    let gate = Arc::new(Switch::default());
    let cache = farm
        .start(GIB)
        .await
        .with_flush_gate(Arc::clone(&gate) as _);
    cache
        .store_blobs(vec![big(1).verified()])
        .await
        .expect("stored");
    gate.set(9);
    let report = cache.flush_report().await.expect("report");
    assert_eq!((report.clear_gen, report.staged_objects), (Some(0), 1));
    cache.flush_tier(later()).await.expect("round");
    assert_eq!(
        cache.flush_report().await.expect("report").clear_gen,
        Some(9)
    );
}

/// Catches: a node without a gate, or whose gate holds no map, reporting a generation:
/// it says it cannot say (`None`), never a number a fence would trust. The backlog is
/// still counted.
#[tokio::test]
async fn a_node_that_holds_no_generation_cannot_say() {
    let farm = Farm::new(Default::default());
    let cache = farm.start(GIB).await;
    cache
        .store_blobs(vec![big(1).verified()])
        .await
        .expect("stored");
    let report = cache.flush_report().await.expect("report");
    assert_eq!((report.clear_gen, report.staged_objects), (None, 1));
    let gate = Arc::new(Switch::default());
    let blind = Farm::new(Default::default())
        .start(GIB)
        .await
        .with_flush_gate(gate);
    assert_eq!(blind.flush_report().await.expect("report").clear_gen, None);
}

/// Catches: a cache without a tier reporting a backlog, or none of its generation: it
/// stages nothing, so everything it ever staged is flushed at the held generation.
#[tokio::test]
async fn a_cache_without_a_tier_is_clear_at_the_held_generation() {
    let cache =
        kbf_front::Cache::memory().with_flush_gate(OneFlusher::new(Node::new(1, &solo(4)) as _));
    let report = cache.flush_report().await.expect("report");
    assert_eq!(
        (report.clear_gen, report.staged_objects, report.staged_bytes),
        (Some(4), 0, 0)
    );
}

/// Catches: the backlog gauges missing from a scrape or not following the staged set:
/// after a round on the replica that flushes nothing they count the three staged
/// objects (the sum of their committed bytes), and on the primary, once a round has
/// flushed them and the next has found none, zero.
#[tokio::test]
async fn the_staged_backlog_is_scraped() {
    let map = two(5, MemberState::Active);
    let pair = Pair::new(3, &map).await;
    pair.replica_cache.flush_tier(later()).await.expect("round");
    let metrics = pair.replica_cache.render_metrics();
    let report = pair.replica_cache.flush_report().await.expect("report");
    assert!(
        metrics.contains("\nkbf_write_tier_staged_objects 3\n"),
        "{metrics}"
    );
    let bytes = format!("\nkbf_write_tier_staged_bytes {}\n", report.staged_bytes);
    assert!(metrics.contains(&bytes), "{metrics}");
    // The gauges are what a round found at its start: the second round finds none.
    pair.primary_cache.flush_tier(later()).await.expect("round");
    pair.primary_cache.flush_tier(later()).await.expect("round");
    let metrics = pair.primary_cache.render_metrics();
    assert!(
        metrics.contains("\nkbf_write_tier_staged_objects 0\n"),
        "{metrics}"
    );
    assert!(
        metrics.contains("\nkbf_write_tier_staged_bytes 0\n"),
        "{metrics}"
    );
}

/// Catches: a block that takes more blobs being re-dated by each commit: it is as old as
/// its first blob. A small blob staged under generation 5 and another into the same open
/// block under 7 leave one object, still held at 5.
#[tokio::test]
async fn a_block_is_as_old_as_its_first_blob() {
    let farm = Farm::new(Default::default());
    let node = Node::new(1, &solo(5));
    let cache = farm
        .start(GIB)
        .await
        .with_flush_gate(OneFlusher::new(Arc::clone(&node) as _));
    let blobs = support::blobs(1, 2, 4096);
    cache
        .store_blobs(vec![blobs[0].verified()])
        .await
        .expect("stored");
    node.set(&solo(7));
    cache
        .store_blobs(vec![blobs[1].verified()])
        .await
        .expect("stored");
    assert_eq!(
        farm.log.object_of(&blobs[0].digest),
        farm.log.object_of(&blobs[1].digest),
        "one open block"
    );
    let report = cache.flush_report().await.expect("report");
    assert_eq!((report.clear_gen, report.staged_objects), (Some(5), 1));
}

/// Catches: a restart losing the dates of what is staged and then claiming the held
/// generation: the objects a process finds at its start were staged by an earlier one, at
/// a generation it cannot tell, so they hold the clear generation at 0 until flushed.
#[tokio::test]
async fn objects_found_at_a_restart_hold_the_clear_generation_at_zero() {
    let farm = Farm::new(Default::default());
    let cache = farm
        .start(GIB)
        .await
        .with_flush_gate(OneFlusher::new(Node::new(1, &solo(5)) as _));
    cache
        .store_blobs(vec![big(1).verified()])
        .await
        .expect("stored");
    assert_eq!(
        cache.flush_report().await.expect("report").clear_gen,
        Some(5)
    );
    drop(cache);
    let fs = farm.fs.restart();
    let (cache, startup) = support::start(&fs, &farm.log, &farm.store, GIB, None)
        .await
        .expect("restart");
    assert_eq!(startup.recovery.sealed.len(), 1, "{startup:?}");
    let cache = cache.with_flush_gate(OneFlusher::new(Node::new(1, &solo(9)) as _));
    let report = cache.flush_report().await.expect("report");
    assert_eq!((report.clear_gen, report.staged_objects), (Some(0), 1));
    let round = cache.flush_tier(later()).await.expect("round");
    assert_eq!(round.flushed.len(), 1);
    assert_eq!(
        cache.flush_report().await.expect("report").clear_gen,
        Some(9)
    );
}

/// Catches: a report that answers when the log could not (a node saying "clear" because
/// it could not look): the failed read is an error, not a number, and the next report,
/// once the log answers, is the real one.
#[tokio::test]
async fn a_report_the_log_cannot_answer_is_an_error() {
    let farm = Farm::new(Default::default());
    let cache = farm
        .start(GIB)
        .await
        .with_flush_gate(OneFlusher::new(Node::new(1, &solo(5)) as _));
    cache
        .store_blobs(vec![big(1).verified()])
        .await
        .expect("stored");
    farm.log.fail_next_read();
    assert!(cache.flush_report().await.is_err());
    let report = cache.flush_report().await.expect("report");
    assert_eq!((report.clear_gen, report.staged_objects), (Some(5), 1));
}

/// Catches: the staging dates growing with every object ever staged when nothing asks for
/// a flush report (a node serving writes whose report nobody polls): the flush round, which
/// already reads the staged set, drops the date of every object that left it. Two objects
/// staged are two dates; once the round has flushed them there are none, with no
/// `flush_report` call in between.
#[tokio::test]
async fn a_flush_round_drops_the_dates_of_objects_that_are_no_longer_staged() {
    let farm = Farm::new(Default::default());
    let node = Node::new(1, &solo(5));
    let cache = farm
        .start(GIB)
        .await
        .with_flush_gate(OneFlusher::new(Arc::clone(&node) as _));
    assert_eq!(cache.dated_objects(), 0);
    for seed in [1, 2] {
        cache
            .store_blobs(vec![big(seed).verified()])
            .await
            .expect("stored");
    }
    assert_eq!(cache.dated_objects(), 2);
    let round = cache.flush_tier(later()).await.expect("round");
    assert_eq!(round.flushed.len(), 2);
    assert_eq!(cache.dated_objects(), 0);
}
