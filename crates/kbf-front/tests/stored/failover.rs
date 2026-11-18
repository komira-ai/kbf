//! A store that fails over its metadata, as a client of a frontend with no storage node sees it:
//! while the store answers every call 503 for 30 s, the uploads, reads and lookups in flight wait
//! and then succeed, the waits counted; a store that stays down past the bound of the waits ends
//! the call UNAVAILABLE, saying so, which the client repeats; and a call whose `grpc-timeout`
//! passes first is ended DEADLINE_EXCEEDED. The clock is the runtime's, paused, so a window of
//! 30 s passes while nothing else can run, not by waiting; what is asserted is each call's
//! answer and what the store and the waits counted.

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use kbf_front::store_metrics::Op;
use kbf_front::store_wait::STORE_WAIT;
use kbf_proto::google::bytestream::ReadRequest;
use kbf_proto::google::bytestream::byte_stream_client::ByteStreamClient;
use kbf_proto::reapi::BatchUpdateBlobsRequest;
use kbf_proto::reapi::content_addressable_storage_client::ContentAddressableStorageClient;
use kbf_types::Digest;
use tonic::{Code, Request};

use crate::bytestream::{pieces, read as read_stream, read_name, write};
use crate::cas::{item, missing, read, update};
use crate::rig::{Rig, SMALL, blob_len, blob_of};

const OK: i32 = Code::Ok as i32;
const UNAVAILABLE: i32 = Code::Unavailable as i32;

/// A failover of the store's metadata: the longest the store answers 503 for.
const FAILOVER: Duration = Duration::from_secs(30);

/// A rig holding the blobs `small` (packed) and `large` (an object of its own, past [`SMALL`]).
async fn holding() -> (Arc<Rig>, (Digest, Bytes), (Digest, Bytes)) {
    let rig = Arc::new(Rig::new().await);
    let small = blob_of(b"held before the failover");
    let large = blob_len(3 * SMALL as usize, 7);
    let stored = update(&rig, vec![item(&small.0, &small.1)]).await;
    assert_eq!(stored.expect("answered"), [OK]);
    let written = write(&rig, pieces(&large.0, &large.1, 512)).await;
    assert_eq!(written.expect("acknowledged"), large.1.len() as i64);
    (rig, small, large)
}

/// Catches a call that fails while the store fails over instead of waiting: with every call of
/// the store answered 503 for 30 s, a batch upload, a small and a large ByteStream Write, a batch
/// read, and a small and a large ByteStream Read, all sent as the failover begins, each succeed
/// with the bytes they sent or asked for; the store refused calls during the window, the waits
/// of the writes, reads and lengths were counted, and no call ran out of its waits.
#[tokio::test(start_paused = true)]
async fn every_call_in_flight_succeeds_through_a_failover_of_30_seconds() {
    let (rig, held, held_large) = holding().await;
    let fresh = blob_of(b"uploaded during the failover");
    let streamed = blob_len(100, 3);
    let streamed_large = blob_len(2 * SMALL as usize, 9);
    rig.store.fail_over_for(FAILOVER);

    let batch = {
        let (rig, fresh) = (Arc::clone(&rig), fresh.clone());
        tokio::spawn(async move { update(&rig, vec![item(&fresh.0, &fresh.1)]).await })
    };
    let small_write = {
        let (rig, streamed) = (Arc::clone(&rig), streamed.clone());
        tokio::spawn(async move { write(&rig, pieces(&streamed.0, &streamed.1, 40)).await })
    };
    let large_write = {
        let (rig, large) = (Arc::clone(&rig), streamed_large.clone());
        tokio::spawn(async move { write(&rig, pieces(&large.0, &large.1, 512)).await })
    };
    let batch_read = {
        let (rig, digest) = (Arc::clone(&rig), held.0);
        tokio::spawn(async move { read(&rig, &[digest]).await })
    };
    let small_read = {
        let (rig, digest) = (Arc::clone(&rig), held.0);
        tokio::spawn(async move { read_stream(&rig, &read_name(&digest), 0, 0).await })
    };
    let large_read = {
        let (rig, digest) = (Arc::clone(&rig), held_large.0);
        tokio::spawn(async move { read_stream(&rig, &read_name(&digest), 0, 0).await })
    };

    assert_eq!(batch.await.expect("ran").expect("answered"), [OK]);
    assert_eq!(small_write.await.expect("ran").expect("acknowledged"), 100);
    let large_len = streamed_large.1.len() as i64;
    assert_eq!(
        large_write.await.expect("ran").expect("acknowledged"),
        large_len
    );
    let batch_read = batch_read.await.expect("ran").expect("answered");
    assert_eq!(batch_read, [(OK, held.1.to_vec())]);
    let small_read = small_read.await.expect("ran").expect("read");
    assert_eq!(small_read, held.1.to_vec());
    let large_read = large_read.await.expect("ran").expect("read");
    assert_eq!(large_read, held_large.1.to_vec());
    let asked = [fresh.0, streamed.0, streamed_large.0];
    assert!(missing(&rig, &asked).await.expect("asked").is_empty());
    assert!(
        rig.store.failover_refusals() >= 6,
        "the store never refused"
    );
    let waits = rig.stored.packs().store_waits();
    for op in [Op::Put, Op::Get, Op::Head] {
        assert!(waits.waits(op).count() > 0, "no wait of {op:?}");
        assert_eq!(waits.exhausted(op), 0, "{op:?} ran out of its waits");
    }
    assert_eq!(rig.stored.packs().metrics().write_failures.get(), 0);
}

/// Catches a store down for good that hangs a call, costs each of its store calls the whole bound,
/// or ends it in a way a client does not repeat: with the store answering 503 for an hour, a
/// batch upload with no `grpc-timeout` answers its blob UNAVAILABLE, naming the 503 and saying it
/// was not waited for past the bound, and a batch read after it answers UNAVAILABLE at once; all
/// their calls to the store (the pack's writes under each key, the deletes of the failed keys,
/// the read) waited 60 s in all, once. When the store is back, the same upload is stored.
#[tokio::test(start_paused = true)]
async fn a_store_down_past_the_bound_is_unavailable_saying_so() {
    let (rig, held, _) = holding().await;
    let fresh = blob_of(b"never stored");
    rig.store.fail_over_for(Duration::from_secs(3600));
    let refused_before = rig.store.failover_refusals();

    let upload = update_answer(&rig, &fresh).await;
    let read = read(&rig, &[held.0]).await.expect("answered");

    assert_eq!(upload.code, UNAVAILABLE);
    assert!(
        upload
            .message
            .contains("store answered 503 ServiceUnavailable: injected (not waited for: the store answered 503 past the bound of 60 s of waits, and no call since)"),
        "{}",
        upload.message
    );
    assert_eq!(read[0].0, UNAVAILABLE);
    let waits = rig.stored.packs().store_waits();
    let waited: u64 = Op::ALL.iter().map(|op| waits.waits(*op).sum()).sum();
    assert_eq!(waited, STORE_WAIT.as_nanos() as u64);
    assert!(waits.exhausted(Op::Put) >= 2);
    assert_eq!(waits.exhausted(Op::Get), 1);
    assert!(waits.down());
    assert!(rig.store.failover_refusals() > refused_before);

    rig.store.fail_over_for(Duration::ZERO);
    let again = update_answer(&rig, &fresh).await;
    assert_eq!(again.code, OK, "{}", again.message);
    assert!(!waits.down());
}

/// The status of the one blob of a BatchUpdateBlobs of `blob`.
async fn update_answer(rig: &Rig, blob: &(Digest, Bytes)) -> kbf_proto::google::rpc::Status {
    let response = ContentAddressableStorageClient::new(rig.front.clone())
        .batch_update_blobs(BatchUpdateBlobsRequest {
            requests: vec![item(&blob.0, &blob.1)],
            ..BatchUpdateBlobsRequest::default()
        })
        .await
        .expect("answered")
        .into_inner();
    response.responses[0].status.clone().expect("a status")
}

/// Catches a read held past the client's deadline by the waits, which the transport would end
/// CANCELLED: with the store answering 503 for an hour, a ByteStream Read with a `grpc-timeout`
/// of 10 s ends DEADLINE_EXCEEDED, saying the store did not give its blobs in time.
#[tokio::test(start_paused = true)]
async fn a_read_whose_deadline_passes_during_a_failover_is_deadline_exceeded() {
    let (rig, held, _) = holding().await;
    rig.store.fail_over_for(Duration::from_secs(3600));
    let mut request = Request::new(ReadRequest {
        resource_name: read_name(&held.0),
        read_offset: 0,
        read_limit: 0,
    });
    request.set_timeout(Duration::from_secs(10));

    let ended = ByteStreamClient::new(rig.front.clone()).read(request).await;

    let status = ended.expect_err("ended");
    assert_eq!(status.code(), Code::DeadlineExceeded);
    assert_eq!(
        status.message(),
        "the call's deadline (grpc-timeout) passed before the store gave its blobs"
    );
}
