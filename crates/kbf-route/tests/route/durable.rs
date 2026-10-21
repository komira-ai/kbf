//! Rule 2, barrier (a) (`docs/design/storage-ram-ack.md`): an action-cache entry is
//! committed only once every blob it names is on the disk of an owner of that blob. The
//! map has three members and two owners per blob. The entry is the action's owner, member
//! 3's, and names a result blob and outputs that members 1 and 2 own. The real node's
//! side of the wait (a held blob drained by the write-behind) is in
//! `crates/kbf-front/tests/rule2.rs`.

use std::sync::Arc;

use kbf_map::Map;
use kbf_meta::{ActionRecord, ActionWriteError, Closure, Role};
use kbf_route::Router;
use kbf_storage::StorageError;
use kbf_types::Digest;

use super::support::{Cluster, active, blobs_written_to, map, router};

struct Rig {
    map: Arc<Map>,
    cluster: Arc<Cluster>,
    router: Router,
    action: Digest,
}

fn rig() -> Rig {
    let map = map(1, 2, active(&[1, 2, 3]));
    let cluster = Cluster::of(3);
    let (router, _) = router(&map, &cluster);
    cluster.node(3).check_closure();
    let action = blobs_written_to(&map, &[3, 1], 1)[0].1;
    Rig {
        map,
        cluster,
        router,
        action,
    }
}

impl Rig {
    /// `count` blobs written to members 1 (primary) and 2, with their bytes.
    fn blobs(&self, count: usize) -> Vec<(Vec<u8>, Digest)> {
        blobs_written_to(&self.map, &[1, 2], count)
    }

    /// An entry whose result is `result` and whose closure is `closure`.
    fn record(result: Digest, closure: &[Digest]) -> ActionRecord {
        ActionRecord {
            result,
            closure: closure.iter().copied().collect::<Closure>(),
        }
    }

    async fn commit(&self, record: ActionRecord) -> Result<(), StorageError> {
        self.router
            .commit_with_closure(Role::Daemon, self.action, record)
            .await
    }

    fn commits(&self) -> Vec<Digest> {
        self.cluster.node(3).commits.lock().expect("lock").clone()
    }
}

/// Catches a commit that does not wait for a blob that is only in RAM (T3's refusal, T11's
/// twin): the result is on disk on both owners, the output is held in RAM by its primary
/// and nowhere else. The commit is refused UNAVAILABLE (never `Ok`), nothing is committed
/// on the action's owner, and the owners were asked the **waiting** durable question (not
/// the plain one, not the non-waiting one). Once the primary has written the blob the same
/// commit succeeds. Mutants: barrier (a) deleted (the commit succeeds on the RAM-only
/// blob); the barrier asks `find_durable` without waiting (the waiting log is empty);
/// asks the plain question (the blob counts).
#[tokio::test]
async fn a_commit_is_refused_while_a_closure_blob_is_only_in_ram_and_goes_through_once_written() {
    let rig = rig();
    let blobs = rig.blobs(2);
    let (result, output) = (&blobs[0], &blobs[1]);
    for node in [1, 2] {
        rig.cluster.node(node).hold(&result.0);
    }
    rig.cluster.node(1).hold(&output.0);
    rig.cluster.node(1).make_ram_only(&output.1);
    let record = Rig::record(result.1, &[output.1]);

    let error = rig.commit(record.clone()).await.expect_err("not durable");
    assert!(matches!(error, StorageError::Unavailable(_)), "{error:?}");
    assert!(rig.commits().is_empty(), "nothing committed");
    assert!(
        rig.cluster
            .node(1)
            .waiting_log()
            .iter()
            .any(|asked| asked.contains(&output.1)),
        "the primary was asked the waiting question about the output"
    );
    assert!(rig.cluster.node(1).durable_log().is_empty());

    rig.cluster.node(1).make_durable(&output.1);
    rig.commit(record).await.expect("written: committed");
    assert_eq!(rig.commits(), [rig.action]);
}

/// Catches a barrier that accepts the action's owner's own copy: the output is held (on
/// disk) by member 3 only, which is not an owner of it, so no reader would ask member 3
/// for it. The commit is refused `Absent` naming the output, nothing is committed.
/// Mutant: barrier (a) deleted (the commit succeeds and the entry is a permanent miss once
/// the owners lose the blob), or the barrier counts a non-owner.
#[tokio::test]
async fn a_blob_only_the_actions_owner_holds_does_not_satisfy_the_barrier() {
    let rig = rig();
    let blobs = rig.blobs(2);
    let (result, output) = (&blobs[0], &blobs[1]);
    for node in [1, 2] {
        rig.cluster.node(node).hold(&result.0);
    }
    rig.cluster.node(3).hold(&output.0);

    let error = rig
        .commit(Rig::record(result.1, &[output.1]))
        .await
        .expect_err("no owner holds it");
    assert!(
        matches!(&error, StorageError::ActionWrite(ActionWriteError::Absent(d)) if d == &[output.1]),
        "{error:?}"
    );
    assert!(rig.commits().is_empty());
}

/// Catches a barrier that asks only the primary: a blob durable on the replica (its
/// primary restarted and lost it) satisfies "durable on an owner". The commit succeeds,
/// the replica was asked only for what the primary lacked, and the primary was asked
/// first. Mutant: the barrier is `Router::find_durable` (primary only).
#[tokio::test]
async fn a_blob_durable_on_the_replica_only_satisfies_the_barrier() {
    let rig = rig();
    let blobs = rig.blobs(2);
    let (result, output) = (&blobs[0], &blobs[1]);
    rig.cluster.node(1).hold(&result.0);
    rig.cluster.node(2).hold(&output.0);
    rig.cluster.node(2).hold(&result.0);

    rig.commit(Rig::record(result.1, &[output.1]))
        .await
        .expect("durable on an owner");
    assert_eq!(rig.commits(), [rig.action]);
    let asked_first = rig.cluster.node(1).waiting_log();
    assert_eq!(asked_first.len(), 1);
    assert_eq!(asked_first[0].len(), 2, "one batched call");
    assert_eq!(rig.cluster.node(2).waiting_log(), [vec![output.1]]);
}

/// Catches a barrier that costs more than the doc says: five blobs all held on disk by
/// their primary are one batched waiting call to that primary and none to the replica.
/// Mutant: a call per blob, or the replica asked as well.
#[tokio::test]
async fn the_barrier_is_one_batched_call_per_primary() {
    let rig = rig();
    let blobs = rig.blobs(5);
    for (bytes, _) in &blobs {
        rig.cluster.node(1).hold(bytes);
    }
    let closure: Vec<Digest> = blobs[1..].iter().map(|(_, d)| *d).collect();

    rig.commit(Rig::record(blobs[0].1, &closure))
        .await
        .expect("all durable");
    let asked = rig.cluster.node(1).waiting_log();
    assert_eq!(asked.len(), 1, "one call");
    assert_eq!(asked[0].len(), 5);
    assert!(rig.cluster.node(2).waiting_log().is_empty());
    assert!(rig.cluster.node(1).durable_log().is_empty());
}

/// Catches an owner that cannot answer being counted as lacking the blob (the commit would
/// then be told the blob is absent): the primary holds the output on disk but fails the
/// barrier's call, the replica lacks it. The blob may be there, so the commit is refused
/// UNAVAILABLE, never `Absent`, and nothing is committed.
#[tokio::test]
async fn an_owner_that_cannot_answer_is_not_a_blob_that_is_absent() {
    let rig = rig();
    let blobs = rig.blobs(2);
    let (result, output) = (&blobs[0], &blobs[1]);
    for node in [1, 2] {
        rig.cluster.node(node).hold(&result.0);
    }
    rig.cluster.node(1).hold(&output.0);
    rig.cluster.node(1).fail_next(super::support::unavailable());

    let error = rig
        .commit(Rig::record(result.1, &[output.1]))
        .await
        .expect_err("cannot tell");
    assert!(matches!(error, StorageError::Unavailable(_)), "{error:?}");
    assert!(rig.commits().is_empty());
}

/// Catches a barrier that fails the commit for an owner that could not answer when another
/// owner holds every blob durably: the primary fails the barrier's call, the replica holds
/// the output, so the commit goes through. Mutant: an owner's failure fails the call
/// whatever the later owners say.
#[tokio::test]
async fn an_owner_that_cannot_answer_does_not_fail_a_blob_another_owner_holds() {
    let rig = rig();
    let blobs = rig.blobs(2);
    let (result, output) = (&blobs[0], &blobs[1]);
    for node in [1, 2] {
        rig.cluster.node(node).hold(&result.0);
    }
    rig.cluster.node(2).hold(&output.0);
    rig.cluster.node(1).fail_next(super::support::unavailable());

    rig.commit(Rig::record(result.1, &[output.1]))
        .await
        .expect("the replica holds it");
    assert_eq!(rig.commits(), [rig.action]);
}

/// Catches a barrier that does not follow the stale-map rule: the primary answers
/// STALE_MAP to the barrier's call, the router fetches the new map and the commit goes
/// through against it. Mutant: the stale answer counted as an owner that could not answer.
#[tokio::test]
async fn a_stale_map_in_the_barrier_is_followed() {
    let one = map(1, 2, active(&[1, 2, 3]));
    let two = map(2, 2, active(&[1, 2, 3]));
    let cluster = Cluster::of(3);
    let (router, cell) = router(&one, &cluster);
    cluster.node(3).check_closure();
    let action = blobs_written_to(&one, &[3, 1], 1)[0].1;
    let blobs = blobs_written_to(&one, &[1, 2], 2);
    // The blobs must be placed the same by the new map, which has the same members.
    for (bytes, _) in &blobs {
        cluster.node(1).hold(bytes);
    }
    cell.answer_fetch(Ok(super::support::view(&two)));
    // The first call to node 1 is the barrier's (the copies read, and node 3 is the
    // action's owner).
    cluster
        .node(1)
        .fail_next(StorageError::StaleMap { current: 2 });

    router
        .commit_with_closure(Role::Daemon, action, Rig::record(blobs[0].1, &[blobs[1].1]))
        .await
        .expect("committed against the newer map");
    assert_eq!(cell.fetched(), [2]);
}
