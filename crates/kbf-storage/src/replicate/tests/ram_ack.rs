//! The chain's acknowledgement point behind `--ram-ack-bytes`
//! (`docs/design/storage-ram-ack.md`, "The new acknowledgement point"): which commit mode
//! each owner uses and why, the leg header that asks the replica for a held answer, the
//! leg dedup that follows from it, and what the node registers with its peer confirmer. The
//! stores are in-memory (`Mem`), which records the mode of every commit it takes; the
//! `kbf-front` crash tests run the same service over real caches.
//!
//! Each wait is an event (`Knobs::commit_arrived`), none is a sleep.

use std::sync::atomic::Ordering;
use std::time::Duration;

use bytes::Bytes;
use futures::stream;
use kbf_map::MemberState;
use kbf_proto::google::bytestream::byte_stream_client::ByteStreamClient;
use kbf_proto::google::bytestream::{WriteRequest, WriteResponse};
use kbf_types::Digest;
use tokio::time::Instant;
use tonic::metadata::MetadataValue;
use tonic::transport::Channel;
use tonic::{Code, Request, Status};

use super::chain::{MIB, held, within};
use super::farm::Farm;
use crate::replicate::{ChainConfig, LEG_HEADER, LEG_HELD_HEADER};
use crate::service::MAP_GENERATION_HEADER;
use crate::service::tests::fake::{patterned, sha};
use crate::{CommitMode, Storage, Through};

fn modes(farm: &Farm, node: usize) -> Vec<CommitMode> {
    farm.nodes[node]
        .mem
        .knobs
        .modes
        .lock()
        .expect("lock")
        .clone()
}

fn ram_only(farm: &Farm, node: usize, digest: &Digest) -> bool {
    farm.nodes[node]
        .mem
        .ram_only
        .lock()
        .expect("lock")
        .contains(digest)
}

/// Every node acknowledges from RAM, as `--ram-ack-bytes` over 0 says.
fn ram_on(farm: &Farm, nodes: &[usize]) {
    for node in nodes {
        farm.nodes[*node]
            .mem
            .knobs
            .ram_ack
            .store(true, Ordering::SeqCst);
    }
}

/// A write of `bytes` as a copy from member `sender`, asking for a held answer if `ask`.
async fn leg(
    channel: Channel,
    bytes: &[u8],
    sender: u64,
    ask: Option<&'static str>,
) -> Result<WriteResponse, Status> {
    let digest = sha(bytes);
    let message = WriteRequest {
        resource_name: format!(
            "uploads/u-1/blobs/{}/{}",
            digest.hash_hex(),
            digest.size_bytes
        ),
        write_offset: 0,
        finish_write: true,
        data: bytes.to_vec(),
    };
    let mut request = Request::new(stream::iter(vec![message]));
    let metadata = request.metadata_mut();
    metadata.insert(MAP_GENERATION_HEADER, MetadataValue::from(1u64));
    metadata.insert(LEG_HEADER, MetadataValue::from(sender));
    if let Some(value) = ask {
        metadata.insert(LEG_HELD_HEADER, MetadataValue::from_static(value));
    }
    ByteStreamClient::new(channel)
        .write(request)
        .await
        .map(tonic::Response::into_inner)
}

/// Catches `--ram-ack-bytes` at its default changing a write: with no node acknowledging
/// from RAM, both owners commit `Durable`, neither registers a digest with its confirmer,
/// and no blob is held in RAM.
#[tokio::test]
async fn at_the_default_no_write_is_held_or_confirmed() {
    let farm = Farm::start(2, ChainConfig::default()).await;
    let map = farm.map(1, 2, &[]);
    let bytes = farm.blob(&map, 0, 1, 4096, 1);
    let digest = sha(&bytes);
    within(farm.client(0, 1).put(Bytes::from(bytes)))
        .await
        .expect("answered");
    assert_eq!(modes(&farm, 0), [CommitMode::Durable]);
    assert_eq!(modes(&farm, 1), [CommitMode::Durable]);
    assert!(!ram_only(&farm, 0, &digest) && !ram_only(&farm, 1, &digest));
    assert!(farm.nodes[0].confirmer.is_empty() && farm.nodes[1].confirmer.is_empty());
}

/// Catches the replica not being asked for a held answer, an owner that does not register
/// the other with its confirmer, and a gauge that is not told: with the primary
/// acknowledging from RAM and the replica's own flag off, both commit `Held` (the replica
/// because the leg asked), the blob is held and not durable on both, the primary's
/// confirmer keeps it for member 2 and the replica's for member 1, and each store was told
/// it keeps one.
#[tokio::test]
async fn both_owners_hold_and_each_confirms_the_other() {
    let farm = Farm::start(2, ChainConfig::default()).await;
    ram_on(&farm, &[0]);
    let map = farm.map(1, 2, &[]);
    let bytes = farm.blob(&map, 0, 1, 4096, 2);
    let digest = sha(&bytes);
    within(farm.client(0, 1).put(Bytes::from(bytes)))
        .await
        .expect("answered");
    assert_eq!(modes(&farm, 0), [CommitMode::Held]);
    assert_eq!(modes(&farm, 1), [CommitMode::Held]);
    assert!(ram_only(&farm, 0, &digest) && ram_only(&farm, 1, &digest));
    assert!(farm.nodes[0].confirmer.is_tracked(&digest));
    assert!(farm.nodes[1].confirmer.is_tracked(&digest));
    for node in 0..2 {
        assert_eq!(
            farm.nodes[node]
                .mem
                .knobs
                .unconfirmed
                .load(Ordering::SeqCst),
            1
        );
    }
}

/// Catches the order replica, primary, client: the replica's commit is parked at its gate
/// (a positive event: it arrived), and then the primary has committed nothing and the
/// write is not answered; released, both commit `Held` and the write is answered.
#[tokio::test]
async fn the_replica_holds_before_the_primary_and_the_client_hear() {
    let farm = Farm::start(2, ChainConfig::default()).await;
    ram_on(&farm, &[0, 1]);
    let map = farm.map(1, 2, &[]);
    let bytes = farm.blob(&map, 0, 1, 3 * MIB + 5, 3);
    farm.nodes[1].mem.knobs.hold_commits();
    let client = farm.client(0, 1);
    let write = tokio::spawn({
        let bytes = Bytes::from(bytes);
        async move { client.put(bytes).await }
    });
    within(farm.nodes[1].mem.knobs.commit_arrived.acquire())
        .await
        .expect("open")
        .forget();
    assert_eq!(
        farm.nodes[0].mem.knobs.commit_arrived.available_permits(),
        0,
        "the primary reached its commit while the replica's was parked"
    );
    assert!(!write.is_finished(), "answered before the replica held it");
    farm.nodes[1].mem.knobs.release_commits();
    within(write).await.expect("join").expect("answered");
    assert_eq!(modes(&farm, 0), [CommitMode::Held]);
    assert_eq!(modes(&farm, 1), [CommitMode::Held]);
}

/// Catches a replica that is down being asked for a held answer, a write through that is
/// not counted for its reason, and a digest registered for a peer that never held it: the
/// replica is cut off, so the primary commits `Through(ReplicaLost)` after it journaled
/// the debt, holds nothing in RAM, and registers no digest.
#[tokio::test]
async fn a_lost_replica_is_a_write_through_with_its_debt() {
    let farm = Farm::start(2, ChainConfig::default()).await;
    ram_on(&farm, &[0]);
    let map = farm.map(1, 2, &[]);
    let bytes = farm.blob(&map, 0, 1, 4096, 4);
    let digest = sha(&bytes);
    farm.nodes[1].switch.cut();
    within(farm.client(0, 1).put(Bytes::from(bytes)))
        .await
        .expect("answered");
    assert_eq!(modes(&farm, 0), [CommitMode::Through(Through::ReplicaLost)]);
    assert!(!ram_only(&farm, 0, &digest));
    assert_eq!(farm.nodes[0].repairs.open_records().len(), 1);
    assert!(farm.nodes[0].confirmer.is_empty());
    assert!(!held(&farm, 1, &digest));
}

/// The same for a replica the map names down (no call is made): `ReplicaLost`, a debt.
/// Catches a down replica that is asked for a held answer or skipped without the debt.
#[tokio::test]
async fn a_replica_the_map_names_down_is_a_write_through_with_its_debt() {
    let farm = Farm::start(2, ChainConfig::default()).await;
    ram_on(&farm, &[0]);
    let map = farm.map(2, 2, &[(1, MemberState::Down)]);
    farm.publish(&map);
    let bytes = farm.blob(&map, 0, 1, 4096, 5);
    within(farm.client(0, 2).put(Bytes::from(bytes)))
        .await
        .expect("answered");
    assert_eq!(modes(&farm, 0), [CommitMode::Through(Through::ReplicaLost)]);
    assert_eq!(farm.nodes[0].repairs.open_records().len(), 1);
}

/// Catches a one-owner write acknowledged from RAM, which would be one copy: with R = 1
/// the node commits `Through(SingleOwner)`, nothing is held in RAM and nothing is
/// registered.
#[tokio::test]
async fn a_blob_with_one_owner_is_a_write_through() {
    let farm = Farm::start(2, ChainConfig::default()).await;
    ram_on(&farm, &[0, 1]);
    let map = farm.map(1, 1, &[]);
    farm.publish(&map);
    let (bytes, primary) = (0..10_000u32)
        .map(|round| {
            let mut bytes = patterned(4096);
            bytes[..4].copy_from_slice(&round.to_le_bytes());
            bytes[4] = 6;
            let primary = farm.primary_of(&map, &sha(&bytes));
            (bytes, primary)
        })
        .next()
        .expect("a blob");
    let digest = sha(&bytes);
    within(farm.client(primary, 1).put(Bytes::from(bytes)))
        .await
        .expect("answered");
    assert_eq!(
        modes(&farm, primary),
        [CommitMode::Through(Through::SingleOwner)]
    );
    assert!(!ram_only(&farm, primary, &digest));
    assert!(farm.nodes[primary].confirmer.is_empty());
}

/// Catches a copy from the repair worker (a leg that does not ask) being held in RAM, and
/// its dedup counting a RAM blob: the replica holds the blob in RAM only; a plain leg
/// writes it (`Durable`, no longer RAM-only) and registers nothing; a leg that asks finds
/// it held and writes nothing.
#[tokio::test]
async fn only_a_leg_that_asks_dedups_on_a_blob_held_in_ram() {
    let farm = Farm::start(2, ChainConfig::default()).await;
    ram_on(&farm, &[1]);
    let map = farm.map(1, 2, &[]);
    let bytes = farm.blob(&map, 0, 1, 4096, 7);
    let digest = sha(&bytes);
    let in_ram = || {
        farm.nodes[1]
            .mem
            .blobs
            .lock()
            .expect("lock")
            .insert(digest, Bytes::from(bytes.clone()));
        farm.nodes[1]
            .mem
            .ram_only
            .lock()
            .expect("lock")
            .insert(digest);
    };

    in_ram();
    within(leg(farm.storage_channel(1), &bytes, 1, Some("1")))
        .await
        .expect("answered");
    assert!(
        modes(&farm, 1).is_empty(),
        "a leg that asks is answered by the held blob"
    );
    assert!(ram_only(&farm, 1, &digest));

    within(leg(farm.storage_channel(1), &bytes, 1, None))
        .await
        .expect("answered");
    assert_eq!(
        modes(&farm, 1),
        [CommitMode::Durable],
        "a plain copy is written"
    );
    assert!(!ram_only(&farm, 1, &digest), "to the disk");
    assert!(farm.nodes[1].confirmer.is_empty());
}

/// Catches a replica that obeys the header on a write that is not a leg: a client's write
/// of the same header is a plain write, committed by the node's own mode, and a header that
/// is not the one value is refused. A leg whose header says anything but `1` is
/// INVALID_ARGUMENT and writes nothing.
#[tokio::test]
async fn the_held_header_is_read_from_a_leg_only_and_only_its_one_value() {
    let farm = Farm::start(2, ChainConfig::default()).await;
    let map = farm.map(1, 2, &[]);
    let bytes = farm.blob(&map, 0, 1, 4096, 8);
    let refused = within(leg(farm.storage_channel(1), &bytes, 1, Some("yes")))
        .await
        .expect_err("refused");
    assert_eq!(refused.code(), Code::InvalidArgument);
    assert!(modes(&farm, 1).is_empty());

    // Not a leg: the same entry, with any value, on a client's write to the primary is
    // ignored: the node does not acknowledge from RAM, so it commits `Durable`.
    let digest = sha(&bytes);
    let message = WriteRequest {
        resource_name: format!(
            "uploads/u-1/blobs/{}/{}",
            digest.hash_hex(),
            digest.size_bytes
        ),
        write_offset: 0,
        finish_write: true,
        data: bytes.clone(),
    };
    let mut request = Request::new(stream::iter(vec![message]));
    let metadata = request.metadata_mut();
    metadata.insert(MAP_GENERATION_HEADER, MetadataValue::from(1u64));
    metadata.insert(LEG_HELD_HEADER, MetadataValue::from_static("no"));
    ByteStreamClient::new(farm.channel(0))
        .write(request)
        .await
        .expect("a client's write ignores the entry");
    assert_eq!(modes(&farm, 0), [CommitMode::Durable]);
}

/// Catches a confirmer pass that asks the peer before this node's own copy is on its disk,
/// or never learns that it is: a registered digest whose own blob is still held in RAM is
/// not asked about, even long after the grace; once the blob is on this node's disk the
/// pass notes it (starting the grace), and after the grace asks the peer, which holds it
/// on its disk: forgotten, nothing owed, and the store is told the set is empty.
#[tokio::test]
async fn the_pass_asks_the_peer_only_after_this_node_has_written_the_blob() {
    let farm = Farm::start(2, ChainConfig::default()).await;
    ram_on(&farm, &[0]);
    let map = farm.map(1, 2, &[]);
    let bytes = farm.blob(&map, 0, 1, 4096, 9);
    let digest = sha(&bytes);
    within(farm.client(0, 1).put(Bytes::from(bytes)))
        .await
        .expect("answered");
    let worker = &farm.nodes[0].worker;
    let now = Instant::now();
    let grace = Duration::from_secs(5);

    // Own copy RAM-only: never asked, however late.
    let done = worker.confirm_pass(now + grace * 100).await;
    assert_eq!((done.durable, done.owed), (0, 0));
    assert_eq!(farm.nodes[1].mem.durable_asked.load(Ordering::SeqCst), 0);
    assert!(farm.nodes[0].confirmer.is_tracked(&digest));

    // The drain wrote it here and on the peer.
    farm.nodes[0]
        .mem
        .ram_only
        .lock()
        .expect("lock")
        .remove(&digest);
    farm.nodes[1]
        .mem
        .ram_only
        .lock()
        .expect("lock")
        .remove(&digest);
    let noted = worker.confirm_pass(now).await;
    assert_eq!(
        (noted.durable, noted.owed),
        (0, 0),
        "the grace has not passed"
    );
    assert_eq!(farm.nodes[1].mem.durable_asked.load(Ordering::SeqCst), 0);
    let done = worker.confirm_pass(now + grace).await;
    assert_eq!((done.durable, done.owed), (1, 0));
    assert_eq!(farm.nodes[1].mem.durable_asked.load(Ordering::SeqCst), 1);
    assert!(farm.nodes[0].confirmer.is_empty());
    assert!(farm.nodes[0].repairs.open_records().is_empty());
    assert_eq!(
        farm.nodes[0].mem.knobs.unconfirmed.load(Ordering::SeqCst),
        0
    );
}
