//! `GetActionResult` checks the blobs of the entry's closure at read time, across the
//! owners of each blob (av2-12 / storage-ram-ack, R2).
//!
//! The map has three members and two owners per blob. Blobs live on members 1 (primary)
//! and 2 (replica); the action cache entry lives on member 3, which the mock reports a
//! closure for, as a node does that has the field.

use std::sync::Arc;

use kbf_map::Map;
use kbf_route::Router;
use kbf_storage::StorageError;
use kbf_storage::service::MAX_FIND_MISSING_DIGESTS;
use kbf_types::{Digest, DigestFunction};

use super::support::{
    Cell, Cluster, active, blobs_with_primary, blobs_written_to, map, router, view,
};

struct Rig {
    map: Arc<Map>,
    cluster: Arc<Cluster>,
    router: Router,
    cell: Arc<Cell>,
}

/// Members 1 to 3, two owners per blob.
fn rig() -> Rig {
    let map = map(1, 2, active(&[1, 2, 3]));
    let cluster = Cluster::of(3);
    let (router, cell) = router(&map, &cluster);
    Rig {
        map,
        cluster,
        router,
        cell,
    }
}

impl Rig {
    /// `count` closure blobs written to members 1 and 2, held by neither yet.
    fn blobs(&self, count: usize) -> Vec<Digest> {
        blobs_written_to(&self.map, &[1, 2], count)
            .into_iter()
            .map(|(_, digest)| digest)
            .collect()
    }

    /// An action whose entry is on member 3, naming `closure` and a result blob.
    fn action(&self, n: usize, closure: &[Digest]) -> Digest {
        let action = blobs_with_primary(&self.map, 3, n + 1)[n].1;
        let result = blobs_with_primary(&self.map, 3, n + 2)[n + 1].1;
        self.cluster
            .node(3)
            .hold_action_with_closure(action, result, closure);
        action
    }

    async fn hit(&self, action: &Digest) -> bool {
        self.router
            .action_result(action)
            .await
            .expect("an answer")
            .is_some()
    }

    /// FindMissing calls `member` has been sent, each as its number of digests.
    fn finds(&self, member: u64) -> Vec<usize> {
        self.cluster
            .node(member)
            .find_log()
            .iter()
            .map(Vec::len)
            .collect()
    }
}

fn hold(node: &super::support::Node, digests: &[Digest]) {
    for digest in digests {
        node.hold_as(*digest, b"x");
    }
}

/// Catches a GetActionResult that is a hit when a closure blob is gone from both owners:
/// the entry names two blobs, one held by both owners and one by neither; the hit becomes
/// a miss (the action runs again). With every blob held it is a hit. Mutant: no check at
/// read time.
#[tokio::test]
async fn a_closure_blob_gone_from_both_owners_is_a_miss() {
    let rig = rig();
    let blobs = rig.blobs(3);
    for node in [1, 2] {
        hold(&rig.cluster.node(node), &blobs[..2]);
    }
    let whole = rig.action(0, &blobs[..2]);
    let broken = rig.action(1, &blobs);

    assert!(rig.hit(&whole).await, "every blob is held");
    assert!(!rig.hit(&broken).await, "a blob neither owner holds");
}

/// Catches a check that asks the primary only: the blob is gone from the primary and held
/// by the replica, so the entry is still a hit, and the replica is asked only for what the
/// primary lacked. Mutant: a blob missing at the primary counts as gone.
#[tokio::test]
async fn a_blob_missing_on_the_primary_and_held_by_the_replica_is_a_hit() {
    let rig = rig();
    let blobs = rig.blobs(3);
    hold(&rig.cluster.node(1), &blobs[..2]);
    hold(&rig.cluster.node(2), &blobs);
    let action = rig.action(0, &blobs);

    assert!(rig.hit(&action).await);
    assert_eq!(rig.finds(1), [3]);
    assert_eq!(rig.finds(2), [1], "the replica is asked for the one the primary lacks");
}

/// Catches a check with no cache, or a fan-out per blob. Fifty blobs held by their
/// primary are checked with one FindMissing call to that primary, none to the replica; a
/// repeat makes no call at all. When the primary lacks them, the replica gets one call
/// and the primary still got one: two per owner group. Mutants: no cache (the repeat
/// asks again); a call per blob (fifty calls).
#[tokio::test]
async fn a_cached_check_makes_no_calls_and_an_uncached_one_at_most_two_per_group() {
    let rig = rig();
    let blobs = rig.blobs(50);
    hold(&rig.cluster.node(1), &blobs);
    let action = rig.action(0, &blobs);

    assert!(rig.hit(&action).await);
    assert_eq!(rig.finds(1), [50], "one call for the primary's whole group");
    assert!(rig.finds(2).is_empty());

    assert!(rig.hit(&action).await);
    assert!(rig.hit(&action).await);
    assert_eq!(rig.finds(1), [50], "a cached check asks nobody");
    assert!(rig.finds(2).is_empty());

    // Not cached: nobody has asked about these, and the primary lacks them.
    let others = {
        let all = blobs_written_to(&rig.map, &[1, 2], 100);
        all[50..].iter().map(|(_, d)| *d).collect::<Vec<_>>()
    };
    hold(&rig.cluster.node(2), &others);
    let second = rig.action(1, &others);
    assert!(rig.hit(&second).await);
    assert_eq!(rig.finds(1), [50, 50]);
    assert_eq!(rig.finds(2), [50], "one call to the replica for the lot");
}

/// Catches a call that is not split at the node's limit: a closure of one more digest
/// than a node takes in a call goes to it as two calls, the first full.
#[tokio::test]
async fn a_long_closure_is_asked_in_batches_the_node_takes() {
    let map = map(1, 1, active(&[1]));
    let cluster = Cluster::of(1);
    let (router, _) = router(&map, &cluster);
    let digests: Vec<Digest> = (0..=MAX_FIND_MISSING_DIGESTS as u64)
        .map(|i| {
            let mut hash = [0_u8; 32];
            hash[..8].copy_from_slice(&i.to_be_bytes());
            Digest::new(DigestFunction::Sha256, hash, 1)
        })
        .collect();
    hold(&cluster.node(1), &digests);
    let action = blobs_with_primary(&map, 1, 1)[0].1;
    cluster
        .node(1)
        .hold_action_with_closure(action, digests[0], &digests);

    assert!(router.action_result(&action).await.expect("an answer").is_some());
    let sizes: Vec<usize> = cluster.node(1).find_log().iter().map(Vec::len).collect();
    assert_eq!(sizes, [MAX_FIND_MISSING_DIGESTS, 1]);
}

/// Catches a stale cache: after the map's generation changes (here to one that leaves the
/// members as they were), a blob is verified again, and a blob that went away in between
/// is found gone. Mutant: entries survive a generation change.
#[tokio::test]
async fn a_generation_change_drops_what_was_verified() {
    let rig = rig();
    let blobs = rig.blobs(2);
    for node in [1, 2] {
        hold(&rig.cluster.node(node), &blobs);
    }
    let action = rig.action(0, &blobs);

    assert!(rig.hit(&action).await);
    assert!(rig.hit(&action).await);
    assert_eq!(rig.finds(1), [2], "the repeat is cached");

    for node in [1, 2] {
        rig.cluster.node(node).forget(&blobs[0]);
    }
    assert!(rig.hit(&action).await, "still inside the cached generation");

    rig.cell
        .set(view(&map(2, 2, active(&[1, 2, 3]))));
    assert!(!rig.hit(&action).await, "asked again at the new generation");
    assert_eq!(rig.finds(1), [2, 2]);
}

/// Catches a mandatory field: a node from before the closure was reported answers an
/// entry with none, and the router serves it as it did before, asking nobody. Mutant:
/// an absent closure treated as an empty one that fails, or as a closure to look up.
#[tokio::test]
async fn an_older_node_without_the_closure_gives_todays_answer() {
    let rig = rig();
    let blobs = rig.blobs(2);
    let action = blobs_with_primary(&rig.map, 3, 1)[0].1;
    rig.cluster.node(3).hold_action(action, blobs[0]);

    assert!(rig.hit(&action).await, "unchecked, as before");
    assert!(rig.finds(1).is_empty());
    assert!(rig.finds(2).is_empty());
}

/// Catches a miss answered, or remembered, for a blob no owner could be asked about. With
/// both owners down the entry is a miss (it is safe: the action runs again); nothing is
/// remembered, so once they answer and hold the blobs it is a hit at once. Mutant: an
/// unanswered owner counted as present (a hit) or as gone and cached.
#[tokio::test]
async fn blobs_nobody_could_answer_for_are_a_miss_and_not_remembered() {
    let rig = rig();
    let blobs = rig.blobs(1);
    let action = rig.action(0, &blobs);
    for node in [1, 2] {
        rig.cluster.node(node).set_down(true);
    }
    assert!(!rig.hit(&action).await);

    for node in [1, 2] {
        rig.cluster.node(node).set_down(false);
        hold(&rig.cluster.node(node), &blobs);
    }
    assert!(rig.hit(&action).await);
}

/// Catches a stale-map answer swallowed as a miss: a node that holds a newer map answers
/// the FindMissing with STALE_MAP, and the call, as every router call, fetches the map and
/// runs again.
#[tokio::test]
async fn a_stale_map_during_the_check_runs_the_call_again() {
    let rig = rig();
    let blobs = rig.blobs(1);
    for node in [1, 2] {
        hold(&rig.cluster.node(node), &blobs);
    }
    let action = rig.action(0, &blobs);
    rig.cluster
        .node(1)
        .fail_next(StorageError::StaleMap { current: 2 });
    rig.cell
        .answer_fetch(Ok(view(&map(2, 2, active(&[1, 2, 3])))));

    // The first call on node 1 is the check (the entry is on node 3).
    assert!(rig.hit(&action).await);
    assert_eq!(rig.cell.fetched(), [2]);
}
