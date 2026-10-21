//! The gate: which digests a node flushes under the map it holds: the rank-1 owner, and
//! a node that owns none of it (an orphan). Each test's comment names the defect it
//! catches.

use std::sync::{Arc, Mutex};

use futures::future::BoxFuture;
use kbf_map::{Map, Member, MemberId, MemberState, Rule};
use kbf_types::Digest;

use super::{FlushGate, OneFlusher};
use crate::replicate::MapSource;

fn digests() -> Vec<Digest> {
    (0..64u8).map(|i| kbf_segments::sha256(&[i])).collect()
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

fn map(generation: u64, members: &[(u64, MemberState)]) -> Arc<Map> {
    let members = members.iter().map(|&(id, s)| member(id, s)).collect();
    Arc::new(Map::new(generation, members, Rule::Rendezvous, 2, 1, Vec::new()).expect("map"))
}

/// A node's source: the map it holds, its id (if registered), and what a fetch brings.
struct Node {
    me: Option<MemberId>,
    held: Mutex<Option<Arc<Map>>>,
    /// The map a refetch delivers, if any.
    fetch: Mutex<Option<Arc<Map>>>,
}

impl Node {
    fn new(me: Option<u64>, held: Option<Arc<Map>>) -> Arc<Self> {
        Arc::new(Self {
            me: me.map(MemberId),
            held: Mutex::new(held),
            fetch: Mutex::new(None),
        })
    }
}

impl MapSource for Node {
    fn current(&self) -> Option<Arc<Map>> {
        self.held.lock().expect("lock").clone()
    }

    fn me(&self) -> Option<MemberId> {
        self.me
    }

    fn refetch(&self, _at_least: u64) -> BoxFuture<'_, Option<Arc<Map>>> {
        let fetched = self.fetch.lock().expect("lock").clone();
        if let Some(map) = &fetched {
            *self.held.lock().expect("lock") = Some(Arc::clone(map));
        }
        Box::pin(async move { fetched })
    }
}

const BOTH: [(u64, MemberState); 2] = [(1, MemberState::Active), (2, MemberState::Active)];

/// How many of the digests each of the two nodes flushes under `map`.
fn split(map: &Arc<Map>) -> (usize, usize) {
    let one = OneFlusher::new(Node::new(Some(1), Some(Arc::clone(map))));
    let two = OneFlusher::new(Node::new(Some(2), Some(Arc::clone(map))));
    let ds = digests();
    (
        ds.iter().filter(|d| one.flushes(d)).count(),
        ds.iter().filter(|d| two.flushes(d)).count(),
    )
}

/// Catches: both owners flushing (the double PUT), or neither: every digest has exactly
/// one flusher among the two members, and both members flush some.
#[test]
fn every_digest_has_exactly_one_flusher() {
    let map = map(5, &BOTH);
    let (a, b) = split(&map);
    assert_eq!(a + b, digests().len());
    assert!(a > 0 && b > 0, "{a} {b}");
}

/// Catches: a node that flushes without a map, or without knowing its id (a late flush
/// is cheap, a wrong one is a second PUT).
#[test]
fn a_node_without_a_map_or_an_id_flushes_nothing() {
    let ds = digests();
    let no_map = OneFlusher::new(Node::new(Some(1), None));
    let no_id = OneFlusher::new(Node::new(None, Some(map(5, &BOTH))));
    assert!(ds.iter().all(|d| !no_map.flushes(d) && !no_id.flushes(d)));
}

/// Catches: a replica that takes rank 1 while the primary is only `Down` (inside the
/// dwell): the primary keeps its slot, so the replica still holds. And that, once the
/// member has left the map, the replica flushes everything the primary never did.
#[test]
fn the_replica_takes_over_only_when_the_primary_has_left_the_map() {
    let down = map(6, &[(1, MemberState::Down), (2, MemberState::Active)]);
    let (a, b) = split(&down);
    assert_eq!(
        a + b,
        digests().len(),
        "a down primary still owns its slice"
    );
    let ds = digests();
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
    let two = OneFlusher::new(Node::new(Some(2), Some(alone)));
    assert!(ds.iter().all(|d| two.flushes(d)));
}

/// Catches: a node behind the map flushing as rank 1 after a caller showed it a newer
/// generation exists (a second flusher beside the real primary). It holds until its map
/// is at least as new, then judges by that one.
#[tokio::test]
async fn a_node_told_it_is_behind_flushes_nothing_until_it_has_the_new_map() {
    let old = map(5, &BOTH);
    let ds = digests();
    let source = Node::new(Some(1), Some(Arc::clone(&old)));
    let gate = OneFlusher::new(Arc::clone(&source) as Arc<dyn MapSource>);
    let mine: Vec<&Digest> = ds.iter().filter(|d| gate.flushes(d)).collect();
    assert!(!mine.is_empty());
    // A caller routed by generation 6; the node cannot get it.
    assert!(gate.refetch(6).await.is_none());
    assert!(mine.iter().all(|d| !gate.flushes(d)));
    // The map arrives: the same node is judged by it, member 1 now only Draining.
    let new = map(6, &[(1, MemberState::Draining), (2, MemberState::Active)]);
    *source.fetch.lock().expect("lock") = Some(Arc::clone(&new));
    assert_eq!(gate.refetch(6).await.expect("fetched").generation(), 6);
    // Member 1 is Draining under it: it owns nothing, so what it staged is an orphan and
    // it flushes all of it (member 2, the only writer, flushes it too).
    assert!(mine.iter().all(|d| gate.flushes(d)), "an orphan flushes");
    assert_eq!(gate.current().expect("held").generation(), 6);
    assert_eq!(gate.me(), Some(MemberId(1)));
}

/// The gate is `Debug` (the cache that holds it is), and says how new a map it was told of.
#[test]
fn the_gate_formats_with_the_newest_generation_it_was_told_of() {
    let gate = OneFlusher::new(Node::new(Some(1), None));
    assert!(format!("{gate:?}").contains("newest: 0"), "{gate:?}");
}

fn node_gate(me: u64, map: &Arc<Map>) -> Arc<OneFlusher> {
    OneFlusher::new(Node::new(Some(me), Some(Arc::clone(map))))
}

/// Catches: a Draining member stranding what it staged. It takes no writes, so it is in
/// no digest's owner list and never rank 1; under the old gate it flushed nothing and
/// its staged blobs waited for a flusher that does not hold them. Every digest it has
/// is now flushed by it; the Active member (rank 1 of all of them) flushes them too.
#[test]
fn a_draining_member_flushes_everything_it_staged() {
    let map = map(6, &[(1, MemberState::Draining), (2, MemberState::Active)]);
    let ds = digests();
    assert!(
        ds.iter()
            .all(|d| !map.write_owners(d).contains(&MemberId(1)))
    );
    let drain = node_gate(1, &map);
    let active = node_gate(2, &map);
    assert!(ds.iter().all(|d| drain.flushes(d) && active.flushes(d)));
}

/// Catches: an r = 1 joiner stranding what it staged. At r = 1 a joiner owns nothing
/// while a settled writer exists, so a blob a caller routed to it by an older map is an
/// orphan on it. The joiner flushes it; the settled owner (rank 1) keeps flushing its own.
#[test]
fn an_r1_joiner_flushes_what_it_staged_and_the_owner_still_flushes() {
    let members = vec![
        member(1, MemberState::Active),
        member(2, MemberState::Joining),
    ];
    let one = Arc::new(Map::new(5, members, Rule::Rendezvous, 1, 1, vec![]).expect("map"));
    let ds = digests();
    assert!(ds.iter().all(|d| one.write_owners(d) == [MemberId(1)]));
    let (joiner, owner) = (node_gate(2, &one), node_gate(1, &one));
    assert!(ds.iter().all(|d| joiner.flushes(d)), "an orphan flushes");
    assert!(ds.iter().all(|d| owner.flushes(d)), "rank 1 flushes");
}

/// Catches: the rank-1 gate loosened to "owner flushes" (every replica PUTs too: double
/// bytes, the defect the one-flusher rule prevents) or tightened back to rank 1 only
/// (a displaced node stranding its blobs). With three members at r = 2, every digest has
/// a rank-1 owner, a replica and a node owning none of it; for each, exactly the rank-1
/// owner and the non-owner flush, and the replica never does. All three roles occur for
/// every member, so no assertion below is vacuous.
#[test]
fn only_the_rank_one_owner_and_non_owners_flush() {
    let all = [
        (1, MemberState::Active),
        (2, MemberState::Active),
        (3, MemberState::Active),
    ];
    let map = map(5, &all);
    let gates: Vec<(u64, Arc<OneFlusher>)> = (1..=3).map(|m| (m, node_gate(m, &map))).collect();
    let (mut rank1, mut replica, mut orphan) = ([0usize; 3], [0usize; 3], [0usize; 3]);
    for d in digests() {
        let owners = map.write_owners(&d);
        assert_eq!(owners.len(), 2);
        for (m, gate) in &gates {
            let slot = usize::try_from(*m).expect("small") - 1;
            let me = MemberId(*m);
            if owners[0] == me {
                rank1[slot] += 1;
                assert!(gate.flushes(&d), "rank 1 of {d} is {me}");
            } else if owners[1] == me {
                replica[slot] += 1;
                assert!(!gate.flushes(&d), "the replica {me} of {d} must not flush");
            } else {
                orphan[slot] += 1;
                assert!(gate.flushes(&d), "{me} owns none of {d}");
            }
        }
    }
    for slot in 0..3 {
        assert!(rank1[slot] > 0 && replica[slot] > 0 && orphan[slot] > 0);
    }
}

/// Catches: a node the map no longer names (its row lapsed, or it left) keeping what it
/// staged: it owns nothing, so it flushes all of it.
#[test]
fn a_node_the_map_does_not_name_flushes_what_it_staged() {
    let map = map(5, &BOTH);
    let gone = node_gate(9, &map);
    assert!(digests().iter().all(|d| gone.flushes(d)));
}

/// Catches: the orphan rule letting a node that is behind the map flush. A node told a
/// newer generation exists holds even what it owns none of, until it holds that map.
#[tokio::test]
async fn an_orphan_behind_the_map_still_waits_for_the_new_map() {
    let draining = map(6, &[(1, MemberState::Draining), (2, MemberState::Active)]);
    let gate = node_gate(1, &draining);
    let ds = digests();
    assert!(ds.iter().all(|d| gate.flushes(d)));
    assert!(gate.refetch(7).await.is_none());
    assert!(ds.iter().all(|d| !gate.flushes(d)));
}

/// The gate reports the generation of the map it holds, and none without one: the
/// cache dates what it stages by it.
#[test]
fn the_gate_reports_the_generation_it_holds() {
    assert_eq!(node_gate(1, &map(5, &BOTH)).generation(), Some(5));
    let none = OneFlusher::new(Node::new(Some(1), None));
    assert_eq!(none.generation(), None);
}
