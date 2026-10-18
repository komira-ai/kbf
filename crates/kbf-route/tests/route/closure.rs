//! `GetActionResult` checks the blobs of the entry's closure at read time, across the
//! owners of each blob (av2-12 / storage-ram-ack, R2).
//!
//! The map has three members and two owners per blob. Blobs live on members 1 (primary)
//! and 2 (replica); the action cache entry lives on member 3, which the mock reports a
//! closure for, as a node does that has the field.

use std::sync::Arc;
use std::time::Duration;

use kbf_map::Map;
use kbf_route::{ClosureStats, Router};
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
    assert_eq!(
        rig.finds(2),
        [1],
        "the replica is asked for the one the primary lacks"
    );
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

    assert!(
        router
            .action_result(&action)
            .await
            .expect("an answer")
            .is_some()
    );
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

    rig.cell.set(view(&map(2, 2, active(&[1, 2, 3]))));
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

/// Catches a verified blob that never expires: a blob verified once is trusted for the
/// time to live and asked about again after it, so one that went away meanwhile (cold GC,
/// eviction) is found. Mutant: no time to live.
#[tokio::test(start_paused = true)]
async fn what_was_verified_is_asked_again_after_the_time_to_live() {
    let mut rig = rig();
    rig.router = rig.router.with_closure_cache(1000, Duration::from_secs(30));
    let blobs = rig.blobs(2);
    for node in [1, 2] {
        hold(&rig.cluster.node(node), &blobs);
    }
    let action = rig.action(0, &blobs);

    assert!(rig.hit(&action).await);
    tokio::time::advance(Duration::from_secs(29)).await;
    assert!(rig.hit(&action).await);
    assert_eq!(rig.finds(1), [2], "still inside the time to live");

    for node in [1, 2] {
        rig.cluster.node(node).forget(&blobs[1]);
    }
    tokio::time::advance(Duration::from_secs(2)).await;
    assert!(!rig.hit(&action).await, "asked again, and the blob is gone");
    assert_eq!(rig.finds(1), [2, 2]);
}

/// Catches an unbounded cache: with room for two entries, three verified blobs push the
/// least recently used one out, and only that one is asked about again. Mutant: no bound
/// (nothing is asked again), or a bound that forgets everything (all are).
#[tokio::test]
async fn the_cache_forgets_the_least_recently_used_blob_first() {
    let mut rig = rig();
    rig.router = rig.router.with_closure_cache(2, Duration::from_secs(60));
    let blobs = rig.blobs(3);
    hold(&rig.cluster.node(1), &blobs);
    let first = rig.action(0, &blobs[..2]);
    let second = rig.action(1, &blobs[2..]);

    assert!(rig.hit(&first).await);
    assert!(rig.hit(&second).await);
    assert_eq!(rig.finds(1), [2, 1]);

    assert!(rig.hit(&first).await);
    assert_eq!(
        rig.finds(1),
        [2, 1, 1],
        "the oldest was out, the other kept"
    );
}

/// Catches counters that do not say what happened: checks that hit, missed, or were not
/// made (no closure reported), the blobs the cache answered, and the FindMissing calls.
#[tokio::test]
async fn the_stats_count_checks_cache_hits_and_calls() {
    let rig = rig();
    let blobs = rig.blobs(4);
    hold(&rig.cluster.node(1), &blobs[..3]);
    let whole = rig.action(0, &blobs[..3]);
    let broken = rig.action(1, &blobs[3..]);
    let old = blobs_with_primary(&rig.map, 3, 6)[5].1;
    rig.cluster.node(3).hold_action(old, blobs[0]);

    let none = rig.router.closure_stats();
    assert_eq!(none, ClosureStats::default());
    assert!(
        none.cache_hit_ratio().abs() < 1e-9,
        "no ratio before any lookup"
    );
    assert!(
        none.rpcs_per_check().abs() < 1e-9,
        "no rate before any check"
    );

    assert!(rig.hit(&whole).await);
    assert!(rig.hit(&whole).await);
    assert!(!rig.hit(&broken).await);
    assert!(rig.hit(&old).await);

    let stats = rig.router.closure_stats();
    assert_eq!((stats.hit, stats.miss, stats.unchecked), (2, 1, 1));
    assert_eq!((stats.cache_hits, stats.cache_lookups), (3, 7));
    assert_eq!(
        stats.rpcs, 3,
        "one to the primary, then one to each for the lost one"
    );
    assert!((stats.cache_hit_ratio() - 3.0 / 7.0).abs() < 1e-9);
    assert!((stats.rpcs_per_check() - 1.0).abs() < 1e-9);
}

/// Catches a cache of size zero that remembers anyway: with no room, every check asks the
/// owners again. Mutant: the bound applied after the insert is read.
#[tokio::test]
async fn a_cache_with_no_room_remembers_nothing() {
    let mut rig = rig();
    rig.router = rig.router.with_closure_cache(0, Duration::from_secs(60));
    let blobs = rig.blobs(2);
    hold(&rig.cluster.node(1), &blobs);
    let action = rig.action(0, &blobs);

    assert!(rig.hit(&action).await);
    assert!(rig.hit(&action).await);
    assert_eq!(rig.finds(1), [2, 2]);
}

/// Catches a caller working from an older map overwriting what a newer map verified: a
/// check under generation 2 is remembered; a check under the older generation 1 asks again
/// and neither reads nor replaces those entries, so generation 2 is still cached after.
/// Mutant: entries of a newer generation dropped for an older caller.
#[tokio::test]
async fn a_caller_on_an_older_map_does_not_disturb_what_a_newer_one_verified() {
    let rig = rig();
    let blobs = rig.blobs(2);
    hold(&rig.cluster.node(1), &blobs);
    let action = rig.action(0, &blobs);
    let older = view(&rig.map);
    let newer = view(&map(2, 2, active(&[1, 2, 3])));

    rig.cell.set(newer.clone());
    assert!(rig.hit(&action).await);
    assert_eq!(rig.finds(1), [2]);

    rig.cell.set(older);
    assert!(rig.hit(&action).await);
    assert_eq!(rig.finds(1), [2, 2], "the older map's caller asks again");

    rig.cell.set(newer);
    assert!(rig.hit(&action).await);
    assert_eq!(rig.finds(1), [2, 2], "generation 2 is still remembered");
}

/// Members 1 to 3 with the boots `boots` (members 1, 2, 3 in that order).
fn booted(boots: [u64; 3]) -> Vec<kbf_map::Member> {
    let mut members = active(&[1, 2, 3]);
    for (member, boot) in members.iter_mut().zip(boots) {
        member.boot = boot;
    }
    members
}

/// Catches a cache that survives a restart that lost RAM-only data: member 1 restarts
/// (boot 1 to 2) and the map a caller holds says so, at the same generation. What member 1
/// was verified to hold is asked about again, and the blob it lost is found gone (member 2
/// does not hold it either). Mutant: entries keyed by generation alone.
#[tokio::test]
async fn a_member_whose_boot_changed_at_the_same_generation_is_asked_again() {
    let rig = rig();
    let blobs = rig.blobs(2);
    hold(&rig.cluster.node(1), &blobs);
    let action = rig.action(0, &blobs);

    assert!(rig.hit(&action).await);
    assert!(rig.hit(&action).await);
    assert_eq!(rig.finds(1), [2], "the repeat is cached");

    rig.cluster.node(1).forget(&blobs[0]);
    rig.cell.set(view(&map(1, 2, booted([2, 1, 1]))));
    assert!(
        !rig.hit(&action).await,
        "the restarted member lost the blob"
    );
    assert_eq!(
        rig.finds(1),
        [2, 2],
        "asked again though the generation held"
    );
}

/// Catches a boot change of one member dropping what other members verified: member 1
/// restarts, member 2 does not; blobs verified at member 2 stay cached. Mutant: any boot
/// change clears the whole cache.
#[tokio::test]
async fn a_boot_change_drops_only_that_members_entries() {
    let rig = rig();
    let blobs = rig.blobs(2);
    hold(&rig.cluster.node(2), &blobs);
    let action = rig.action(0, &blobs);

    assert!(rig.hit(&action).await);
    assert_eq!((rig.finds(1), rig.finds(2)), (vec![2], vec![2]));

    // Member 3 (the action cache owner) restarts; members 1 and 2 are unchanged.
    rig.cell.set(view(&map(1, 2, booted([1, 1, 2]))));
    assert!(rig.hit(&action).await);
    assert_eq!(
        (rig.finds(1), rig.finds(2)),
        (vec![2], vec![2]),
        "still cached"
    );
}

/// Catches the generation rule lost when the boot joined the key: the same boot at a new
/// generation drops the entries as before (asked again), and the same boot at the same
/// generation keeps them. Mutant: the generation dropped from the key.
#[tokio::test]
async fn the_same_boot_at_a_new_generation_is_asked_again() {
    let rig = rig();
    let blobs = rig.blobs(2);
    hold(&rig.cluster.node(1), &blobs);
    let action = rig.action(0, &blobs);

    assert!(rig.hit(&action).await);
    rig.cell.set(view(&map(1, 2, booted([1, 1, 1]))));
    assert!(rig.hit(&action).await);
    assert_eq!(rig.finds(1), [2], "same generation, same boot: cached");

    rig.cell.set(view(&map(2, 2, booted([1, 1, 1]))));
    assert!(rig.hit(&action).await);
    assert_eq!(rig.finds(1), [2, 2], "a new generation drops them");
}

/// Catches a caller with an older boot of a member overwriting a newer one: after a
/// check under boot 2, a caller whose map says boot 1 (same generation) asks again and
/// does not replace the entries, so boot 2 stays cached. Mutant: the older boot adopted.
#[tokio::test]
async fn a_caller_with_an_older_boot_does_not_disturb_a_newer_one() {
    let rig = rig();
    let blobs = rig.blobs(2);
    hold(&rig.cluster.node(1), &blobs);
    let action = rig.action(0, &blobs);
    let newer = view(&map(1, 2, booted([2, 1, 1])));

    rig.cell.set(newer.clone());
    assert!(rig.hit(&action).await);
    rig.cell.set(view(&map(1, 2, booted([1, 1, 1]))));
    assert!(rig.hit(&action).await);
    assert_eq!(rig.finds(1), [2, 2], "the older boot asks again");
    rig.cell.set(newer);
    assert!(rig.hit(&action).await);
    assert_eq!(rig.finds(1), [2, 2], "boot 2 is still remembered");
}

/// Catches a cache trusting a member whose boot is unknown (0, a node from before the
/// field): a restart cannot be told, so nothing is remembered and every check asks it,
/// whether the boot later becomes known or not. Once the boot is known (1) it is cached.
/// Mutant: boot 0 remembered like any other boot.
#[tokio::test]
async fn a_member_with_an_unknown_boot_is_never_remembered() {
    let rig = rig();
    let blobs = rig.blobs(2);
    hold(&rig.cluster.node(1), &blobs);
    let action = rig.action(0, &blobs);
    let unknown = view(&map(1, 2, booted([0, 1, 1])));

    rig.cell.set(unknown.clone());
    assert!(rig.hit(&action).await);
    assert!(rig.hit(&action).await);
    assert_eq!(rig.finds(1), [2, 2], "boot 0: every check asks");

    rig.cell.set(view(&map(1, 2, booted([1, 1, 1]))));
    assert!(rig.hit(&action).await);
    assert!(rig.hit(&action).await);
    assert_eq!(
        rig.finds(1),
        [2, 2, 2],
        "boot known: asked once more, then cached"
    );
}

/// Catches entries surviving a spell at an unknown boot: member 1 is verified at boot 5,
/// a caller then checks it at boot 0 (a downgraded binary), and the next check at boot 5
/// again must ask, since the member may have restarted and lost RAM-only data in between.
/// Mutant: boot 0 returns without dropping the member's entries.
#[tokio::test]
async fn a_spell_at_boot_zero_drops_what_boot_five_verified() {
    let rig = rig();
    let blobs = rig.blobs(2);
    hold(&rig.cluster.node(1), &blobs);
    let action = rig.action(0, &blobs);
    let five = view(&map(1, 2, booted([5, 1, 1])));

    rig.cell.set(five.clone());
    assert!(rig.hit(&action).await);
    assert!(rig.hit(&action).await);
    assert_eq!(rig.finds(1), [2], "boot 5 is cached");

    rig.cell.set(view(&map(2, 2, booted([0, 1, 1]))));
    assert!(rig.hit(&action).await);

    rig.cell.set(five);
    let before = rig.finds(1).len();
    assert!(rig.hit(&action).await);
    assert_eq!(
        rig.finds(1).len(),
        before + 1,
        "boot 5 again: asked, not trusted"
    );
}

/// Catches the mixed pair (an older generation with a newer boot) being read or added:
/// after a check at generation 2 boot 1, a caller at generation 1 boot 2 asks every time
/// and leaves what generation 2 verified in place. Mutant: the pair compared per field.
#[tokio::test]
async fn a_lower_generation_with_a_higher_boot_neither_reads_nor_adds() {
    let rig = rig();
    let blobs = rig.blobs(2);
    hold(&rig.cluster.node(1), &blobs);
    let action = rig.action(0, &blobs);
    let current = view(&map(2, 2, booted([1, 1, 1])));

    rig.cell.set(current.clone());
    assert!(rig.hit(&action).await);
    assert_eq!(rig.finds(1), [2]);

    rig.cell.set(view(&map(1, 2, booted([2, 1, 1]))));
    assert!(rig.hit(&action).await);
    assert!(rig.hit(&action).await);
    assert_eq!(
        rig.finds(1),
        [2, 2, 2],
        "the mixed pair is never read or added"
    );

    rig.cell.set(current);
    assert!(rig.hit(&action).await);
    assert_eq!(
        rig.finds(1),
        [2, 2, 2],
        "generation 2 boot 1 is still cached"
    );
}
