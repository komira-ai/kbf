//! Uploads: FindMissing before each, and the writes themselves.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use kbf_proto::reapi::Digest;
use kbf_transfer::{Cas, CasError, TransferConfig, TransferStats, digest_of, label};
use kbf_types::LeaseId;

use super::engine;
use super::fake::{Fake, Find, lock, within};
use super::front::{chunked, failing};

/// C-11, first half (docs/design/worker-cache.md#10-test-plan). A blob the server
/// holds is not uploaded, as bytes or as chunks (`outputs_present`); one the node read
/// from the server earlier, but the server has since lost, is uploaded; a new one is.
/// Catches FindMissing answered by anything but the server (the node's own copy of a
/// blob says nothing about the server). Mutants seen red: skip the upload of a blob the
/// lease has read (the lost blob was never uploaded); no FindMissing (the held blob was
/// uploaded).
#[tokio::test]
async fn c11_the_server_decides_what_is_uploaded() {
    let fake = Arc::new(Fake::default());
    let held = fake.insert(b"held".to_vec());
    let lost = fake.insert(b"read, then lost".to_vec());
    let engine = engine(fake.serve().await, TransferConfig::default());
    let scope = engine.scope(&LeaseId::new(6, 1));
    assert_eq!(scope.get(&lost).await, Ok(b"read, then lost".to_vec()));
    fake.remove(&lost);
    assert_eq!(scope.put(b"held".to_vec()).await, Ok(held.clone()));
    assert_eq!(
        scope.put(b"read, then lost".to_vec()).await,
        Ok(lost.clone())
    );
    let (_, new, chunks) = chunked(5000, 1000);
    assert_eq!(scope.put_chunks(new.clone(), chunks).await, Ok(new.clone()));
    let (_, again, chunks) = chunked(5000, 700);
    assert_eq!(scope.put_chunks(again, chunks).await, Ok(new.clone()));
    assert_eq!(*lock(&fake.writes), [label(&lost), label(&new)]);
    let stats = scope.stats();
    assert_eq!(
        (
            stats.find_missing_digests,
            stats.outputs_present,
            stats.write_calls
        ),
        (4, 2, 2)
    );
    assert_eq!(stats.written_bytes, lost.size_bytes as u64 + 5000);
}

/// C-11, second half. Against a worker listener that does not serve FindMissing
/// (UNIMPLEMENTED), every blob is uploaded, held or not, and none fails; FindMissing is
/// asked once for the life of the engine (the uploads queued behind that call are not
/// asked either) and counted in `find_missing_unavailable`. Catches UNIMPLEMENTED
/// failing the upload, and the engine asking again on every upload. Mutants seen red:
/// UNIMPLEMENTED fails the upload; no FindMissing (no call reached the server).
#[tokio::test]
async fn c11_without_find_missing_every_blob_is_uploaded_and_none_fails() {
    let fake = Arc::new(Fake::default());
    *lock(&fake.find) = Find::Unimplemented;
    let held = fake.insert(b"held".to_vec());
    let config = TransferConfig {
        find_missing_chunk: 1,
        ..TransferConfig::default()
    };
    let engine = engine(fake.serve().await, config);
    let (a, b, h) = futures::join!(
        engine.put(b"a".to_vec()),
        engine.put(b"b".to_vec()),
        engine.put(b"held".to_vec())
    );
    assert_eq!(
        (a, b, h),
        (Ok(digest_of(b"a")), Ok(digest_of(b"b")), Ok(held.clone()))
    );
    assert_eq!(engine.put(b"c".to_vec()).await, Ok(digest_of(b"c")));
    let mut written = lock(&fake.writes).clone();
    written.sort();
    let mut expected: Vec<String> = [b"a".as_slice(), b"b", b"held", b"c"]
        .iter()
        .map(|b| label(&digest_of(b)))
        .collect();
    expected.sort();
    assert_eq!(written, expected);
    assert_eq!(*lock(&fake.finds), [1], "FindMissing calls");
    let stats = engine.stats();
    assert_eq!(
        (stats.find_missing_calls, stats.find_missing_unavailable),
        (1, 1)
    );
}

/// Catches a FindMissing that fails failing the upload, or retried past reason: one
/// UNAVAILABLE is retried and then answered (the held blob is skipped); a refusal
/// (INVALID_ARGUMENT) is not retried and the blob is uploaded.
#[tokio::test]
async fn a_find_missing_that_fails_leaves_the_blob_to_be_uploaded() {
    let fake = Arc::new(Fake::default());
    let held = fake.insert(b"held".to_vec());
    *lock(&fake.find) = Find::UnavailableFirst(1);
    let engine = engine(fake.serve().await, TransferConfig::default());
    assert_eq!(engine.put(b"held".to_vec()).await, Ok(held.clone()));
    assert!(lock(&fake.writes).is_empty());
    assert_eq!(*lock(&fake.finds), [1, 1]);
    *lock(&fake.find) = Find::Refuse;
    assert_eq!(engine.put(b"held".to_vec()).await, Ok(held.clone()));
    assert_eq!(*lock(&fake.writes), [label(&held)]);
    assert_eq!(*lock(&fake.finds), [1, 1, 1]);
    assert_eq!(engine.stats().find_missing_unavailable, 0);
}

/// One question for a lease's whole output set, the way the container driver asks
/// (the 14,245 distinct outputs of one action took 785 s to upload when each upload
/// asked for itself, 64 uploads at a time). `missing` over 15,000 digests, 14,000 of them
/// held, asks in 15 calls of 1000 (`--transfer-find-missing-chunk`), all 15 out at the
/// server at once, and answers the 1000 it lacks in the order asked, counted in the
/// lease's `find_missing_digests` and `outputs_present`. Storing those 1000 asks
/// nothing more. An answer is used once: a digest not asked about, or asked about and
/// already stored, is asked again on its next put. Mutants seen red: puts ask
/// FindMissing again after `missing` (a 16th call, for the 1000 puts together); the
/// chunks asked one at a time (1 call out at once); the answer kept after its put
/// (the second put of `blobs[0]` asks nothing, 16 calls, not 17).
#[tokio::test]
async fn a_whole_output_set_is_asked_about_in_chunks_at_once() {
    let fake = Arc::new(Fake::default());
    let blobs: Vec<Vec<u8>> = (0..15_000)
        .map(|i| format!("output {i}").into_bytes())
        .collect();
    let digests: Vec<Digest> = blobs.iter().map(|b| digest_of(b)).collect();
    // Every 15th blob is new; the server holds the other 14,000.
    for (i, bytes) in blobs.iter().enumerate() {
        if i % 15 != 0 {
            fake.insert(bytes.clone());
        }
    }
    let new: Vec<Digest> = digests.iter().step_by(15).cloned().collect();
    let engine = engine(fake.serve().await, TransferConfig::default());
    let scope = engine.scope(&LeaseId::new(8, 1));
    fake.close_finds();
    let watch = async {
        within(10, || lock(&fake.finds).len() == 15).await;
        let at_once = lock(&fake.finds).len();
        fake.open_finds();
        at_once
    };
    let (missing, at_once) = futures::join!(scope.missing(&digests), watch);
    assert_eq!(at_once, 15, "FindMissing calls out at the server at once");
    assert_eq!(missing, new);
    assert_eq!(*lock(&fake.finds), [1000; 15], "the digests of each call");
    let stats = scope.stats();
    assert_eq!(
        (stats.find_missing_digests, stats.outputs_present),
        (15_000, 14_000)
    );
    let puts = blobs.iter().step_by(15).map(|b| scope.put(b.clone()));
    let stored: Result<Vec<Digest>, CasError> =
        futures::future::join_all(puts).await.into_iter().collect();
    assert_eq!(stored, Ok(new.clone()));
    assert_eq!(
        lock(&fake.finds).len(),
        15,
        "FindMissing calls after the puts"
    );
    assert_eq!(lock(&fake.writes).len(), 1000);
    let stats = scope.stats();
    assert_eq!(
        (
            stats.find_missing_digests,
            stats.outputs_present,
            stats.write_calls,
            stats.written_blobs
        ),
        (15_000, 14_000, 1000, 1000)
    );
    // Not asked about: asked now, held, not written.
    assert_eq!(scope.put(blobs[1].clone()).await, Ok(digests[1].clone()));
    // Asked about and stored: the answer was used, so it is asked again.
    assert_eq!(scope.put(blobs[0].clone()).await, Ok(digests[0].clone()));
    assert_eq!(lock(&fake.finds).len(), 17);
    assert_eq!(lock(&fake.writes).len(), 1000);
    let stats = scope.stats();
    assert_eq!((stats.outputs_present, stats.written_blobs), (14_002, 1000));
}

/// A whole set asked about against a worker listener without FindMissing: its three
/// calls, out at once, are each answered UNIMPLEMENTED; every digest is answered
/// missing (the one the server holds too) and stored without another question, and
/// the first UNIMPLEMENTED alone is counted (and logged). Mutant seen red: each
/// UNIMPLEMENTED counted (`find_missing_unavailable` 3).
#[tokio::test]
async fn without_find_missing_a_whole_set_is_answered_missing() {
    let fake = Arc::new(Fake::default());
    *lock(&fake.find) = Find::Unimplemented;
    let blobs: Vec<Vec<u8>> = (0..3000)
        .map(|i| format!("blob {i}").into_bytes())
        .collect();
    let digests: Vec<Digest> = blobs.iter().map(|b| digest_of(b)).collect();
    fake.insert(blobs[7].clone());
    let engine = engine(fake.serve().await, TransferConfig::default());
    let scope = engine.scope(&LeaseId::new(8, 2));
    fake.close_finds();
    let watch = async {
        within(10, || lock(&fake.finds).len() == 3).await;
        fake.open_finds();
    };
    let (missing, ()) = futures::join!(scope.missing(&digests), watch);
    assert_eq!(missing, digests);
    assert_eq!(*lock(&fake.finds), [1000; 3]);
    assert_eq!(engine.stats().find_missing_unavailable, 1);
    for bytes in &blobs[..10] {
        scope.put(bytes.clone()).await.expect("stored");
    }
    assert_eq!(lock(&fake.finds).len(), 3);
    assert_eq!(lock(&fake.writes).len(), 10);
    assert_eq!(scope.stats().outputs_present, 0);
    // Turned off, the engine asks nothing more: every digest is missing.
    assert_eq!(engine.missing(&digests[..5]).await, digests[..5]);
    assert_eq!(lock(&fake.finds).len(), 3);
}

/// Catches a write refused with UNAVAILABLE failing at once (a blob in memory can be
/// sent again: it is, after a backoff, and counted in `retries`), and a streamed write
/// retried (its chunks were read once: it fails). `written_blobs` counts the blob the
/// retry stored once, not once per attempt, and not the refused streamed write.
#[tokio::test]
async fn a_write_in_memory_is_retried_and_a_streamed_one_is_not() {
    let fake = Arc::new(Fake::default());
    let engine = engine(fake.serve().await, TransferConfig::default());
    let scope = engine.scope(&LeaseId::new(7, 1));
    fake.fail_writes.store(1, Ordering::SeqCst);
    assert_eq!(scope.put(b"again".to_vec()).await, Ok(digest_of(b"again")));
    let stats = scope.stats();
    assert_eq!(
        (stats.write_calls, stats.retries, stats.written_blobs),
        (2, 1, 1)
    );
    fake.fail_writes.store(1, Ordering::SeqCst);
    let (_, digest, chunks) = chunked(3000, 1000);
    let error = scope
        .put_chunks(digest, chunks)
        .await
        .expect_err("not retried");
    assert!(
        matches!(&error, CasError::Unavailable(_, why) if why.contains("write refused")),
        "{error:?}"
    );
    let stats = scope.stats();
    assert_eq!(
        (stats.write_calls, stats.retries, stats.written_blobs),
        (3, 1, 1)
    );
    // A chunk that cannot be read ends the streamed write.
    let error = scope
        .put_chunks(digest_of(b"abcdef"), failing(b"abc"))
        .await
        .expect_err("unreadable");
    assert!(
        matches!(&error, CasError::Read(_, why) if why == "disk gone"),
        "{error:?}"
    );
}

/// Catches `written_blobs` left uncounted for a streamed write that stored a new blob
/// (the native driver uploads outputs this way), and counted for a write before it is
/// sent (a write refused on every attempt stores nothing). Mutants seen red: no
/// `written_blobs` add on `put_chunks`' success, `(1, 0, 3000)`; `put`'s add moved before
/// the write, counting every attempt, `2` for the refused blob.
#[tokio::test]
async fn written_blobs_counts_blobs_the_server_stored() {
    let fake = Arc::new(Fake::default());
    let config = TransferConfig {
        retries: 1,
        ..TransferConfig::default()
    };
    let engine = engine(fake.serve().await, config);
    let streamed = engine.scope(&LeaseId::new(7, 1));
    let (_, digest, chunks) = chunked(3000, 1000);
    assert_eq!(
        streamed.put_chunks(digest.clone(), chunks).await,
        Ok(digest)
    );
    let stats = streamed.stats();
    assert_eq!(
        (stats.write_calls, stats.written_blobs, stats.written_bytes),
        (1, 1, 3000)
    );
    let refused = engine.scope(&LeaseId::new(7, 2));
    fake.fail_writes.store(2, Ordering::SeqCst);
    let error = refused
        .put(b"never".to_vec())
        .await
        .expect_err("refused on every attempt");
    assert!(matches!(&error, CasError::Unavailable(..)), "{error:?}");
    let stats = refused.stats();
    assert_eq!(
        (stats.write_calls, stats.retries, stats.written_blobs),
        (2, 1, 0)
    );
}

/// Catches a write message larger than `--transfer-chunk-bytes` (the server refuses
/// one past its message limit), for bytes and for chunks, and bytes lost re-cutting
/// them.
#[tokio::test]
async fn writes_are_cut_to_the_chunk_size() {
    let fake = Arc::new(Fake::default());
    let config = TransferConfig {
        chunk_bytes: 1000,
        ..TransferConfig::default()
    };
    let engine = engine(fake.serve().await, config);
    let (bytes, digest, chunks) = chunked(10_000, 4096);
    assert_eq!(
        engine.put_chunks(digest.clone(), chunks).await,
        Ok(digest.clone())
    );
    assert_eq!(engine.get(&digest).await, Ok(bytes));
    let other: Vec<u8> = (0..10_000).map(|i| (i % 13) as u8).collect();
    let digest = engine.put(other.clone()).await.expect("put");
    assert_eq!(engine.get(&digest).await, Ok(other));
    assert_eq!(fake.largest_write.load(Ordering::SeqCst), 1000);
    // An empty blob is one message with no data, the one that finishes the write.
    let (_, empty, chunks) = chunked(0, 1);
    assert_eq!(
        engine.put_chunks(empty.clone(), chunks).await,
        Ok(empty.clone())
    );
    assert!(lock(&fake.writes).contains(&label(&empty)));
}

/// Catches a lease's counters left out of its log line, or named otherwise than the
/// design's `TransferStats`.
#[test]
fn a_leases_stats_print_every_counter() {
    let stats = TransferStats {
        read_calls: 1,
        read_bytes: 2,
        merged_reads: 3,
        find_missing_digests: 4,
        outputs_present: 5,
        write_calls: 6,
        written_bytes: 7,
        written_blobs: 8,
        retries: 9,
    };
    assert_eq!(
        stats.to_string(),
        "read_calls=1 read_bytes=2 merged_reads=3 find_missing_digests=4 outputs_present=5 \
         write_calls=6 written_bytes=7 written_blobs=8 retries=9"
    );
}
