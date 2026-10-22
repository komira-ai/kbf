//! Rule 2 over the real producer (`docs/design/storage-ram-ack.md`, T3): an action-cache
//! entry is written only once every blob it names is on an owner's disk. A farm of three
//! real caches holds an output and its result in RAM on two owners and has the entry's
//! action owned by the third; the commit parks at the barrier (a positive event: the wait
//! parked on the owner's cache), and the owners are killed with it parked, or the first
//! owner's drain lets it through. No test sleeps.

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

use bytes::Bytes;
use chain::{Cluster, Setup, within};
use kbf_meta::{ActionRecord, Closure, Role};
use kbf_proto::reapi::{ActionResult, OutputFile};
use kbf_storage::StorageError;
use kbf_types::Digest;
use prost::Message;
use support::TestBlob;
use tokio::time::Instant;

type CommitTask = tokio::task::JoinHandle<Result<(), StorageError>>;

/// The entry under test: its output and `ActionResult` blobs have the same two owners
/// (nodes 0 and 1), and its action is owned by node 2.
struct Entry {
    action: Digest,
    output: TestBlob,
    result: Bytes,
    result_digest: Digest,
}

fn digest_of(digest: &Digest) -> kbf_proto::reapi::Digest {
    kbf_proto::reapi::Digest {
        hash: digest.hash_hex(),
        size_bytes: digest.size_bytes as i64,
    }
}

fn entry(cluster: &Cluster, salt: u64) -> Entry {
    let output = cluster.pair_blob(0, 1, 4096, salt);
    let (result, result_digest) = (0..10_000u32)
        .map(|round| {
            let result = ActionResult {
                output_files: vec![OutputFile {
                    path: "out".into(),
                    digest: Some(digest_of(&output.digest)),
                    ..OutputFile::default()
                }],
                stdout_raw: round.to_le_bytes().to_vec(),
                ..ActionResult::default()
            }
            .encode_to_vec();
            let digest = kbf_segments::sha256(&result);
            (Bytes::from(result), digest)
        })
        .find(|(_, digest)| cluster.primary_of(digest) == 0 && cluster.rank_of(digest, 1) == 1)
        .expect("a result with the output's owners");
    let action = (0..10_000u32)
        .map(|n| kbf_segments::sha256(format!("action {salt} {n}").as_bytes()))
        .find(|digest| cluster.primary_of(digest) == 2)
        .expect("an action owned by node 2");
    Entry {
        action,
        output,
        result,
        result_digest,
    }
}

/// Writes the entry's blobs through the router (acknowledged from RAM on nodes 0 and 1) and
/// starts the commit of its record as a task.
async fn commit(cluster: &Cluster, entry: &Entry) -> CommitTask {
    let router = cluster.router();
    router
        .put(entry.output.bytes.clone())
        .await
        .expect("output");
    let stored = router.put(entry.result.clone()).await.expect("result");
    assert_eq!(stored, entry.result_digest);
    let record = ActionRecord {
        result: entry.result_digest,
        closure: [entry.output.digest].into_iter().collect::<Closure>(),
    };
    let action = entry.action;
    tokio::spawn(async move {
        router
            .commit_with_closure(Role::Daemon, action, record)
            .await
    })
}

/// Waits until the commit's barrier has parked on node `i` (the owner holds a blob of the
/// entry in RAM); fails the test, rather than waiting for ever, if the commit finishes
/// first.
async fn parked_at(cluster: &Cluster, i: usize, commit: &mut CommitTask) {
    tokio::select! {
        () = within("the barrier to park", cluster.nodes[i].cache().parked_for_test(1)) => {}
        done = commit => panic!("committed before the blobs were on an owner's disk: {done:?}"),
    }
}

/// T3, with the owners killed. The entry's blobs are acknowledged from RAM on two owners;
/// the commit parks at the barrier on the primary; with it parked both owners are killed
/// and the commit is refused. After the restart there is no entry: `action_result` is a
/// miss (the action runs again), never a hit whose outputs are gone. Catches an entry
/// written over blobs that are only in RAM. Mutant: barrier (a) deleted (`commit_closed_at`
/// skips the `find_durable(.., wait)` call): the commit finishes while both owners hold
/// the output in RAM and `parked_at` fails.
#[tokio::test(flavor = "multi_thread")]
async fn an_entry_is_not_written_over_blobs_that_only_the_owners_ram_holds() {
    let mut cluster = Cluster::start(3, Setup::new(64 << 20)).await;
    let entry = entry(&cluster, 1);
    let mut committing = commit(&cluster, &entry).await;
    parked_at(&cluster, 0, &mut committing).await;
    assert!(
        cluster
            .router()
            .action_result(&entry.action)
            .await
            .expect("asked")
            .is_none(),
        "no entry while the barrier waits"
    );

    cluster.kill(0);
    cluster.kill(1);
    let refused = within("the commit", committing).await.expect("join");
    assert!(refused.is_err(), "the commit cannot succeed: {refused:?}");
    cluster.restart(0).await;
    cluster.restart(1).await;
    assert!(
        cluster
            .router()
            .action_result(&entry.action)
            .await
            .expect("asked")
            .is_none(),
        "no entry after both owners lost their RAM"
    );

    // The action runs again, its outputs are uploaded again and the same entry is
    // committed: it heals, and the output reads back.
    let mut again = commit(&cluster, &entry).await;
    parked_at(&cluster, 0, &mut again).await;
    let report = cluster.nodes[0].cache().drain_held(Instant::now()).await;
    assert_eq!(report.failed, 0);
    within("the commit", again)
        .await
        .expect("join")
        .expect("committed");
    let router = cluster.router();
    assert!(
        router
            .action_result(&entry.action)
            .await
            .expect("asked")
            .is_some(),
        "the entry is a hit"
    );
    assert_eq!(
        router.read_blob(&entry.output.digest).await.expect("read"),
        entry.output.bytes
    );
}

/// T3's positive twin. The same entry; once the primary has written the output and the
/// result the commit goes through (a blob durable on an owner is enough, the replica is
/// still in RAM), the entry exists, and after both owners are killed and restarted it is
/// still a hit with its output readable from the primary's disk. Catches a barrier that
/// never lets the commit through, and an entry whose blobs are not on a disk. Mutant: the
/// barrier asks the replica only (the commit stays parked after the primary's drain).
#[tokio::test(flavor = "multi_thread")]
async fn an_entry_is_written_once_the_primary_has_the_blobs_on_its_disk() {
    let mut cluster = Cluster::start(3, Setup::new(64 << 20)).await;
    let entry = entry(&cluster, 2);
    let mut committing = commit(&cluster, &entry).await;
    parked_at(&cluster, 0, &mut committing).await;

    let cache = Arc::clone(cluster.nodes[0].cache());
    let report = cache.drain_held(Instant::now()).await;
    assert_eq!(report.failed, 0);
    within("the commit", committing)
        .await
        .expect("join")
        .expect("committed");
    assert!(
        cluster
            .router()
            .action_result(&entry.action)
            .await
            .expect("asked")
            .is_some()
    );

    cluster.kill(0);
    cluster.kill(1);
    cluster.restart(0).await;
    cluster.restart(1).await;
    let router = cluster.router();
    assert!(
        router
            .action_result(&entry.action)
            .await
            .expect("asked")
            .is_some(),
        "the entry stands"
    );
    assert_eq!(
        router.read_blob(&entry.output.digest).await.expect("read"),
        entry.output.bytes
    );
}
