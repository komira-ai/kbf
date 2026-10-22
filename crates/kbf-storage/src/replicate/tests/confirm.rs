//! Peer confirmation (`docs/design/storage-ram-ack.md`): a node that acknowledged a blob
//! from RAM asks the other owner, once its own write and the grace are over, whether the
//! blob is on its disk, and journals a debt if it is not. The tests register digests through
//! `PeerConfirmer::track` (the service's write path does it for a blob it may hold; see
//! `ram_ack.rs`), and
//! take the instant to confirm at as an argument, so none waits for the grace.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use bytes::Bytes;
use kbf_map::MemberState;
use kbf_types::Digest;
use tokio::time::Instant;

use super::chain::within;
use super::farm::Farm;
use crate::Storage;
use crate::repair::{Confirmed, PeerConfirmer, RepairConfig, RepairWorker, Tracked};
use crate::replicate::ChainConfig;
use crate::service::tests::fake::sha;

const GRACE: Duration = Duration::from_secs(5);

fn config() -> RepairConfig {
    RepairConfig {
        interval: Duration::from_millis(50),
        concurrency: 2,
        peer_grace: GRACE,
        unconfirmed_cap: 100,
        probe: Duration::from_secs(60),
    }
}

fn confirmer(farm: &Farm, config: &RepairConfig) -> Arc<PeerConfirmer> {
    Arc::new(PeerConfirmer::new(
        Arc::clone(&farm.nodes[0].chain),
        Arc::clone(&farm.nodes[0].repairs),
        config,
    ))
}

/// A blob held by node 0, and registered with its confirmer as acknowledged under
/// generation 1 with node 1's member (id 2) as the owner to confirm.
async fn acknowledged(farm: &Farm, confirmer: &PeerConfirmer, salt: u8) -> Digest {
    let bytes = vec![salt; 64];
    let digest = sha(&bytes);
    farm.nodes[0]
        .mem
        .blobs
        .lock()
        .expect("lock")
        .insert(digest, Bytes::from(bytes));
    let tracked = confirmer
        .track(digest, farm.nodes[1].id, 1)
        .await
        .expect("tracked");
    assert_eq!(tracked, Tracked::Waiting);
    digest
}

fn peer_has_on_disk(farm: &Farm, digest: Digest) {
    farm.nodes[1]
        .mem
        .blobs
        .lock()
        .expect("lock")
        .insert(digest, Bytes::from(vec![0; 64]));
}

fn peer_has_in_ram(farm: &Farm, digest: Digest) {
    peer_has_on_disk(farm, digest);
    farm.nodes[1]
        .mem
        .ram_only
        .lock()
        .expect("lock")
        .insert(digest);
}

fn owed(farm: &Farm) -> Vec<Digest> {
    farm.nodes[0]
        .repairs
        .open_records()
        .iter()
        .map(|r| r.digest)
        .collect()
}

fn asked(farm: &Farm) -> usize {
    farm.nodes[1].mem.durable_asked.load(Ordering::SeqCst)
}

/// Catches a confirmation that journals a debt on the normal path, or one that does not ask
/// the durable question: the peer holds the blob on its disk, so the digest is forgotten
/// with nothing in the journal; the peer was asked once, `find_durable`, and `find_missing`
/// never. (The normal path does no IO.) Mutant: the debt journalled whatever the peer says;
/// the peer asked the plain question (this peer would then pass a RAM copy too, below).
#[tokio::test]
async fn a_peer_that_holds_the_blob_on_its_disk_is_forgotten_and_nothing_is_written() {
    let farm = Farm::start(2, ChainConfig::default()).await;
    let confirmer = confirmer(&farm, &config());
    let digest = acknowledged(&farm, &confirmer, 1).await;
    peer_has_on_disk(&farm, digest);
    let now = Instant::now();
    confirmer.drained(&digest, now);

    let done = confirmer.confirm(now + GRACE).await;
    assert_eq!(
        done,
        Confirmed {
            durable: 1,
            owed: 0,
            dropped: 0
        }
    );
    assert!(confirmer.is_empty());
    assert!(owed(&farm).is_empty(), "no debt: the peer is durable");
    assert_eq!(asked(&farm), 1);
    assert_eq!(farm.nodes[1].mem.knobs.asked.load(Ordering::SeqCst), 0);
}

/// Catches a confirmation that takes a RAM copy for a durable one (T1, the kill-the-peer
/// case as the survivor sees it): the peer holds the blob only in RAM, or has lost it (a
/// restarted peer), and after the grace the survivor journals exactly one debt naming the
/// blob and the peer, forgets the digest, and a second confirmation adds nothing. The
/// worker would then copy it. Mutants: the plain question asked (the RAM copy counts, no
/// debt); the debt journalled twice; the digest kept after the debt.
#[tokio::test]
async fn a_peer_without_a_durable_copy_is_owed_once_after_the_grace() {
    let farm = Farm::start(2, ChainConfig::default()).await;
    let confirmer = confirmer(&farm, &config());
    let in_ram = acknowledged(&farm, &confirmer, 2).await;
    let lost = acknowledged(&farm, &confirmer, 3).await;
    peer_has_in_ram(&farm, in_ram);
    let now = Instant::now();
    confirmer.drained(&in_ram, now);
    confirmer.drained(&lost, now);

    let done = confirmer.confirm(now + GRACE).await;
    assert_eq!(
        done,
        Confirmed {
            durable: 0,
            owed: 2,
            dropped: 0
        }
    );
    let mut want = vec![in_ram, lost];
    want.sort();
    assert_eq!(owed(&farm), want);
    let records = farm.nodes[0].repairs.open_records();
    assert!(
        records
            .iter()
            .all(|r| r.replica == farm.nodes[1].id && r.generation == 1)
    );
    assert!(confirmer.is_empty());
    assert_eq!(asked(&farm), 1, "one call for the batch");

    assert_eq!(
        confirmer.confirm(now + GRACE * 2).await,
        Confirmed::default()
    );
    assert_eq!(owed(&farm).len(), 2, "no second debt");
}

/// Catches a confirmation that asks before the node's own write (the digest is not drained)
/// or before the grace is over: neither asks the peer or journals anything, the digest
/// stays; at the instant the grace ends it is asked. Mutants: `due` ignored; the grace
/// counted from the registration instead of the drain.
#[tokio::test]
async fn the_peer_is_asked_only_after_the_drain_and_the_grace() {
    let farm = Farm::start(2, ChainConfig::default()).await;
    let confirmer = confirmer(&farm, &config());
    let digest = acknowledged(&farm, &confirmer, 4).await;
    let now = Instant::now();

    assert_eq!(
        confirmer.confirm(now + GRACE * 100).await,
        Confirmed::default(),
        "not drained: never due"
    );
    confirmer.drained(&digest, now);
    assert_eq!(
        confirmer
            .confirm(now + GRACE - Duration::from_millis(1))
            .await,
        Confirmed::default()
    );
    assert_eq!(asked(&farm), 0);
    assert!(owed(&farm).is_empty());
    assert!(confirmer.is_tracked(&digest));

    peer_has_on_disk(&farm, digest);
    assert_eq!(confirmer.confirm(now + GRACE).await.durable, 1);
    assert_eq!(asked(&farm), 1);

    // A drain of a digest nobody registered is ignored.
    confirmer.drained(&sha(b"nobody"), now);
    assert!(confirmer.is_empty());
}

/// Catches a peer that cannot be asked being taken for a durable one: the peer is cut off
/// (a crashed owner the map has not noticed), or the map names it down: the blob is owed
/// (nobody confirmed it), and the peer that the map names down is not called. Mutants: an
/// error counted as durable; a down peer asked.
#[tokio::test]
async fn a_peer_that_cannot_be_asked_is_owed() {
    let farm = Farm::start(2, ChainConfig::default()).await;
    let mut quick = config();
    quick.probe = Duration::from_secs(60);
    let confirmer = confirmer(&farm, &quick);
    let cut = acknowledged(&farm, &confirmer, 5).await;
    let now = Instant::now();
    confirmer.drained(&cut, now);
    farm.nodes[1].switch.cut();
    assert_eq!(confirmer.confirm(now + GRACE).await.owed, 1);
    assert_eq!(owed(&farm), [cut]);

    farm.nodes[1].switch.restore();
    let down = acknowledged(&farm, &confirmer, 6).await;
    confirmer.drained(&down, now);
    let before = asked(&farm);
    let map = farm.map(2, 2, &[(1, MemberState::Down)]);
    farm.publish(&map);
    assert_eq!(confirmer.confirm(now + GRACE).await.owed, 1);
    assert_eq!(asked(&farm), before, "a down peer is not called");
    assert!(owed(&farm).contains(&down));
}

/// Catches a digest this node no longer has to confirm, or never had a peer for, kept or
/// journalled: with R = 1 there is no other owner, so it is dropped with nothing asked and
/// nothing owed. Mutant: the digest owed to a peer that does not own it.
#[tokio::test]
async fn a_digest_with_no_other_owner_is_dropped() {
    let farm = Farm::start(2, ChainConfig::default()).await;
    let confirmer = confirmer(&farm, &config());
    let map = farm.map(2, 1, &[]);
    farm.publish(&map);
    // Find a blob node 0 owns alone.
    let bytes = (0_u32..10_000)
        .map(|i| i.to_le_bytes().to_vec())
        .find(|b| map.write_owners(&sha(b)) == [farm.nodes[0].id])
        .expect("a blob node 0 owns alone");
    let digest = sha(&bytes);
    confirmer
        .track(digest, farm.nodes[1].id, 1)
        .await
        .expect("tracked");
    let now = Instant::now();
    confirmer.drained(&digest, now);

    assert_eq!(
        confirmer.confirm(now + GRACE).await,
        Confirmed {
            durable: 0,
            owed: 0,
            dropped: 1
        }
    );
    assert!(confirmer.is_empty());
    assert!(owed(&farm).is_empty());
    assert_eq!(asked(&farm), 0);
}

/// Catches a set that grows without bound, or loses what it is given when full (T: "the
/// set is capped; overflow journals the debt at once"): with room for one digest, the
/// second is owed at once, the first is still kept, and a digest kept twice stays one
/// entry (the first's). Mutants: no cap; the cap dropping the digest without a debt.
#[tokio::test]
async fn a_full_set_journals_the_debt_at_once() {
    let farm = Farm::start(2, ChainConfig::default()).await;
    let mut one = config();
    one.unconfirmed_cap = 1;
    let confirmer = confirmer(&farm, &one);
    let first = acknowledged(&farm, &confirmer, 7).await;
    assert_eq!(
        confirmer
            .track(first, farm.nodes[1].id, 9)
            .await
            .expect("tracked"),
        Tracked::Waiting,
        "a digest kept twice is one entry"
    );
    assert_eq!(confirmer.len(), 1);
    let second = sha(b"the second");
    assert_eq!(
        confirmer
            .track(second, farm.nodes[1].id, 1)
            .await
            .expect("owed"),
        Tracked::Owed
    );
    assert_eq!(owed(&farm), [second]);
    assert_eq!(confirmer.len(), 1);
    assert!(confirmer.is_tracked(&first));
}

/// Catches a worker that never confirms: `run` confirms on its ticks, so a digest drained
/// long ago whose peer lost it is owed without anyone calling `confirm`, and `run` stops
/// when told. The wait is a yield loop over the debt that must appear, bounded as a hang.
/// Mutant: `run` does not confirm.
#[tokio::test]
async fn the_worker_confirms_on_its_ticks() {
    let farm = Farm::start(2, ChainConfig::default()).await;
    let worker = Arc::new(RepairWorker::new(
        Arc::clone(&farm.nodes[0].mem) as Arc<dyn Storage>,
        Arc::clone(&farm.nodes[0].chain),
        Arc::clone(&farm.nodes[0].repairs),
        config(),
    ));
    let confirmer = worker.confirmer();
    let digest = acknowledged(&farm, &confirmer, 8).await;
    confirmer.drained(&digest, Instant::now() - GRACE * 2);
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let running = tokio::spawn({
        let worker = Arc::clone(&worker);
        async move {
            worker
                .run(async {
                    let _ = stopped.await;
                })
                .await;
        }
    });
    within(async {
        while confirmer.is_tracked(&digest) {
            tokio::task::yield_now().await;
        }
    })
    .await;
    assert_eq!(owed(&farm), [digest]);
    let _ = stop.send(());
    within(running).await.expect("run returns");
}

/// Catches a node with no map taking itself to have confirmed, or asking nobody and
/// forgetting: a node that holds no map cannot ask any peer, so what it acknowledged is
/// owed (and the digest forgotten, the debt being the record). Mutant: the digest dropped
/// without a debt.
#[tokio::test]
async fn a_node_with_no_map_owes_what_it_cannot_confirm() {
    let farm = Farm::start(2, ChainConfig::default()).await;
    let unmapped = crate::replicate::tests::farm::NodeMap::new(farm.nodes[0].id);
    let chain = Arc::new(crate::replicate::Chain::new(
        unmapped as Arc<dyn crate::replicate::MapSource>,
        farm.peers(),
        Arc::clone(&farm.nodes[0].repairs),
        ChainConfig::default(),
    ));
    let confirmer = PeerConfirmer::new(chain, Arc::clone(&farm.nodes[0].repairs), &config());
    let digest = sha(b"acknowledged with no map");
    confirmer
        .track(digest, farm.nodes[1].id, 1)
        .await
        .expect("tracked");
    let now = Instant::now();
    confirmer.drained(&digest, now);

    assert_eq!(confirmer.confirm(now + GRACE).await.owed, 1);
    assert_eq!(owed(&farm), [digest]);
    assert!(confirmer.is_empty());
    assert_eq!(asked(&farm), 0);
}

/// Catches a digest forgotten when its debt could not be journalled (the survivor would
/// then hold the only copy and owe nothing): the journal's disk fails once, so the
/// confirmation keeps the digest and owes nothing (the journal has stopped after its
/// failure, so it is not forgotten with no debt). A full set whose journal fails reports it to the caller, which must not rely on a
/// confirmation. Mutants: the digest removed whether or not the debt was written; the
/// failure of a full set's journal swallowed.
#[tokio::test]
async fn a_debt_that_cannot_be_journalled_keeps_the_digest_for_the_next_pass() {
    let farm = Farm::start(2, ChainConfig::default()).await;
    let mut one = config();
    one.unconfirmed_cap = 1;
    let confirmer = confirmer(&farm, &one);
    let digest = acknowledged(&farm, &confirmer, 9).await;
    let now = Instant::now();
    confirmer.drained(&digest, now);

    farm.nodes[0]
        .fs
        .fail_at(farm.nodes[0].fs.ops(), kbf_store::Fault::Error);
    assert_eq!(confirmer.confirm(now + GRACE).await, Confirmed::default());
    assert!(confirmer.is_tracked(&digest));
    assert!(owed(&farm).is_empty());

    // The journal stops after a failure (as `Repairs::owe` says), so the same set, still
    // full with the kept digest, cannot take a debt for another digest either.
    let refused = confirmer
        .track(sha(b"over the cap"), farm.nodes[1].id, 1)
        .await;
    assert!(
        matches!(refused, Err(crate::StorageError::Unavailable(_))),
        "{refused:?}"
    );
    assert!(confirmer.is_tracked(&digest));
}

/// Catches a peer whose address cannot be dialled being taken for durable: the map names
/// the peer with an address no channel can be made to, so nothing can be asked, and the
/// blob is owed. Mutant: an address that cannot be dialled skips the debt.
#[tokio::test]
async fn a_peer_that_cannot_be_dialled_is_owed() {
    let farm = Farm::start(2, ChainConfig::default()).await;
    let confirmer = confirmer(&farm, &config());
    let digest = acknowledged(&farm, &confirmer, 11).await;
    let good = farm.map(2, 2, &[]);
    let members = (1..=2)
        .map(|id| {
            let mut member = good
                .member(kbf_map::MemberId(id))
                .expect("a member")
                .clone();
            if id == 2 {
                member.addr = "not an address".to_owned();
            }
            member
        })
        .collect();
    let bad = Arc::new(
        kbf_map::Map::new(2, members, kbf_map::Rule::Rendezvous, 2, 1, Vec::new()).expect("map"),
    );
    farm.publish_to(0, &bad);
    let now = Instant::now();
    confirmer.drained(&digest, now);

    assert_eq!(confirmer.confirm(now + GRACE).await.owed, 1);
    assert_eq!(owed(&farm), [digest]);
}
