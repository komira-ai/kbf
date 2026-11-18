//! The store's back-pressure, as a client of a frontend with no storage node sees it: a 503
//! `SlowDown` is waited out and the upload is stored, every write to the store shares the packs'
//! store-write slots, and an upload whose `grpc-timeout` passes first is ended DEADLINE_EXCEEDED,
//! never UNAVAILABLE. The clock is the runtime's, paused, so the waits and the deadlines pass
//! when nothing else can run, not by waiting; every bound is checked by counting the writes the
//! store saw.

use std::num::NonZeroUsize;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use futures::stream;
use kbf_front::PackSettings;
use kbf_front::large_backfill::LargeStore;
use kbf_objstore::{BodyStream, ObjectKey};
use kbf_proto::google::bytestream::byte_stream_client::ByteStreamClient;
use kbf_proto::reapi::BatchUpdateBlobsRequest;
use kbf_proto::reapi::content_addressable_storage_client::ContentAddressableStorageClient;
use tonic::{Code, Request};

use crate::bytestream::{pieces, write};
use crate::cas::{item, missing, update};
use crate::rig::{Rig, SMALL, blob_len, blob_of};

const OK: i32 = Code::Ok as i32;

/// The `grpc-timeout` the deadline tests send.
const TIMEOUT: Duration = Duration::from_secs(10);

/// Packs that take blobs of at most [`SMALL`] bytes and have `slots` writes at the store at once.
async fn rig_with_slots(slots: usize) -> Rig {
    let settings = PackSettings {
        max_blob_bytes: SMALL,
        store_writes: NonZeroUsize::new(slots).expect("not zero"),
        ..PackSettings::default()
    };
    Rig::with(settings).await
}

/// Catches the failure this bound exists for: a batch whose pack the store answers 503
/// `SlowDown` five times (once more than the pack writer's three attempts) is acknowledged OK
/// for every blob, held afterwards, and written six times, with no write failure counted.
#[tokio::test(start_paused = true)]
async fn a_batch_the_store_slows_down_is_stored_once_it_takes_it() {
    let rig = Rig::new().await;
    let (one, one_bytes) = blob_of(b"slowed one");
    let (two, two_bytes) = blob_of(b"slowed two");
    rig.store.slow_down_puts(5);

    let stored = update(&rig, vec![item(&one, &one_bytes), item(&two, &two_bytes)]).await;

    assert_eq!(stored.expect("answered"), [OK, OK]);
    assert!(missing(&rig, &[one, two]).await.expect("asked").is_empty());
    assert_eq!(rig.store.puts(), 6);
    assert_eq!(rig.stored.packs().metrics().write_failures.get(), 0);
}

/// Catches the same failure for a ByteStream Write: a blob whose pack the store answers
/// `SlowDown` five times is acknowledged with its size and held.
#[tokio::test(start_paused = true)]
async fn a_write_the_store_slows_down_is_stored_once_it_takes_it() {
    let rig = Rig::new().await;
    let (digest, bytes) = blob_len(100, 21);
    rig.store.slow_down_puts(5);

    let written = write(&rig, pieces(&digest, &bytes, 40)).await;

    assert_eq!(written.expect("acknowledged"), 100);
    assert!(missing(&rig, &[digest]).await.expect("asked").is_empty());
    assert_eq!(rig.store.puts(), 6);
}

/// Catches back-pressure that ends a batch UNAVAILABLE (each blob code 14) rather than the call
/// DEADLINE_EXCEEDED, and a slot an ended call keeps: with one slot, a batch the store answers
/// only `SlowDown` ends DEADLINE_EXCEEDED when its 10 s `grpc-timeout` passes, holds nothing,
/// and the same batch sent once the store takes writes is acknowledged.
#[tokio::test(start_paused = true)]
async fn a_batch_whose_deadline_passes_while_the_store_slows_down_is_deadline_exceeded() {
    let rig = rig_with_slots(1).await;
    let (one, one_bytes) = blob_of(b"never taken one");
    let (two, two_bytes) = blob_of(b"never taken two");
    let batch = || BatchUpdateBlobsRequest {
        requests: vec![item(&one, &one_bytes), item(&two, &two_bytes)],
        ..BatchUpdateBlobsRequest::default()
    };
    rig.store.slow_down_puts(u32::MAX);
    let mut request = Request::new(batch());
    request.set_timeout(TIMEOUT);

    let ended = ContentAddressableStorageClient::new(rig.front.clone())
        .batch_update_blobs(request)
        .await;

    assert_eq!(ended.expect_err("ended").code(), Code::DeadlineExceeded);
    assert_eq!(missing(&rig, &[one, two]).await.expect("asked"), [one, two]);
    assert_eq!(rig.stored.packs().metrics().write_failures.get(), 0);
    rig.store.slow_down_puts(0);
    let again = update(&rig, batch().requests).await;
    assert_eq!(again.expect("answered"), [OK, OK]);
}

/// Catches the same for a ByteStream Write: with one slot, a Write the store answers only
/// `SlowDown` ends DEADLINE_EXCEEDED when its `grpc-timeout` passes, and is acknowledged when sent
/// again once the store takes writes.
#[tokio::test(start_paused = true)]
async fn a_write_whose_deadline_passes_while_the_store_slows_down_is_deadline_exceeded() {
    let rig = rig_with_slots(1).await;
    let (digest, bytes) = blob_len(100, 22);
    rig.store.slow_down_puts(u32::MAX);
    let mut request = Request::new(stream::iter(pieces(&digest, &bytes, 40)));
    request.set_timeout(TIMEOUT);

    let ended = ByteStreamClient::new(rig.front.clone())
        .write(request)
        .await;

    assert_eq!(ended.expect_err("ended").code(), Code::DeadlineExceeded);
    assert_eq!(missing(&rig, &[digest]).await.expect("asked"), [digest]);
    rig.store.slow_down_puts(0);
    let again = write(&rig, pieces(&digest, &bytes, 40)).await;
    assert_eq!(again.expect("acknowledged"), 100);
}

/// Catches client uploads past the bound reaching the store: with two slots and the store holding
/// every write it gets, eight concurrent batches of one blob each put two writes inside it and no
/// more; once the store lets them on, every batch is acknowledged and no more than two writes were
/// ever inside at once.
#[tokio::test(start_paused = true)]
async fn concurrent_uploads_never_exceed_the_store_writes() {
    let rig = Arc::new(rig_with_slots(2).await);
    rig.store.hold_puts();
    let mut uploads = Vec::new();
    for n in 0..8_u8 {
        let rig = Arc::clone(&rig);
        uploads.push(tokio::spawn(async move {
            let (digest, bytes) = blob_len(50, n);
            update(&rig, vec![item(&digest, &bytes)]).await
        }));
    }

    rig.store.until_inside(2).await;
    for _ in 0..64 {
        tokio::task::yield_now().await;
    }
    assert_eq!(
        rig.store.puts(),
        2,
        "an upload past the slots reached the store"
    );
    rig.store.let_puts(8);
    for upload in uploads {
        assert_eq!(upload.await.expect("ran").expect("answered"), [OK]);
    }

    assert_eq!(rig.store.puts(), 8);
    assert_eq!(rig.store.most_inside(), 2);
}

/// Catches a writer that goes around the slots: with one slot held by a client's batch, a write
/// of an action tree's object through the packs (as the tree writer and base publication make
/// it) does not reach the store until the batch's write ends, and then is stored.
#[tokio::test(start_paused = true)]
async fn a_tree_write_and_a_client_upload_share_the_slots() {
    let rig = Arc::new(rig_with_slots(1).await);
    rig.store.hold_puts();
    let upload = {
        let rig = Arc::clone(&rig);
        tokio::spawn(async move {
            let (digest, bytes) = blob_of(b"the client's blob");
            update(&rig, vec![item(&digest, &bytes)]).await
        })
    };
    rig.store.until_inside(1).await;
    let tree = {
        let rig = Arc::clone(&rig);
        tokio::spawn(async move {
            let key = ObjectKey::new("cas/l/a-manifest").expect("a key");
            let bytes = Bytes::from_static(b"a manifest");
            let body: BodyStream = Box::pin(stream::iter([Ok(bytes)]));
            LargeStore::put(rig.stored.packs(), &key, 10, body).await
        })
    };

    for _ in 0..64 {
        tokio::task::yield_now().await;
    }
    assert_eq!(
        rig.store.puts(),
        1,
        "the tree's write reached the store past the slot"
    );
    rig.store.let_puts(2);
    assert_eq!(upload.await.expect("ran").expect("answered"), [OK]);
    tree.await.expect("ran").expect("stored");

    assert_eq!(rig.store.puts(), 2);
    assert_eq!(rig.store.most_inside(), 1);
    assert!(rig.store.held("cas/l/a-manifest").await.is_some());
}
