//! A write through a chain of two real services: what is held where, and when the writer is
//! told.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use bytes::Bytes;
use futures::{StreamExt as _, stream};
use kbf_map::MemberState;
use kbf_proto::google::bytestream::byte_stream_client::ByteStreamClient;
use kbf_proto::google::bytestream::{WriteRequest, WriteResponse};
use kbf_types::Digest;
use tonic::metadata::MetadataValue;
use tonic::transport::Channel;
use tonic::{Code, Request, Status};

use super::farm::Farm;
use crate::replicate::{ChainConfig, LEG_HEADER};
use crate::service::MAP_GENERATION_HEADER;
use crate::service::tests::fake::{StreamFault, patterned, sha};
use crate::{Storage, StorageError};

pub(super) const MIB: usize = 1 << 20;

/// A write that should end, ended or hung: the deadline is for a defect that would wait for
/// ever, not a speed.
pub(super) async fn within<T>(call: impl std::future::Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(30), call)
        .await
        .expect("the write hung")
}

pub(super) fn held(farm: &Farm, node: usize, digest: &Digest) -> bool {
    farm.nodes[node]
        .mem
        .blobs
        .lock()
        .expect("lock")
        .contains_key(digest)
}

fn name(digest: &Digest) -> String {
    format!(
        "uploads/u-1/blobs/{}/{}",
        digest.hash_hex(),
        digest.size_bytes
    )
}

/// A write of `parts` (data, finish) under `digest`'s resource name, routed by
/// `generation`, as a copy from member `leg` if given.
async fn raw(
    channel: Channel,
    digest: &Digest,
    parts: &[(&[u8], bool)],
    generation: u64,
    leg: Option<u64>,
) -> Result<WriteResponse, Status> {
    let mut offset = 0;
    let messages: Vec<WriteRequest> = parts
        .iter()
        .enumerate()
        .map(|(i, (data, finish))| {
            let message = WriteRequest {
                resource_name: if i == 0 { name(digest) } else { String::new() },
                write_offset: offset,
                finish_write: *finish,
                data: data.to_vec(),
            };
            offset += data.len() as i64;
            message
        })
        .collect();
    let mut request = Request::new(stream::iter(messages));
    request
        .metadata_mut()
        .insert(MAP_GENERATION_HEADER, MetadataValue::from(generation));
    if let Some(sender) = leg {
        request
            .metadata_mut()
            .insert(LEG_HEADER, MetadataValue::from(sender));
    }
    ByteStreamClient::new(channel)
        .write(request)
        .await
        .map(tonic::Response::into_inner)
}

/// Catches an answer given before the second copy exists, and a copy on the primary that
/// the replica does not have: the replica's commit is held, so the write must not be
/// answered, and the primary must not hold the blob either (present on the primary means
/// the replica has it or a repair is owed); released, both hold the same bytes and the
/// write is answered. Mutant: the primary commits before it waits for the replica.
#[tokio::test]
async fn a_write_is_answered_only_when_both_owners_hold_it() {
    let farm = Farm::start(2, ChainConfig::default()).await;
    let map = farm.map(1, 2, &[]);
    let bytes = farm.blob(&map, 0, 1, 3 * MIB + 17, 1);
    let digest = sha(&bytes);
    farm.nodes[1].mem.knobs.hold_commits();
    let client = farm.client(0, 1);
    let write = client.put(Bytes::from(bytes.clone()));
    tokio::pin!(write);
    let early = tokio::time::timeout(Duration::from_millis(500), &mut write).await;
    assert!(early.is_err(), "answered with the replica's commit held");
    assert!(
        !held(&farm, 0, &digest),
        "the primary holds it before the replica does"
    );
    assert!(!held(&farm, 1, &digest));
    farm.nodes[1].mem.knobs.release_commits();
    assert_eq!(write.await.expect("answered"), digest);
    for node in 0..2 {
        let blobs = farm.nodes[node].mem.blobs.lock().expect("lock");
        assert_eq!(blobs.get(&digest).map(|b| b.to_vec()), Some(bytes.clone()));
    }
}

/// How many chunks a stalled replica lets the writer send.
async fn stalled_pull(chunks: usize, replica_stalls: bool) -> (usize, usize, usize, Farm, Digest) {
    let farm = Farm::start(2, ChainConfig::default()).await;
    let map = farm.map(1, 2, &[]);
    let data: Vec<Vec<u8>> = (0..chunks)
        .map(|i| {
            let mut chunk = patterned(MIB);
            chunk[..8].copy_from_slice(&(i as u64).to_le_bytes());
            chunk
        })
        .collect();
    let digest = sha(&data.concat());
    let (primary, replica) = {
        let owners = map.write_owners(&digest);
        let at = |id| farm.nodes.iter().position(|n| n.id == id).expect("a node");
        (at(owners[0]), at(owners[1]))
    };
    if replica_stalls {
        farm.nodes[replica].mem.knobs.hold_appends(true);
    }
    let pulled = Arc::new(AtomicUsize::new(0));
    let messages = {
        let pulled = Arc::clone(&pulled);
        stream::unfold(0, move |i| {
            let data = data.clone();
            let pulled = Arc::clone(&pulled);
            let resource_name = name(&digest);
            async move {
                let chunk = data.get(i)?;
                pulled.fetch_add(1, Ordering::SeqCst);
                let message = WriteRequest {
                    resource_name: if i == 0 { resource_name } else { String::new() },
                    write_offset: (i * MIB) as i64,
                    finish_write: i + 1 == data.len(),
                    data: chunk.clone(),
                };
                Some((message, i + 1))
            }
        })
    };
    let mut request = Request::new(messages);
    request
        .metadata_mut()
        .insert(MAP_GENERATION_HEADER, MetadataValue::from(1u64));
    let channel = farm.channel(primary);
    let call = tokio::spawn(async move { ByteStreamClient::new(channel).write(request).await });
    // Wait until the pull has not moved for a while: the pipeline is full.
    let mut last = usize::MAX;
    let mut steady = 0;
    while steady < 4 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        let now = pulled.load(Ordering::SeqCst);
        steady = if now == last { steady + 1 } else { 0 };
        last = now;
        if now == chunks {
            break;
        }
    }
    let local = farm.nodes[primary].mem.knobs.chunks.load(Ordering::SeqCst);
    let pulled_at_rest = pulled.load(Ordering::SeqCst);
    farm.nodes[replica].mem.knobs.hold_appends(false);
    let answer = call.await.expect("joined").expect("the write");
    assert_eq!(answer.into_inner().committed_size, (chunks * MIB) as i64);
    for node in [primary, replica] {
        assert!(held(&farm, node, &digest), "node {node} holds the blob");
    }
    (pulled_at_rest, local, chunks, farm, digest)
}

/// Catches a primary that holds the blob whole, or queues it without bound, instead of
/// passing it through: with the replica stalled, a 40 MiB blob must not be read from the
/// writer beyond the few messages the pipeline holds (a writer that was not held back
/// has all 40 pulled), and the primary's own store must not run ahead of the replica by
/// more than that either. Mutants: unbounded queues on the staging and the leg; the
/// blob collected before it is forwarded.
#[tokio::test]
async fn the_blob_passes_through_in_a_bounded_window() {
    let (pulled, local, chunks, _, _) = stalled_pull(40, true).await;
    assert!(
        pulled <= 20,
        "{pulled} of {chunks} messages were taken from the writer"
    );
    assert!(
        local <= 12,
        "the primary's store took {local} chunks ahead of the replica"
    );
}

/// The control of the test above: with nothing stalled, every message is taken, so the
/// bound above is the stall's and not the test's.
#[tokio::test]
async fn an_unstalled_replica_takes_the_whole_blob() {
    let (pulled, local, chunks, _, _) = stalled_pull(40, false).await;
    assert_eq!(pulled, chunks);
    assert!(
        local > 20,
        "only {local} chunks reached the primary's store"
    );
}

/// Catches a stale writer served, and a replica contacted for a write that will be refused:
/// a writer a generation behind the primary is refused STALE_MAP, and neither node stores
/// or is sent a byte.
#[tokio::test]
async fn a_stale_view_is_refused_and_nothing_is_copied() {
    let farm = Farm::start(2, ChainConfig::default()).await;
    farm.publish(&farm.map(2, 2, &[]));
    let map = farm.map(2, 2, &[]);
    let bytes = farm.blob(&map, 0, 1, 4096, 2);
    let answer = farm.client(0, 1).put(Bytes::from(bytes.clone())).await;
    assert!(
        matches!(answer, Err(StorageError::StaleMap { current: 2 })),
        "{answer:?}"
    );
    for node in 0..2 {
        assert_eq!(farm.nodes[node].mem.knobs.chunks.load(Ordering::SeqCst), 0);
        assert!(!held(&farm, node, &sha(&bytes)));
    }
    farm.client(0, 2)
        .put(Bytes::from(bytes))
        .await
        .expect("under the current map");
}

/// Catches a primary that is behind its replica and writes anyway: the replica holds a
/// newer map, so its refusal is the answer (STALE_MAP naming the newer generation, for the
/// writer to refetch), the primary does not hold the blob (no copy under a placement
/// the farm has left), and nothing is acknowledged. Mutant: a replica's STALE_MAP taken as
/// the replica being down.
#[tokio::test]
async fn a_primary_behind_its_replica_does_not_acknowledge_one_copy() {
    let farm = Farm::start(2, ChainConfig::default()).await;
    let old = farm.map(1, 2, &[]);
    let bytes = farm.blob(&old, 0, 1, 4096, 3);
    farm.publish_to(1, &farm.map(2, 2, &[]));
    let answer = farm.client(0, 1).put(Bytes::from(bytes.clone())).await;
    assert!(
        matches!(answer, Err(StorageError::StaleMap { current: 2 })),
        "{answer:?}"
    );
    assert!(!held(&farm, 0, &sha(&bytes)));
    assert!(!held(&farm, 1, &sha(&bytes)));
    assert!(
        farm.nodes[0].repairs.open_records().is_empty(),
        "a refusal is not a copy owed"
    );
}

/// Catches a write answered under a placement the map has left: the map turns (the replica
/// is drained) while the replica's commit is held. When the write completes, the primary
/// must not commit or answer, because the new map places the blob elsewhere; a map that
/// turned without moving the blob changes nothing. Mutant: no re-check before the commit.
#[tokio::test]
async fn a_map_that_turns_during_the_write_is_checked_before_the_answer() {
    let farm = Farm::start(3, ChainConfig::default()).await;
    let old = farm.map(1, 2, &[]);
    let bytes = farm.blob(&old, 0, 1, 4096, 4);
    let digest = sha(&bytes);
    // The same placement under a newer generation: the write completes.
    farm.nodes[1].mem.knobs.hold_commits();
    let client = farm.client(0, 1);
    let write = client.put(Bytes::from(bytes.clone()));
    tokio::pin!(write);
    assert!(
        tokio::time::timeout(Duration::from_millis(300), &mut write)
            .await
            .is_err()
    );
    farm.publish_to(0, &farm.map(2, 2, &[]));
    farm.nodes[1].mem.knobs.release_commits();
    assert_eq!(write.await.expect("same placement"), digest);

    // The placement moves: node 1 drains, so the second owner is another node.
    farm.publish(&farm.map(2, 2, &[]));
    let bytes = farm.blob(&old, 0, 1, 4096, 5);
    let digest = sha(&bytes);
    farm.nodes[1].mem.knobs.hold_commits();
    let client = farm.client(0, 2);
    let write = client.put(Bytes::from(bytes.clone()));
    tokio::pin!(write);
    assert!(
        tokio::time::timeout(Duration::from_millis(300), &mut write)
            .await
            .is_err()
    );
    farm.publish_to(0, &farm.map(3, 2, &[(1, MemberState::Draining)]));
    farm.nodes[1].mem.knobs.release_commits();
    let answer = write.await;
    assert!(
        matches!(answer, Err(StorageError::StaleMap { current: 3 })),
        "{answer:?}"
    );
    assert!(
        !held(&farm, 0, &digest),
        "committed under a placement the map had left"
    );
}

/// Catches a write taken by the wrong node: a client write to the replica (not the
/// primary), to a node that owns nothing, or a copy sent to a node that owns nothing or
/// by the node to itself, and a leg header that is not a member id; none stores anything.
#[tokio::test]
async fn a_write_is_served_only_where_the_map_places_it() {
    let farm = Farm::start(3, ChainConfig::default()).await;
    let map = farm.map(1, 2, &[]);
    let bytes = farm.blob(&map, 0, 1, 64, 6);
    let digest = sha(&bytes);
    let parts: &[(&[u8], bool)] = &[(&bytes, true)];
    let invalid = |answer: Result<WriteResponse, Status>| {
        let status = answer.expect_err("refused");
        assert_eq!(status.code(), Code::InvalidArgument, "{status:?}");
        status.message().to_owned()
    };
    let replica = invalid(raw(farm.channel(1), &digest, parts, 1, None).await);
    assert!(replica.contains("not the primary"), "{replica}");
    let third = invalid(raw(farm.channel(2), &digest, parts, 1, None).await);
    assert!(third.contains("not the primary"), "{third}");
    let to_third = invalid(raw(farm.channel(2), &digest, parts, 1, Some(1)).await);
    assert!(to_third.contains("does not own it"), "{to_third}");
    let to_self = invalid(raw(farm.channel(1), &digest, parts, 1, Some(2)).await);
    assert!(to_self.contains("does not own it"), "{to_self}");
    let garbled = {
        let mut request = Request::new(stream::iter(vec![WriteRequest::default()]));
        request
            .metadata_mut()
            .insert(LEG_HEADER, "primary".parse().expect("ASCII"));
        ByteStreamClient::new(farm.channel(1)).write(request).await
    };
    let said = garbled.expect_err("refused");
    assert_eq!(said.code(), Code::InvalidArgument);
    assert!(said.message().contains(LEG_HEADER));
    for node in 0..3 {
        assert!(!held(&farm, node, &digest));
    }
    // From the primary, to the replica that owns it: taken, and forwarded nowhere.
    raw(farm.channel(1), &digest, parts, 1, Some(1))
        .await
        .expect("a copy");
    assert!(held(&farm, 1, &digest) && !held(&farm, 0, &digest) && !held(&farm, 2, &digest));
}

/// Catches a replica that acknowledges bytes it has not verified against the digest the
/// copy names, and a primary that does: bad bytes sent to the replica as a copy are
/// refused INVALID_ARGUMENT and not stored; bad bytes written to the primary are refused
/// and stored on neither node. Mutant: the digest check removed from the staging writer.
#[tokio::test]
async fn bytes_that_are_not_the_digest_are_stored_nowhere() {
    let farm = Farm::start(2, ChainConfig::default()).await;
    let map = farm.map(1, 2, &[]);
    let bytes = farm.blob(&map, 0, 1, 64, 7);
    let digest = sha(&bytes);
    let mut wrong = bytes.clone();
    wrong[10] ^= 1;
    let to_replica = raw(farm.channel(1), &digest, &[(&wrong, true)], 1, Some(1)).await;
    assert_eq!(
        to_replica.expect_err("refused").code(),
        Code::InvalidArgument
    );
    let to_primary = raw(farm.channel(0), &digest, &[(&wrong, true)], 1, None).await;
    let status = to_primary.expect_err("refused");
    assert_eq!(status.code(), Code::InvalidArgument);
    assert!(
        status.message().contains("hashes to"),
        "{}",
        status.message()
    );
    for node in 0..2 {
        assert!(!held(&farm, node, &digest));
        assert_eq!(farm.nodes[node].mem.puts.load(Ordering::SeqCst), 0);
    }
}

/// Catches a write that does not hold to the size its digest names, through the streamed
/// path: more bytes than declared, a stream that ends before `finish_write`, and fewer
/// bytes than declared all refuse INVALID_ARGUMENT and store nothing, on the primary and on
/// a replica.
#[tokio::test]
async fn a_streamed_write_holds_to_its_declared_size() {
    let farm = Farm::start(2, ChainConfig::default()).await;
    let map = farm.map(1, 2, &[]);
    let bytes = farm.blob(&map, 0, 1, 64, 8);
    let digest = sha(&bytes);
    for (channel, leg) in [(0, None), (1, Some(1))] {
        let cases: [&[(&[u8], bool)]; 3] = [
            &[(&[0; 65], true)],
            &[(&bytes[..10], false)],
            &[(&bytes[..10], true)],
        ];
        for parts in cases {
            let answer = raw(farm.channel(channel), &digest, parts, 1, leg).await;
            assert_eq!(answer.expect_err("refused").code(), Code::InvalidArgument);
        }
    }
    for node in 0..2 {
        assert!(!held(&farm, node, &digest));
    }
}

/// Catches a node that keeps taking bytes past the size its digest names, until the writer
/// chooses to stop: the call is refused as soon as the declared size is passed, though the
/// writer would send more (a writer that never finishes could otherwise fill the store's
/// write tier under a small digest). On a primary and on a replica.
#[tokio::test]
async fn a_writer_is_stopped_at_the_size_it_declared() {
    let farm = Farm::start(2, ChainConfig::default()).await;
    let map = farm.map(1, 2, &[]);
    let bytes = farm.blob(&map, 0, 1, 64, 17);
    let digest = sha(&bytes);
    for (node, leg) in [(0, None), (1, Some(1))] {
        let first = WriteRequest {
            resource_name: name(&digest),
            write_offset: 0,
            finish_write: false,
            data: vec![0; 70],
        };
        let messages = stream::once(async { first }).chain(stream::pending());
        let mut request = Request::new(messages);
        request
            .metadata_mut()
            .insert(MAP_GENERATION_HEADER, MetadataValue::from(1u64));
        if let Some(sender) = leg {
            request
                .metadata_mut()
                .insert(LEG_HEADER, MetadataValue::from(sender));
        }
        let channel = farm.channel(node);
        let answer = within(ByteStreamClient::new(channel).write(request)).await;
        assert_eq!(answer.expect_err("refused").code(), Code::InvalidArgument);
    }
}

/// Catches a blob the primary already holds being sent on again, or refused when the
/// replica is down: a second write of a held blob is answered at once, unread, with the
/// replica cut off.
#[tokio::test]
async fn a_blob_the_primary_holds_is_answered_without_the_replica() {
    let farm = Farm::start(2, ChainConfig::default()).await;
    let map = farm.map(1, 2, &[]);
    let bytes = farm.blob(&map, 0, 1, 4096, 9);
    farm.client(0, 1)
        .put(Bytes::from(bytes.clone()))
        .await
        .expect("first");
    let chunks = farm.nodes[1].mem.knobs.chunks.load(Ordering::SeqCst);
    farm.nodes[1].switch.cut();
    farm.client(0, 1)
        .put(Bytes::from(bytes))
        .await
        .expect("held");
    assert_eq!(
        farm.nodes[1].mem.knobs.chunks.load(Ordering::SeqCst),
        chunks
    );
}

/// Catches a primary whose own store failing is answered, or half-kept: every point at which
/// the primary's streamed write can fail (an append, the end of the stream, the commit;
/// an error or a panic; early in a long blob and in a short one) fails the write INTERNAL
/// and the primary does not hold the blob.
#[tokio::test]
async fn a_failure_of_the_primarys_own_store_fails_the_write() {
    let farm = Farm::start(2, ChainConfig::default()).await;
    let map = farm.map(1, 2, &[]);
    let faults = [
        StreamFault::AppendFails,
        StreamFault::AppendPanics,
        StreamFault::FinishFails,
        StreamFault::FinishPanics,
        StreamFault::CommitFails,
    ];
    for (round, fault) in faults.into_iter().enumerate() {
        for len in [4096, 6 * MIB] {
            *farm.nodes[0].mem.knobs.fault.lock().expect("lock") = fault;
            let bytes = farm.blob(&map, 0, 1, len, 20 + round as u8);
            let answer = farm.client(0, 1).put(Bytes::from(bytes.clone())).await;
            assert!(
                matches!(answer, Err(StorageError::Internal(_))),
                "{fault:?} on {len} bytes: {answer:?}"
            );
            assert!(!held(&farm, 0, &sha(&bytes)), "{fault:?}");
        }
    }
}

/// Catches a replica's store failing being taken for a down replica or an answer: the
/// refusal (INTERNAL) is the writer's answer, and the primary does not hold the blob.
#[tokio::test]
async fn a_failure_of_the_replicas_store_fails_the_write() {
    let farm = Farm::start(2, ChainConfig::default()).await;
    let map = farm.map(1, 2, &[]);
    *farm.nodes[1].mem.knobs.fault.lock().expect("lock") = StreamFault::AppendFails;
    let bytes = farm.blob(&map, 0, 1, 6 * MIB, 14);
    let answer = farm.client(0, 1).put(Bytes::from(bytes.clone())).await;
    assert!(
        matches!(answer, Err(StorageError::Internal(_))),
        "{answer:?}"
    );
    assert!(!held(&farm, 0, &sha(&bytes)));
}

/// Catches a primary whose store cannot stream being made to hold the blob whole to pass
/// it on: it refuses (FAILED_PRECONDITION) and stores nothing; the same node, outside a
/// chain, still takes a write the buffered way.
#[tokio::test]
async fn a_primary_that_cannot_stream_refuses_rather_than_buffer() {
    let farm = Farm::start(2, ChainConfig::default()).await;
    let map = farm.map(1, 2, &[]);
    let bytes = farm.blob(&map, 0, 1, 4096, 15);
    farm.nodes[0]
        .mem
        .knobs
        .enabled
        .store(false, Ordering::SeqCst);
    let digest = sha(&bytes);
    let answer = raw(farm.channel(0), &digest, &[(&bytes, true)], 1, None).await;
    assert_eq!(
        answer.expect_err("refused").code(),
        Code::FailedPrecondition
    );
    assert!(!held(&farm, 0, &digest) && !held(&farm, 1, &digest));
}

/// Catches a primary that keeps working for a writer that left: the writer's call is
/// dropped part way through a long blob; nothing is stored, and both nodes take the
/// next write.
#[tokio::test]
async fn a_writer_that_leaves_part_way_leaves_nothing() {
    let farm = Farm::start(2, ChainConfig::default()).await;
    let map = farm.map(1, 2, &[]);
    let bytes = farm.blob(&map, 0, 1, 8 * MIB, 16);
    let digest = sha(&bytes);
    let sent = Arc::new(AtomicUsize::new(0));
    let messages = {
        let (sent, bytes, resource) = (Arc::clone(&sent), bytes.clone(), name(&digest));
        stream::unfold(0, move |i| {
            let (sent, bytes, resource) = (Arc::clone(&sent), bytes.clone(), resource.clone());
            async move {
                if i == 3 {
                    futures::future::pending::<()>().await;
                }
                sent.fetch_add(1, Ordering::SeqCst);
                let message = WriteRequest {
                    resource_name: if i == 0 { resource } else { String::new() },
                    write_offset: (i * MIB) as i64,
                    finish_write: false,
                    data: bytes[i * MIB..(i + 1) * MIB].to_vec(),
                };
                Some((message, i + 1))
            }
        })
    };
    let mut request = Request::new(messages);
    request
        .metadata_mut()
        .insert(MAP_GENERATION_HEADER, MetadataValue::from(1u64));
    let channel = farm.channel(0);
    let call = tokio::spawn(async move { ByteStreamClient::new(channel).write(request).await });
    while sent.load(Ordering::SeqCst) < 3 {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    call.abort();
    let _ = call.await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(!held(&farm, 0, &digest) && !held(&farm, 1, &digest));
    farm.client(0, 1)
        .put(Bytes::from(bytes))
        .await
        .expect("the next write");
    assert!(held(&farm, 0, &digest) && held(&farm, 1, &digest));
}

/// Catches a node outside a chain that cannot take a streamed write, or that takes a copy:
/// with a store that streams, the blob is written through the stream (chunks appended,
/// one commit) and checked against its digest; with a leg header the call is refused, as
/// there is no chain it could be part of.
#[tokio::test]
async fn a_node_outside_a_chain_streams_when_its_store_can() {
    use crate::service::tests::fake::{Mem, serve};
    let mem = Arc::new(Mem::default());
    mem.knobs.enabled.store(true, Ordering::SeqCst);
    let channel = serve(&mem, 1 << 30).await;
    let bytes = patterned(3 * MIB);
    let digest = sha(&bytes);
    let client = crate::service::RemoteStorage::new(channel.clone(), 0);
    client
        .put(Bytes::from(bytes.clone()))
        .await
        .expect("streamed");
    assert_eq!(mem.knobs.chunks.load(Ordering::SeqCst), 3);
    assert_eq!(mem.puts.load(Ordering::SeqCst), 1);
    assert!(mem.blobs.lock().expect("lock").contains_key(&digest));
    let other = sha(b"abc");
    let wrong = raw(channel.clone(), &other, &[(b"abd", true)], 0, None).await;
    assert_eq!(wrong.expect_err("refused").code(), Code::InvalidArgument);
    let copy = raw(channel, &other, &[(b"abc", true)], 0, Some(1)).await;
    let status = copy.expect_err("refused");
    assert_eq!(status.code(), Code::InvalidArgument);
    assert!(
        status.message().contains("no chain"),
        "{}",
        status.message()
    );
    assert!(!mem.blobs.lock().expect("lock").contains_key(&other));
}

/// Catches a store whose blocking `stream` call fails being taken for one that has no
/// stream (the write would then go the buffered way, or be refused as FAILED_PRECONDITION):
/// the failure is INTERNAL, and nothing is stored.
#[tokio::test]
async fn a_store_whose_stream_call_fails_fails_the_write() {
    let farm = Farm::start(2, ChainConfig::default()).await;
    let map = farm.map(1, 2, &[]);
    let bytes = farm.blob(&map, 0, 1, 4096, 18);
    farm.nodes[0]
        .mem
        .knobs
        .open_panics
        .store(true, Ordering::SeqCst);
    let digest = sha(&bytes);
    let answer = raw(farm.channel(0), &digest, &[(&bytes, true)], 1, None).await;
    let status = answer.expect_err("refused");
    assert_eq!(status.code(), Code::Internal, "{status:?}");
    assert!(!held(&farm, 0, &digest) && !held(&farm, 1, &digest));
}
