//! The bytes a proxy-mode frontend (`--storage`) holds for the batch CAS calls
//! (`kbf_bytes_in_flight`), read from the process gauge over real gRPC to a fake node that
//! does not count: every blob of a `BatchUpdateBlobs` from the decoded request until its own
//! store is done with it, and the copies a `BatchReadBlobs` answers with until the handler
//! returns them. The fake node counts nothing, so the gauge holds only what the proxy counts.
//!
//! The gauge is the process's, so the tests take turns and each starts its peak over.

#[path = "proxy/harness.rs"]
mod harness;

use std::time::Duration;

use harness::{Fake, front, node, owner, pattern};
use kbf_auth::Authorizers;
use kbf_proto::reapi::content_addressable_storage_client::ContentAddressableStorageClient;
use kbf_proto::reapi::{
    BatchReadBlobsRequest, BatchUpdateBlobsRequest, Digest as Wire, batch_update_blobs_request,
};
use kbf_storage::in_flight::process;
use tokio::sync::{Mutex, MutexGuard};

const KIB: usize = 1 << 10;
/// More blobs than a batch has in flight at once (`BATCH_PARALLELISM`, 32), so a count
/// that only covers the blobs being stored cannot reach the whole batch.
const BLOBS: usize = 40;

static TURN: Mutex<()> = Mutex::const_new(());

async fn turn() -> MutexGuard<'static, ()> {
    let turn = TURN.lock().await;
    given_back("before the test").await;
    process().reset_peak();
    turn
}

/// Waits for the count to reach 0: a handler's answer can reach the caller a moment before
/// the last clone of a blob is dropped.
async fn given_back(when: &str) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while process().current() != 0 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "{when}: {} bytes still counted",
            process().current()
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// A blob of `len` bytes the fake node accepts (its first, middle and last byte are the
/// first chunk's pattern) that differs from every other `i`.
fn blob(i: usize, len: usize) -> Vec<u8> {
    let mut data = vec![pattern(0); len];
    data[1] = u8::try_from(i % 251).expect("a byte");
    data[2] = u8::try_from(i / 251).expect("a byte");
    data
}

async fn proxy() -> ContentAddressableStorageClient<tonic::transport::Channel> {
    let fake = Fake::new(false);
    let addr = node(&fake).await;
    let front = front(vec![owner("node", addr)], Authorizers::allow_all(), None).await;
    // The answer of a batch read is over the client's default 4 MiB decode limit.
    ContentAddressableStorageClient::new(front.channel).max_decoding_message_size(16 << 20)
}

/// Catches: a `BatchUpdateBlobs` through the proxy whose blobs are not counted (peak 0),
/// counted only as each starts its store (a batch of 40 blobs with 32 in flight reaches 32
/// of them, never 40), or never given back. The blobs of a batch are all in memory from
/// the decoded request, so the count reaches the batch's size, and no further: held once.
///
/// Catches: `store_one` counting its blob (`Bytes::from(data)` made `counted`) in place of
/// the batch counted when it is decoded; the count removed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_proxied_batch_update_counts_every_blob_until_it_is_stored() {
    let _turn = turn().await;
    let mut cas = proxy().await;
    let len = 64 * KIB;
    let requests = (0..BLOBS)
        .map(|i| {
            let data = blob(i, len);
            batch_update_blobs_request::Request {
                digest: Some(kbf_front::digest_to_proto(&kbf_segments::sha256(&data))),
                data,
                compressor: 0,
            }
        })
        .collect();
    let answered = cas
        .batch_update_blobs(BatchUpdateBlobsRequest {
            requests,
            ..Default::default()
        })
        .await
        .expect("BatchUpdateBlobs")
        .into_inner();
    assert_eq!(answered.responses.len(), BLOBS);
    assert!(
        answered
            .responses
            .iter()
            .all(|r| r.status.as_ref().is_some_and(|s| s.code == 0)),
        "{answered:?}"
    );

    let total = (BLOBS * len) as u64;
    let peak = process().peak();
    assert!(
        peak >= total,
        "the whole batch is counted: {peak} < {total}"
    );
    assert!(peak <= total + (1 << 20), "held once: {peak}");
    given_back("after the batch").await;
}

/// Catches: a `BatchReadBlobs` through the proxy whose answers are not counted (the copy
/// of each blob that the response carries is in memory until the handler returns it: at
/// most 32 sources are ever in flight, so only counting the copies reaches the batch's
/// size), the blob a copy was made from kept counted after, and bytes never given back.
///
/// Catches: the copy's guard removed from `batch_read_blobs`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_proxied_batch_read_counts_every_copy_it_answers_with() {
    let _turn = turn().await;
    let mut cas = proxy().await;
    let len = 128 * KIB;
    let digests: Vec<Wire> = (0..BLOBS)
        .map(|i| Wire {
            // A hash the fake node reads as an ordinary blob (it keys on the first digit).
            hash: format!("{:064x}", i + 1),
            size_bytes: len as i64,
        })
        .collect();
    let answered = cas
        .batch_read_blobs(BatchReadBlobsRequest {
            digests,
            ..Default::default()
        })
        .await
        .expect("BatchReadBlobs")
        .into_inner();
    assert_eq!(answered.responses.len(), BLOBS);
    for r in &answered.responses {
        assert_eq!(r.status.as_ref().map(|s| s.code), Some(0), "{r:?}");
        assert_eq!(r.data.len(), len);
    }

    let total = (BLOBS * len) as u64;
    let peak = process().peak();
    assert!(peak >= total, "every copy is counted: {peak} < {total}");
    assert!(
        peak <= 2 * total,
        "the copies and the blobs being read: {peak}"
    );
    given_back("after the batch").await;
}
