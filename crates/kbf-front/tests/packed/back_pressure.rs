//! The store's back-pressure, as a client of a proxy given packs sees it: a 503 `SlowDown` is
//! waited out and the upload is stored, and an upload whose `grpc-timeout` passes first is ended
//! DEADLINE_EXCEEDED, never UNAVAILABLE, on the batch call and on the ByteStream Write that the
//! proxy reads as raw HTTP/2. The clock is the runtime's, paused, so the waits and the deadlines
//! pass when nothing else can run, not by waiting.

use std::num::NonZeroUsize;
use std::time::Duration;

use futures::stream;
use kbf_front::{Owners, PackSettings};
use kbf_proto::google::bytestream::WriteRequest;
use kbf_proto::google::bytestream::byte_stream_client::ByteStreamClient;
use kbf_proto::reapi::BatchUpdateBlobsRequest;
use kbf_proto::reapi::content_addressable_storage_client::ContentAddressableStorageClient;
use kbf_types::Digest;
use tonic::{Code, Request, Status};

use crate::harness::{Fake, node as fake_node, owner};
use crate::support::{Rig, blob, item, missing, update};

const OK: i32 = Code::Ok as i32;

/// The `grpc-timeout` the deadline tests send.
const TIMEOUT: Duration = Duration::from_secs(10);

/// Default packs, but `slots` writes at the store at once.
fn slots(slots: usize) -> PackSettings {
    PackSettings {
        store_writes: NonZeroUsize::new(slots).expect("not zero"),
        ..PackSettings::default()
    }
}

/// Packs as `settings` say, in front of a fake node that streams (which the uploads into packs
/// never reach).
async fn rig_over_fake(settings: PackSettings) -> Rig {
    let fake = Fake::new(false);
    let owners = Owners::single(owner("a", fake_node(&fake).await));
    Rig::over(owners, settings).await
}

/// The whole of `bytes` as one Write message for the blob `digest`.
fn whole(digest: &Digest, bytes: &[u8]) -> Vec<WriteRequest> {
    let name = format!(
        "main/uploads/u1/blobs/{}/{}",
        digest.hash_hex(),
        digest.size_bytes
    );
    vec![WriteRequest {
        resource_name: name,
        write_offset: 0,
        finish_write: true,
        data: bytes.to_vec(),
    }]
}

/// What a Write of `messages`, sent with `timeout` as its `grpc-timeout` if any, is
/// acknowledged with, or the status that ended it.
async fn write(
    rig: &Rig,
    messages: Vec<WriteRequest>,
    timeout: Option<Duration>,
) -> Result<i64, Status> {
    let mut request = Request::new(stream::iter(messages));
    if let Some(timeout) = timeout {
        request.set_timeout(timeout);
    }
    let sent = ByteStreamClient::new(rig.front.clone())
        .write(request)
        .await?;
    Ok(sent.into_inner().committed_size)
}

/// Catches the failure this bound exists for, on the proxy's batch call: a batch whose pack the
/// store answers `SlowDown` five times (more than the pack writer's three attempts) is
/// acknowledged OK for every blob and held.
#[tokio::test(start_paused = true)]
async fn a_batch_the_store_slows_down_is_stored_once_it_takes_it() {
    let rig = Rig::new().await;
    let blobs = [blob("slowed alpha"), blob("slowed beta")];
    rig.store.slow_down_puts(5);

    assert_eq!(update(&rig.front, &blobs).await, [OK, OK]);

    let digests: Vec<Digest> = blobs.iter().map(|(digest, _)| *digest).collect();
    assert!(
        missing(&rig.front, &digests)
            .await
            .expect("asked")
            .is_empty()
    );
    assert_eq!(rig.store.puts(), 6);
    assert_eq!(rig.packs.metrics().write_failures.get(), 0);
}

/// Catches back-pressure that ends the proxy's batch with code 14 for each blob rather than the
/// call DEADLINE_EXCEEDED: with one slot, a batch the store answers only `SlowDown` ends
/// DEADLINE_EXCEEDED when its 10 s `grpc-timeout` passes, holds nothing, and is acknowledged when
/// sent again once the store takes writes.
#[tokio::test(start_paused = true)]
async fn a_batch_whose_deadline_passes_while_the_store_slows_down_is_deadline_exceeded() {
    let rig = Rig::with(slots(1)).await;
    let (digest, bytes) = blob("never taken");
    rig.store.slow_down_puts(u32::MAX);
    let mut request = Request::new(BatchUpdateBlobsRequest {
        requests: vec![item(&digest, &bytes)],
        ..BatchUpdateBlobsRequest::default()
    });
    request.set_timeout(TIMEOUT);

    let ended = ContentAddressableStorageClient::new(rig.front.clone())
        .batch_update_blobs(request)
        .await;

    assert_eq!(ended.expect_err("ended").code(), Code::DeadlineExceeded);
    assert_eq!(
        missing(&rig.front, &[digest]).await.expect("asked"),
        [digest]
    );
    rig.store.slow_down_puts(0);
    assert_eq!(update(&rig.front, &[(digest, bytes)]).await, [OK]);
}

/// Catches the same failure on the proxy's raw ByteStream Write: a blob whose pack the store
/// answers `SlowDown` five times is acknowledged with its size and held.
#[tokio::test(start_paused = true)]
async fn a_write_the_store_slows_down_is_stored_once_it_takes_it() {
    let rig = rig_over_fake(PackSettings::default()).await;
    let (digest, bytes) = blob("a slowed write");
    rig.store.slow_down_puts(5);

    let written = write(&rig, whole(&digest, &bytes), None).await;

    assert_eq!(written.expect("acknowledged"), bytes.len() as i64);
    assert!(rig.index.has_all(&[digest]));
    assert_eq!(rig.store.puts(), 6);
}

/// Catches a deadline the raw ByteStream Write does not read from its headers: with one slot, a
/// Write the store answers only `SlowDown` ends DEADLINE_EXCEEDED when its `grpc-timeout` passes,
/// and is acknowledged when sent again once the store takes writes.
#[tokio::test(start_paused = true)]
async fn a_write_whose_deadline_passes_while_the_store_slows_down_is_deadline_exceeded() {
    let rig = rig_over_fake(slots(1)).await;
    let (digest, bytes) = blob("a write never taken");
    rig.store.slow_down_puts(u32::MAX);

    let ended = write(&rig, whole(&digest, &bytes), Some(TIMEOUT)).await;

    assert_eq!(ended.expect_err("ended").code(), Code::DeadlineExceeded);
    assert!(rig.index.has_none(&[digest]));
    rig.store.slow_down_puts(0);
    let again = write(&rig, whole(&digest, &bytes), None).await;
    assert_eq!(again.expect("acknowledged"), bytes.len() as i64);
}
