//! The acknowledgement point behind `--ram-ack-bytes` over a chain of two real caches
//! (`docs/design/storage-ram-ack.md`, "The new acknowledgement point" and the crash
//! tests): a write is answered once both owners hold the blob verified in RAM, the drain
//! writes it to each disk afterwards, and every case that cannot be answered from RAM is
//! written through and counted for its reason.
//!
//! Every wait is an event: a writer parked in `sync` (`Counting::wait_waiting`), a commit
//! parked at the log's hold (`Log::wait_held`), the pool's drain hook, a barrier parked on
//! a held blob (`Cache::parked_for_test`). "Acknowledged before the disk" is asserted with
//! the sync gates closed on both nodes: the write returns, no writer has begun, no sync has
//! happened, the index lacks the blob. No test sleeps or asserts how long anything took.

#[path = "support/chain.rs"]
#[allow(dead_code, reason = "each test binary uses part of the rig")]
mod chain;
mod common;
#[path = "support/rig.rs"]
#[allow(
    dead_code,
    reason = "shared with the write-pool tests, which use the rest of it"
)]
mod rig;
#[path = "write_tier/support.rs"]
#[allow(
    dead_code,
    reason = "shared with write_tier.rs, which uses the rest of it"
)]
mod support;

use std::sync::Arc;

use std::sync::Mutex;
use std::sync::mpsc;
use std::time::Duration;

use chain::{Cluster, Setup, metric, parked_in_sync, read_from, within};
use kbf_front::DrainHook;
use kbf_storage::repair::Confirmed;
use kbf_storage::{Storage, StorageError};
use support::TestBlob;
use tokio::time::Instant;

const KIB4: usize = 4096;
const UNWRITTEN: &str = "kbf_storage_ram_unwritten_bytes";
const UNCONFIRMED: &str = "kbf_storage_ram_unconfirmed_blobs";
/// `--ram-peer-grace` at its default.
const GRACE: Duration = Duration::from_secs(5);

fn through(reason: &str) -> String {
    format!("kbf_storage_ram_writethrough_total{{reason=\"{reason}\"}}")
}

/// Node `i`'s confirmation, as its repair worker's tick makes it: the first pass notes that
/// its own copy is on its disk and starts the grace; the second, once the grace is over,
/// asks the peer.
async fn confirm_after_drain(cluster: &Cluster, i: usize) -> Confirmed {
    let worker = &cluster.nodes[i].up().worker;
    let now = Instant::now();
    let first = worker.confirm_pass(now).await;
    assert_eq!(
        (first.durable, first.owed),
        (0, 0),
        "nothing is asked before the grace"
    );
    worker.confirm_pass(now + GRACE).await
}

/// Starts `write` of `blob` to node `to` as a task, so a test can hold it at a gate.
fn writing(
    cluster: &Cluster,
    to: usize,
    blob: &TestBlob,
) -> tokio::task::JoinHandle<Result<kbf_types::Digest, StorageError>> {
    let client = cluster.client(to);
    let bytes = blob.bytes.clone();
    tokio::spawn(async move { client.put(bytes).await })
}

/// Returns once `n` writers are parked in `sync` on node `i`; fails the test, rather than
/// waiting for ever, if `write` is answered first (it did not wait for the disk).
async fn parked_unanswered(
    cluster: &Cluster,
    i: usize,
    n: usize,
    write: &mut tokio::task::JoinHandle<Result<kbf_types::Digest, StorageError>>,
) {
    tokio::select! {
        () = parked_in_sync(cluster, i, n) => {}
        answered = write => panic!("answered before node {i}'s disk had it: {answered:?}"),
    }
}

/// One drain pass on node `i`, run as a task so a test can hold it at a gate.
fn drain(cluster: &Cluster, i: usize) -> tokio::task::JoinHandle<kbf_front::DrainReport> {
    let cache = Arc::clone(cluster.nodes[i].cache());
    tokio::spawn(async move { cache.drain_held(Instant::now()).await })
}

fn pause_all(cluster: &Cluster) {
    for node in &cluster.nodes {
        node.disk.fs.pause();
    }
}

fn resume_all(cluster: &Cluster) {
    for node in &cluster.nodes {
        node.disk.fs.resume();
    }
}

fn held(cluster: &Cluster, i: usize, blob: &TestBlob) -> bool {
    cluster.nodes[i].cache().is_held_for_test(&blob.digest)
}

/// Catches a write that is not acknowledged from RAM, one acknowledged by only one owner,
/// and a drain that does not write to the disk afterwards. With the sync gates closed on
/// both nodes the write returns: both caches hold the blob (and say so in
/// `unwritten_bytes`), neither index has it, no writer has begun and no sync has happened.
/// The drain then parks a writer in `sync` on each node (still no sync, still not indexed)
/// and, released, puts the blob in both indexes and empties the set. Mutants: the
/// producer disabled (`commit_held` always `commit`): the write then parks in the gate and
/// the first await never returns; the replica's leg not asking (`asks_held` false): the
/// replica's writer parks, the write never returns; the drain hook never called: the set
/// never empties.
#[tokio::test(flavor = "multi_thread")]
async fn a_write_is_acknowledged_from_ram_before_either_disk_has_it() {
    let cluster = Cluster::start(2, Setup::new(64 << 20)).await;
    let blob = cluster.pair_blob(0, 1, KIB4, 1);
    pause_all(&cluster);

    let answered = within("the acknowledgement", cluster.write(0, &blob))
        .await
        .expect("acknowledged");
    assert_eq!(answered, blob.digest);
    for i in 0..2 {
        assert!(held(&cluster, i, &blob), "node {i} holds it");
        assert_eq!(cluster.nodes[i].cache().unwritten().blobs, 1);
        assert_eq!(metric(&cluster, i, UNWRITTEN), KIB4 as f64);
        assert!(
            !cluster.indexed(i, &blob.digest),
            "node {i} has not indexed it"
        );
        assert_eq!(cluster.nodes[i].disk.fs.waiting(), 0, "no writer has begun");
        assert_eq!(cluster.nodes[i].disk.fs.syncs(), 0);
        assert!(cluster.nodes[i].up().confirmer.is_tracked(&blob.digest));
    }

    let drains = [drain(&cluster, 0), drain(&cluster, 1)];
    parked_in_sync(&cluster, 0, 1).await;
    parked_in_sync(&cluster, 1, 1).await;
    for i in 0..2 {
        assert_eq!(cluster.nodes[i].disk.fs.syncs(), 0);
        assert!(!cluster.indexed(i, &blob.digest));
        assert!(held(&cluster, i, &blob), "held while its writer is in sync");
    }

    resume_all(&cluster);
    for drain in drains {
        let report = within("the drain", drain).await.expect("join");
        assert_eq!((report.written, report.failed), (1, 0));
    }
    for i in 0..2 {
        assert!(cluster.indexed(i, &blob.digest), "node {i} indexed it");
        assert!(!held(&cluster, i, &blob));
        assert_eq!(metric(&cluster, i, UNWRITTEN), 0.0);
    }
}

/// Catches `--ram-ack-bytes` at its default changing a write: with no budget on either
/// node and the sync gates closed, the replica's writer parks in `sync` and the write is
/// not answered; nothing is held, nothing is tracked, no `writethrough` row is counted;
/// released, the write is answered and both indexes have the blob. This is the existing
/// behaviour, asserted through the new path. Mutant: the node treats a zero budget as
/// "acknowledge from RAM" (`ram_ack_enabled` always true, `hold` unbudgeted): the write
/// returns with the gates closed and the first assertion fails.
#[tokio::test(flavor = "multi_thread")]
async fn at_the_default_a_write_waits_for_the_disk_and_nothing_is_held() {
    let cluster = Cluster::start(2, Setup::new(0)).await;
    let blob = cluster.pair_blob(0, 1, KIB4, 2);
    pause_all(&cluster);

    let write = tokio::spawn({
        let client = cluster.client(0);
        let bytes = blob.bytes.clone();
        async move { kbf_storage::Storage::put(&client, bytes).await }
    });
    parked_in_sync(&cluster, 1, 1).await;
    assert!(
        !write.is_finished(),
        "not answered before the replica's disk"
    );
    assert_eq!(
        cluster.nodes[0].disk.fs.waiting(),
        0,
        "the primary waits for the replica"
    );
    for i in 0..2 {
        assert!(!held(&cluster, i, &blob));
        assert_eq!(cluster.nodes[i].cache().unwritten().blobs, 0);
        assert!(cluster.nodes[i].up().confirmer.is_empty());
    }

    resume_all(&cluster);
    within("the write", write)
        .await
        .expect("join")
        .expect("answered");
    for i in 0..2 {
        assert!(cluster.indexed(i, &blob.digest));
        assert!(!held(&cluster, i, &blob));
        let text = cluster.nodes[i].cache().render_metrics();
        for line in text
            .lines()
            .filter(|l| l.starts_with("kbf_storage_ram_writethrough_total{"))
        {
            assert!(line.ends_with(" 0"), "{line}");
        }
    }
}

/// T1. Catches an acknowledgement that depends on the primary's disk, a replica that does not
/// hold what it acknowledged, and a survivor that owes nothing. The primary is killed after
/// the answer, before either disk has the blob, and starts again over its log: it says
/// missing; a router read is served by the replica from RAM; once the replica's gate opens
/// its drain puts the blob in its index and the set is empty; the confirmation, after the
/// replica's own write and the grace, finds the restarted primary missing and journals
/// exactly one debt, naming the primary. Mutants: the replica registers nothing with its
/// confirmer (`confirm` left `None` for a copy that asked): no debt, the last assertion
/// fails; the primary acknowledges before the replica holds the blob (the leg's answer not
/// awaited): the router read finds nothing.
#[tokio::test(flavor = "multi_thread")]
async fn killing_the_primary_after_the_ack_leaves_the_replica_serving_and_owing() {
    let mut cluster = Cluster::start(2, Setup::new(64 << 20)).await;
    let blob = cluster.pair_blob(0, 1, KIB4, 10);
    pause_all(&cluster);
    within("the acknowledgement", cluster.write(0, &blob))
        .await
        .expect("acknowledged");

    cluster.kill(0);
    cluster.restart(0).await;
    assert!(!cluster.indexed(0, &blob.digest), "the primary lost it");
    let missing = cluster
        .client(0)
        .find_missing(&[blob.digest])
        .await
        .expect("asked");
    assert_eq!(missing, [blob.digest], "and says so");
    let read = within("the read", cluster.router().read_blob(&blob.digest))
        .await
        .expect("served by the replica");
    assert_eq!(read, blob.bytes);

    cluster.nodes[1].disk.fs.resume();
    let report = within("the drain", drain(&cluster, 1)).await.expect("join");
    assert_eq!((report.written, report.failed), (1, 0));
    assert!(cluster.indexed(1, &blob.digest));
    assert_eq!(metric(&cluster, 1, UNWRITTEN), 0.0);

    let done = confirm_after_drain(&cluster, 1).await;
    assert_eq!((done.durable, done.owed), (0, 1));
    let owed = cluster.nodes[1].up().repairs.open_records();
    assert_eq!(owed.len(), 1);
    assert_eq!(owed[0].digest, blob.digest);
    assert_eq!(owed[0].replica, cluster.nodes[0].id);
}

/// T2, the mirror: the replica is killed after the answer and restarts; the primary serves
/// the blob, writes it when its gate opens, and its confirmation finds the replica missing
/// and journals one debt naming it. Mutant: the primary registers nothing with its
/// confirmer (`confirm` left `None` for `Plan::Primary`): no debt.
#[tokio::test(flavor = "multi_thread")]
async fn killing_the_replica_after_the_ack_leaves_the_primary_serving_and_owing() {
    let mut cluster = Cluster::start(2, Setup::new(64 << 20)).await;
    let blob = cluster.pair_blob(0, 1, KIB4, 11);
    pause_all(&cluster);
    within("the acknowledgement", cluster.write(0, &blob))
        .await
        .expect("acknowledged");

    cluster.kill(1);
    cluster.restart(1).await;
    assert!(!cluster.indexed(1, &blob.digest));
    let read = within("the read", read_from(&cluster, 0, &blob.digest))
        .await
        .expect("served by the primary");
    assert_eq!(read, blob.bytes);

    cluster.nodes[0].disk.fs.resume();
    let report = within("the drain", drain(&cluster, 0)).await.expect("join");
    assert_eq!((report.written, report.failed), (1, 0));
    let done = confirm_after_drain(&cluster, 0).await;
    assert_eq!((done.durable, done.owed), (0, 1));
    let owed = cluster.nodes[0].up().repairs.open_records();
    assert_eq!(owed.len(), 1);
    assert_eq!(
        (owed[0].digest, owed[0].replica),
        (blob.digest, cluster.nodes[1].id)
    );
}

/// T3, the data. Both owners are killed after the answer, before either disk: the blob is
/// lost, as the design accepts. Both restart and say missing, a router read is `NotFound`
/// (never bytes that are not there), a FindMissing reports it so a client uploads it again,
/// and the upload is acknowledged and read back. Catches a lost blob reported present, and
/// a restart that cannot take the re-upload. Mutant: `present` answers from the tier's
/// files rather than the index (an unindexed block reads as present).
#[tokio::test(flavor = "multi_thread")]
async fn killing_both_owners_after_the_ack_loses_the_blob_and_the_client_uploads_it_again() {
    let mut cluster = Cluster::start(2, Setup::new(64 << 20)).await;
    let blob = cluster.pair_blob(0, 1, KIB4, 12);
    pause_all(&cluster);
    within("the acknowledgement", cluster.write(0, &blob))
        .await
        .expect("acknowledged");

    cluster.kill(0);
    cluster.kill(1);
    cluster.restart(0).await;
    cluster.restart(1).await;
    for i in 0..2 {
        assert!(!cluster.indexed(i, &blob.digest));
        let missing = cluster
            .client(i)
            .find_missing(&[blob.digest])
            .await
            .expect("asked");
        assert_eq!(missing, [blob.digest], "node {i}");
    }
    let router = cluster.router();
    let lost = within("the read", router.read_blob(&blob.digest)).await;
    assert!(matches!(lost, Err(StorageError::NotFound(_))), "{lost:?}");
    assert_eq!(
        router.find_missing(&[blob.digest]).await.expect("asked"),
        [blob.digest]
    );

    // The client uploads it again; this time both disks take it before the answer is
    // possible to lose.
    within("the upload", cluster.write(0, &blob))
        .await
        .expect("acknowledged");
    let read = within("the read", router.read_blob(&blob.digest))
        .await
        .expect("read");
    assert_eq!(read, blob.bytes);
}

/// Restart mid-drain (T5). The primary's `PutStaged` is held after the blob's block was
/// synced; the node is killed there and starts again. The orphan block is removed, the index
/// lacks the blob, and the node says missing; the replica still holds it (served from RAM)
/// and, drained, has it intact. Catches a restart that indexes an unacknowledged block, one
/// that keeps the orphan, and a replica that loses its copy with its peer. Mutant: the drain
/// applies `PutStaged` before it syncs the block (the index would then name bytes the
/// restart does not find: this test's `indexed` is true before the kill).
#[tokio::test(flavor = "multi_thread")]
async fn a_restart_mid_drain_removes_the_orphan_and_the_peer_keeps_the_blob() {
    let mut cluster = Cluster::start(2, Setup::new(64 << 20)).await;
    let blob = cluster.pair_blob(0, 1, KIB4, 13);
    cluster.nodes[0].disk.log.hold("put_staged");
    within("the acknowledgement", cluster.write(0, &blob))
        .await
        .expect("acknowledged");

    let _draining = drain(&cluster, 0);
    within(
        "the commit to park at its hold",
        cluster.nodes[0].disk.log.wait_held("put_staged", 1),
    )
    .await;
    assert!(!cluster.indexed(0, &blob.digest), "synced, not yet indexed");
    assert!(
        cluster.nodes[0].disk.fs.syncs() >= 1,
        "the block is on the disk"
    );

    cluster.kill(0);
    cluster.nodes[0].disk.log.release("put_staged");
    cluster.restart(0).await;
    assert!(
        !cluster.nodes[0].up().startup.recovery.removed.is_empty(),
        "the orphan block was removed: {:?}",
        cluster.nodes[0].up().startup.recovery
    );
    assert!(!cluster.indexed(0, &blob.digest));
    let missing = cluster
        .client(0)
        .find_missing(&[blob.digest])
        .await
        .expect("asked");
    assert_eq!(missing, [blob.digest]);

    cluster.nodes[1].disk.fs.resume();
    let read = within("the read", read_from(&cluster, 1, &blob.digest))
        .await
        .expect("the peer serves it");
    assert_eq!(read, blob.bytes);
    let report = within("the drain", drain(&cluster, 1)).await.expect("join");
    assert_eq!(report.written, 1);
    assert!(cluster.indexed(1, &blob.digest));
}

/// T6, the first drain-race point. With the primary's `PutStaged` held (the block is
/// synced, the index lacks the blob, the entry is held), the primary answers a FindMissing
/// present and serves the bytes, both asked of it directly (a router asks the primary
/// alone for FindMissing, and falls over to the replica for a read, which would hide a
/// miss); released, the same after the index has it. Mutant: the pass removes the entry
/// before `put_staged` returns (the `held.remove` moved above the await): the primary
/// answers missing at the hold.
#[tokio::test(flavor = "multi_thread")]
async fn a_blob_is_served_by_the_primary_while_its_index_commit_is_in_flight() {
    let cluster = Cluster::start(2, Setup::new(64 << 20)).await;
    let blob = cluster.pair_blob(0, 1, KIB4, 14);
    cluster.nodes[0].disk.log.hold("put_staged");
    within("the acknowledgement", cluster.write(0, &blob))
        .await
        .expect("acknowledged");
    let draining = drain(&cluster, 0);
    within(
        "the commit to park at its hold",
        cluster.nodes[0].disk.log.wait_held("put_staged", 1),
    )
    .await;

    assert!(!cluster.indexed(0, &blob.digest));
    let primary = cluster.client(0);
    assert!(
        primary
            .find_missing(&[blob.digest])
            .await
            .expect("asked")
            .is_empty()
    );
    assert_eq!(
        read_from(&cluster, 0, &blob.digest).await.expect("served"),
        blob.bytes
    );
    assert!(
        cluster
            .router()
            .find_missing(&[blob.digest])
            .await
            .expect("asked")
            .is_empty()
    );

    cluster.nodes[0].disk.log.release("put_staged");
    let report = within("the drain", draining).await.expect("join");
    assert_eq!(report.written, 1);
    assert!(
        primary
            .find_missing(&[blob.digest])
            .await
            .expect("asked")
            .is_empty()
    );
    assert_eq!(
        read_from(&cluster, 0, &blob.digest).await.expect("served"),
        blob.bytes
    );
}

/// T6, the second drain-race point. The pool's writer is parked after the index took the
/// blob and before the entry is removed: both the index and the set have it, and the
/// primary answers present and serves it; after the pass the set is empty and the index
/// alone answers. Catches a hook that runs before the apply, and an entry removed by the
/// writer instead of the pass. Mutant: the pass removes the entry at the hook (inside
/// `drain_one`): `is_held_for_test` is false while parked.
#[tokio::test(flavor = "multi_thread")]
async fn a_blob_is_in_the_index_and_the_set_at_the_same_time_while_the_drain_finishes() {
    let (parked_tx, mut parked) = tokio::sync::mpsc::unbounded_channel();
    let (go_tx, go_rx) = mpsc::channel::<()>();
    let go_rx = Mutex::new(go_rx);
    let hook: DrainHook = std::sync::Arc::new(move |digests| {
        let _ = parked_tx.send(digests.to_vec());
        let _ = go_rx.lock().expect("lock").recv();
    });
    let mut setup = Setup::new(64 << 20);
    setup.hook = Some(hook);
    let cluster = Cluster::start(2, setup).await;
    let blob = cluster.pair_blob(0, 1, KIB4, 15);
    within("the acknowledgement", cluster.write(0, &blob))
        .await
        .expect("acknowledged");
    let draining = drain(&cluster, 0);
    let digests = within("the hook", parked.recv())
        .await
        .expect("the hook ran");
    assert_eq!(digests, [blob.digest]);

    assert!(cluster.indexed(0, &blob.digest), "the index has it");
    assert!(held(&cluster, 0, &blob), "and the set still does");
    assert!(
        cluster
            .client(0)
            .find_missing(&[blob.digest])
            .await
            .expect("asked")
            .is_empty()
    );
    assert_eq!(
        read_from(&cluster, 0, &blob.digest).await.expect("served"),
        blob.bytes
    );

    go_tx.send(()).expect("go");
    within("the drain", draining).await.expect("join");
    assert!(!held(&cluster, 0, &blob));
    assert_eq!(
        read_from(&cluster, 0, &blob.digest).await.expect("served"),
        blob.bytes
    );
}

/// T7. A replica that cannot be reached is a write through, with its debt journaled before
/// the disk is asked. The replica is cut off and the primary's gate closed: the primary's
/// writer parks in `sync`, the write is not answered, the debt is already in the journal
/// naming the replica, nothing is held in RAM, and the write-through is counted for its
/// reason. Released, the write is answered and the blob is on the disk. Mutants: a lost
/// replica commits `Held` (the write is answered with the gate closed; the test fails in
/// `parked_unanswered`); the debt journaled after the commit (the journal is empty at the
/// park).
#[tokio::test(flavor = "multi_thread")]
async fn a_replica_that_is_down_is_a_write_through_with_the_debt_journaled_first() {
    let cluster = Cluster::start(2, Setup::new(64 << 20)).await;
    let blob = cluster.pair_blob(0, 1, KIB4, 16);
    cluster.nodes[1].up().switch.cut();
    cluster.nodes[0].disk.fs.pause();

    let mut write = writing(&cluster, 0, &blob);
    parked_unanswered(&cluster, 0, 1, &mut write).await;
    assert!(!held(&cluster, 0, &blob));
    assert_eq!(cluster.nodes[0].cache().unwritten().blobs, 0);
    let owed = cluster.nodes[0].up().repairs.open_records();
    assert_eq!(
        owed.len(),
        1,
        "the debt is journaled before the disk is asked"
    );
    assert_eq!(
        (owed[0].digest, owed[0].replica),
        (blob.digest, cluster.nodes[1].id)
    );

    cluster.nodes[0].disk.fs.resume();
    within("the write", write)
        .await
        .expect("join")
        .expect("answered");
    assert!(cluster.indexed(0, &blob.digest));
    assert_eq!(metric(&cluster, 0, &through("replica_lost")), 1.0);
    assert!(cluster.nodes[0].up().confirmer.is_empty());
}

/// Q4. The second owner takes a client write as the primary while the first owner is dead
/// but not down in the map: the leg to the first owner fails as unreachable, which is a lost
/// replica, so the write goes through with a debt naming the first owner. Catches a second
/// owner that acknowledges from RAM with one live copy. Mutant as the test above.
#[tokio::test(flavor = "multi_thread")]
async fn the_second_owner_taking_the_write_while_the_first_is_dead_writes_through() {
    let cluster = Cluster::start(2, Setup::new(64 << 20)).await;
    let blob = cluster.pair_blob(0, 1, KIB4, 17);
    cluster.nodes[0].up().switch.cut();
    cluster.nodes[1].disk.fs.pause();

    let mut write = writing(&cluster, 1, &blob);
    parked_unanswered(&cluster, 1, 1, &mut write).await;
    assert!(!held(&cluster, 1, &blob));
    let owed = cluster.nodes[1].up().repairs.open_records();
    assert_eq!(owed.len(), 1);
    assert_eq!(owed[0].replica, cluster.nodes[0].id);

    cluster.nodes[1].disk.fs.resume();
    within("the write", write)
        .await
        .expect("join")
        .expect("answered");
    assert_eq!(metric(&cluster, 1, &through("replica_lost")), 1.0);
}

/// T8. A blob over 4 MiB stays acknowledged from the disk: with the gates closed the
/// replica's writer parks in `sync`, the write is not answered, nothing is held, the
/// replica counts it as `large`; released, it is answered and indexed on both owners.
/// Catches a producer that holds a streamed blob. Mutant: the `large` arm of
/// `commit_held` removed (the count is 0).
#[tokio::test(flavor = "multi_thread")]
async fn a_blob_over_four_mib_is_acknowledged_from_the_disk() {
    let cluster = Cluster::start(2, Setup::new(64 << 20)).await;
    let blob = cluster.pair_blob(0, 1, (4 << 20) + 1, 18);
    pause_all(&cluster);

    let mut write = writing(&cluster, 0, &blob);
    parked_unanswered(&cluster, 1, 1, &mut write).await;
    for i in 0..2 {
        assert!(!held(&cluster, i, &blob));
        assert_eq!(cluster.nodes[i].cache().unwritten().blobs, 0);
    }
    resume_all(&cluster);
    within("the write", write)
        .await
        .expect("join")
        .expect("answered");
    for i in 0..2 {
        assert!(cluster.indexed(i, &blob.digest));
        assert_eq!(metric(&cluster, i, &through("large")), 1.0, "node {i}");
    }
}

/// T8, R = 1. A blob with one owner is a single copy, so it stays acknowledged from the
/// disk: the owner's writer parks in `sync`, the write is not answered, nothing is held, the
/// write is counted as `single_owner`. Mutant: `Held` for a write with no replica.
#[tokio::test(flavor = "multi_thread")]
async fn a_blob_with_one_owner_is_acknowledged_from_the_disk() {
    let mut setup = Setup::new(64 << 20);
    setup.r = 1;
    let cluster = Cluster::start(2, setup).await;
    let blob = cluster.blob(0, KIB4, 19);
    pause_all(&cluster);

    let mut write = writing(&cluster, 0, &blob);
    parked_unanswered(&cluster, 0, 1, &mut write).await;
    assert!(!held(&cluster, 0, &blob));
    resume_all(&cluster);
    within("the write", write)
        .await
        .expect("join")
        .expect("answered");
    assert!(cluster.indexed(0, &blob.digest));
    assert_eq!(metric(&cluster, 0, &through("single_owner")), 1.0);
}

/// T4 and T9, the budget. With room for two blobs on each node and the gates closed, two
/// writes are answered from RAM and the gauge is exactly the budget; a third is not
/// answered: the replica has no budget left, commits to its disk and its writer parks in
/// `sync` (the backpressure), counted as `budget`; the bytes held never exceed the budget.
/// Released, the third is answered, the drains empty the sets and the gauge returns to 0.
/// Catches a budget that is exceeded, a write that is dropped or answered without its
/// disk when the budget is full, and a gauge that does not return to 0. Mutant:
/// `try_acquire` ignores the cap (the third is held too: answered with the gates closed).
#[tokio::test(flavor = "multi_thread")]
async fn a_full_budget_makes_the_write_wait_for_its_disk_and_the_gauge_returns_to_zero() {
    let cluster = Cluster::start(2, Setup::new(2 * KIB4 as u64)).await;
    let blobs: Vec<TestBlob> = (0..3)
        .map(|n| cluster.pair_blob(0, 1, KIB4, 20 + n))
        .collect();
    pause_all(&cluster);
    for blob in &blobs[..2] {
        within("the acknowledgement", cluster.write(0, blob))
            .await
            .expect("acknowledged");
    }
    for i in 0..2 {
        assert_eq!(metric(&cluster, i, UNWRITTEN), (2 * KIB4) as f64);
    }

    let mut third = writing(&cluster, 0, &blobs[2]);
    parked_unanswered(&cluster, 1, 1, &mut third).await;
    assert_eq!(metric(&cluster, 1, &through("budget")), 1.0);
    for i in 0..2 {
        assert_eq!(
            metric(&cluster, i, UNWRITTEN),
            (2 * KIB4) as f64,
            "never over the budget"
        );
        assert!(!held(&cluster, i, &blobs[2]));
    }

    resume_all(&cluster);
    within("the third write", third)
        .await
        .expect("join")
        .expect("answered");
    for i in 0..2 {
        // The pool's writer may have a pass of its own; a pass takes what is left.
        within("the drain", drain(&cluster, i)).await.expect("join");
        assert_eq!(metric(&cluster, i, UNWRITTEN), 0.0, "node {i}");
        for blob in &blobs {
            assert!(cluster.indexed(i, &blob.digest), "node {i}");
        }
    }
}

/// Disk full on the write-behind (T4). The drain's disk refuses to create a block and the
/// store refuses the upload: the pass fails, the blob stays held (counted as a drain
/// failure) and is still served; once the disk is back the next pass writes it. Catches an
/// entry dropped on a failed write and a failure that is not counted. Mutant: the pass
/// removes the entry whatever the write did.
#[tokio::test(flavor = "multi_thread")]
async fn a_failed_write_behind_keeps_the_blob_held_and_served() {
    let cluster = Cluster::start(2, Setup::new(64 << 20)).await;
    let blob = cluster.pair_blob(0, 1, KIB4, 30);
    within("the acknowledgement", cluster.write(0, &blob))
        .await
        .expect("acknowledged");
    cluster.nodes[0]
        .disk
        .fs
        .probe
        .fail_creates
        .store(true, std::sync::atomic::Ordering::SeqCst);
    cluster.nodes[0]
        .disk
        .store
        .on_put(common::faulty::PutFault::Fail);
    cluster.nodes[0]
        .disk
        .store
        .on_put(common::faulty::PutFault::Fail);

    let report = within("the drain", drain(&cluster, 0)).await.expect("join");
    assert_eq!((report.written, report.failed), (0, 1));
    assert!(held(&cluster, 0, &blob));
    assert_eq!(
        metric(&cluster, 0, "kbf_storage_ram_drain_failures_total"),
        1.0
    );
    assert_eq!(metric(&cluster, 0, UNWRITTEN), KIB4 as f64);
    assert_eq!(
        read_from(&cluster, 0, &blob.digest).await.expect("served"),
        blob.bytes
    );

    cluster.nodes[0]
        .disk
        .fs
        .probe
        .fail_creates
        .store(false, std::sync::atomic::Ordering::SeqCst);
    let cache = std::sync::Arc::clone(cluster.nodes[0].cache());
    let retried = cache.drain_for_shutdown(Duration::from_secs(60)).await;
    assert_eq!(retried, kbf_front::ShutdownDrain::Drained);
    assert!(cluster.indexed(0, &blob.digest));
}

/// The unconfirmed gauge (`kbf_storage_ram_unconfirmed_blobs`): both owners keep the digest
/// after the answer, the gauge says so, and after both drained and the confirmation found
/// the peer durable it is back to 0 with no debt written. Catches a gauge that is never set
/// and a confirmation that journals on the normal path. Mutant: the storage's
/// `unconfirmed` is not told (the gauge stays 0 after the write).
#[tokio::test(flavor = "multi_thread")]
async fn the_unconfirmed_gauge_counts_until_the_peer_is_known_durable() {
    let cluster = Cluster::start(2, Setup::new(64 << 20)).await;
    let blob = cluster.pair_blob(0, 1, KIB4, 31);
    within("the acknowledgement", cluster.write(0, &blob))
        .await
        .expect("acknowledged");
    for i in 0..2 {
        assert_eq!(metric(&cluster, i, UNCONFIRMED), 1.0, "node {i}");
    }
    for i in 0..2 {
        within("the drain", drain(&cluster, i)).await.expect("join");
    }
    for i in 0..2 {
        let done = confirm_after_drain(&cluster, i).await;
        assert_eq!((done.durable, done.owed), (1, 0), "node {i}");
        assert_eq!(metric(&cluster, i, UNCONFIRMED), 0.0, "node {i}");
        assert!(cluster.nodes[i].up().repairs.open_records().is_empty());
    }
}
