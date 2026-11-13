//! `Backend::stored`: what Execute and the scheduler ask of the store, answered by the packs and
//! the state store with no storage node.

use std::io;

use bytes::Bytes;
use futures::{StreamExt as _, stream};
use kbf_front::large_key;
use kbf_meta::{ActionRecord, ActionWriteError};
use kbf_proto::reapi::{ActionResult, OutputFile};
use kbf_storage::{OpenPieces, StorageError};
use kbf_types::Digest;
use prost::Message as _;

use crate::bytestream::{pieces, write};
use crate::rig::{Rig, blob_len, blob_of};

fn proto(digest: &Digest) -> kbf_proto::reapi::Digest {
    kbf_proto::reapi::Digest {
        hash: digest.hash_hex(),
        size_bytes: digest.size_bytes as i64,
    }
}

/// A small blob and a large one, both uploaded by a client.
async fn uploaded(rig: &Rig) -> ((Digest, Bytes), (Digest, Bytes)) {
    let small = blob_len(100, 1);
    let large = blob_len(5000, 2);
    for (digest, bytes) in [&small, &large] {
        write(rig, pieces(digest, bytes, 700))
            .await
            .expect("uploaded");
    }
    (small, large)
}

/// Asserts that FindMissing answers from the index alone, small and large blobs alike, in the
/// order asked, and fails Unavailable (not "everything is missing") when the index cannot answer.
#[tokio::test]
async fn find_missing_asks_the_index_alone() {
    let rig = Rig::new().await;
    let ((small, _), (large, _)) = uploaded(&rig).await;
    let (never, _) = blob_of(b"never");

    let lacking = rig.backend.find_missing(&[never, large, small]).await;
    rig.index.fail_find(1);
    let down = rig.backend.find_missing(&[small]).await;

    assert_eq!(lacking.expect("answered"), vec![never]);
    assert!(matches!(down, Err(StorageError::Unavailable(_))));
}

/// Asserts that a blob within the packs' bound is read whole and verified, a blob that is not
/// held is NotFound, a held blob over the bound is Invalid (it is read as a stream, never
/// buffered whole), and an index that cannot answer is Unavailable for both.
#[tokio::test]
async fn read_blob_is_whole_for_small_blobs_and_refused_for_large_ones() {
    let rig = Rig::new().await;
    let ((small, small_bytes), (large, _)) = uploaded(&rig).await;
    let (never_small, _) = blob_len(100, 9);
    let (never_large, _) = blob_len(5000, 9);

    let read = rig.backend.read_blob(&small).await;
    let absent = rig.backend.read_blob(&never_small).await;
    let too_big = rig.backend.read_blob(&large).await;
    let large_absent = rig.backend.read_blob(&never_large).await;
    rig.index.fail_locate(1);
    let large_down = rig.backend.read_blob(&large).await;
    rig.index.fail_locate(1);
    let small_down = rig.backend.read_blob(&small).await;

    assert_eq!(read.expect("read"), small_bytes);
    assert!(matches!(absent, Err(StorageError::NotFound(d)) if d == never_small));
    let refused = too_big.expect_err("too large to read whole");
    assert!(matches!(refused, StorageError::Invalid(_)), "{refused:?}");
    assert!(refused.to_string().contains("as a stream"), "{refused}");
    assert!(matches!(large_absent, Err(StorageError::NotFound(d)) if d == never_large));
    assert!(matches!(large_down, Err(StorageError::Unavailable(_))));
    assert!(matches!(small_down, Err(StorageError::Unavailable(_))));
}

/// Asserts that a stream of a blob, small or large, is the blob; a blob that is not held is
/// NotFound; and a failure of the object store after the stream began is an error item.
#[tokio::test]
async fn read_stream_is_the_blob_whole() {
    let rig = Rig::new().await;
    let ((small, small_bytes), (large, large_bytes)) = uploaded(&rig).await;
    let (never, _) = blob_len(5000, 9);
    let collect = |digest: Digest| {
        let backend = rig.backend.clone();
        async move {
            let mut windows = backend.read_stream(&digest).await?;
            let mut out = Vec::new();
            while let Some(window) = windows.next().await {
                out.extend_from_slice(&window?);
            }
            Ok::<_, StorageError>(out)
        }
    };

    let small_read = collect(small).await;
    let large_read = collect(large).await;
    let absent = collect(never).await;
    rig.store.reject_reads(1);
    let failed = collect(large).await;

    assert_eq!(small_read.expect("read"), small_bytes.to_vec());
    assert_eq!(large_read.expect("read"), large_bytes.to_vec());
    assert!(matches!(absent, Err(StorageError::NotFound(d)) if d == never));
    assert!(matches!(failed, Err(StorageError::Unavailable(_))));
}

/// Asserts that ReadMany answers each digest alone, in order: the packed blobs together, and a
/// blob that is held nowhere as NotFound (there is no node to ask for the rest).
#[tokio::test]
async fn read_many_answers_each_blob_alone() {
    let rig = Rig::new().await;
    let ((small, small_bytes), _) = uploaded(&rig).await;
    let (never, _) = blob_len(100, 9);

    let read = rig.backend.read_many(&[never, small]).await;

    assert!(matches!(&read[0], Err(StorageError::NotFound(d)) if *d == never));
    assert_eq!(read[1].as_ref().expect("read"), &small_bytes);
}

/// Asserts that the pack index answers where a small blob lies and none for a large one (never
/// packed), and that the object store answers the length of the large blob's object and of no
/// other key.
#[tokio::test]
async fn the_locate_checks_see_packs_and_large_objects() {
    let rig = Rig::new().await;
    let ((small, _), (large, _)) = uploaded(&rig).await;

    let located = rig
        .backend
        .locate_packed(&[small, large])
        .await
        .expect("located");
    let large_len = rig.backend.object_len(&large_key(&large)).await;
    let none_len = rig.backend.object_len("cas/l/nothing").await;

    assert!(located[0].is_some());
    assert!(located[1].is_none(), "a large blob is not in a pack");
    assert_eq!(large_len.expect("asked"), Some(5000));
    assert_eq!(none_len.expect("asked"), None);
}

/// A result whose one output file is `output`.
fn names(output: &Digest) -> ActionResult {
    ActionResult {
        output_files: vec![OutputFile {
            path: "out".to_owned(),
            digest: Some(proto(output)),
            ..OutputFile::default()
        }],
        ..ActionResult::default()
    }
}

/// Asserts that the action-cache write is one call: a result whose blobs are held is stored and
/// found, one with a blob that is not held is refused naming it, and the prepared record of a
/// result names the digest of its encoding and its closure but cannot be committed (the result
/// is not a blob here), with the reason in the error.
#[tokio::test]
async fn the_action_cache_is_written_whole_and_a_prepared_record_is_not_committed() {
    let rig = Rig::new().await;
    let ((small, _), (large, _)) = uploaded(&rig).await;
    let action = blob_of(b"an action").0;
    let (absent, _) = blob_of(b"absent");
    let result = ActionResult {
        output_files: vec![names(&small).output_files.remove(0), {
            let mut file = names(&large).output_files.remove(0);
            file.path = "big".to_owned();
            file
        }],
        ..ActionResult::default()
    };

    let written = rig.backend.put_action_result(action, &result).await;
    let refused = rig.backend.put_action_result(action, &names(&absent)).await;
    let prepared = rig
        .backend
        .prepare_action_result(&result)
        .await
        .expect("prepared");
    let committed = rig
        .backend
        .commit_action_record(
            action,
            ActionRecord {
                result: prepared.result,
                closure: prepared.closure.clone(),
            },
        )
        .await;

    written.expect("written");
    assert!(matches!(
        refused,
        Err(StorageError::ActionWrite(ActionWriteError::Absent(ref named))) if *named == vec![absent]
    ));
    assert_eq!(
        prepared.result,
        kbf_segments::sha256(&result.encode_to_vec())
    );
    let closure: Vec<Digest> = prepared.closure.iter().copied().collect();
    assert_eq!(closure.len(), 2);
    assert!(closure.contains(&small) && closure.contains(&large));
    let refused = committed.expect_err("nothing to commit");
    assert!(matches!(refused, StorageError::Invalid(_)), "{refused:?}");
    assert!(
        refused.to_string().contains("put_action_result"),
        "{refused}"
    );
    let missing = rig.backend.prepare_action_result(&names(&absent)).await;
    assert!(matches!(missing, Err(StorageError::ActionWrite(_))));
}

/// The pieces of `bytes` as `open` gives them each time it is called.
fn open_pieces(bytes: &Bytes) -> impl Fn() -> kbf_storage::PieceStream + Send + Sync + use<> {
    let chunks: Vec<Bytes> = bytes.chunks(700).map(Bytes::copy_from_slice).collect();
    move || stream::iter(chunks.clone().into_iter().map(io::Result::Ok)).boxed()
}

/// Asserts that a large blob streamed in by Execute's writer (an output of a job) is stored as
/// its object and recorded, so it is held, read and in the closure of a result; that bytes that
/// are not the digest are Invalid and store nothing; and that a piece that fails, and an object
/// store that refuses the put, are Unavailable.
#[tokio::test]
async fn put_streamed_stores_a_large_blob_as_its_object() {
    let rig = Rig::new().await;
    let (digest, bytes) = blob_len(5000, 3);
    let (wrong, _) = blob_len(5000, 4);
    let (_, other_bytes) = blob_len(5000, 5);
    let (broken, _) = blob_len(5000, 6);
    let (refused, refused_bytes) = blob_len(5000, 7);

    let open = open_pieces(&bytes);
    let stored = rig.backend.put_streamed(&digest, &open).await;
    let open: &OpenPieces<'_> = &open_pieces(&other_bytes);
    let mismatch = rig.backend.put_streamed(&wrong, open).await;
    let failing = || {
        let first = Bytes::from_static(b"some");
        stream::iter([
            Ok(first),
            Err(io::Error::other("the job's object went away")),
        ])
        .boxed()
    };
    let failed = rig.backend.put_streamed(&broken, &failing).await;
    rig.store.reject_puts(1);
    let open = open_pieces(&refused_bytes);
    let store_down = rig.backend.put_streamed(&refused, &open).await;

    stored.expect("stored");
    assert_eq!(rig.store.held(&large_key(&digest)).await, Some(bytes));
    let missing = rig
        .backend
        .find_missing(&[digest, wrong, broken, refused])
        .await;
    assert_eq!(missing.expect("asked"), vec![wrong, broken, refused]);
    assert!(
        matches!(mismatch, Err(StorageError::Invalid(_))),
        "{mismatch:?}"
    );
    assert!(
        matches!(failed, Err(StorageError::Unavailable(_))),
        "{failed:?}"
    );
    assert!(
        matches!(store_down, Err(StorageError::Unavailable(_))),
        "{store_down:?}"
    );
    assert_eq!(rig.store.held(&large_key(&wrong)).await, None);
}

/// Asserts that the store reports no leadership (it has no metadata log), and renders its own
/// metrics, not a cache's.
#[tokio::test]
async fn the_store_has_no_leadership_and_renders_its_metrics() {
    let rig = Rig::new().await;

    assert!(rig.backend.leadership().is_none());
    let rendered = rig.backend.render_metrics();
    assert!(rendered.contains("kbf_bytes_in_flight"), "{rendered}");
    assert_eq!(format!("{:?}", rig.backend), "Backend(..)");
    assert!(rig.backend.metrics().render().contains("kbf_"));
}

/// Asserts that a large blob already held is not read or put again by the writer of Execute (the
/// pieces are not opened), and that a stream of more bytes than the digest names is Invalid and
/// stores nothing.
#[tokio::test]
async fn put_streamed_skips_a_held_blob_and_refuses_too_many_bytes() {
    let rig = Rig::new().await;
    let (digest, bytes) = blob_len(5000, 8);
    let (long, _) = blob_len(5000, 9);
    let (_, too_long) = blob_len(6000, 9);
    let open = open_pieces(&bytes);
    rig.backend
        .put_streamed(&digest, &open)
        .await
        .expect("stored");
    let unopened = || -> kbf_storage::PieceStream { panic!("a held blob is not read") };

    let again = rig.backend.put_streamed(&digest, &unopened).await;
    let puts_after_again = rig.store.stream_puts();
    let open = open_pieces(&too_long);
    let over = rig.backend.put_streamed(&long, &open).await;

    again.expect("held already");
    assert_eq!(puts_after_again, 1, "the held blob was not put again");
    let refused = over.expect_err("more than its digest names");
    assert!(matches!(refused, StorageError::Invalid(_)), "{refused:?}");
    assert!(
        refused.to_string().contains("more than the 5000 bytes"),
        "{refused}"
    );
    assert_eq!(rig.store.held(&large_key(&long)).await, None);
}

/// Asserts that the store can be printed without printing what it holds.
#[tokio::test]
async fn the_store_prints_without_its_contents() {
    let rig = Rig::new().await;

    assert_eq!(format!("{:?}", rig.stored), "Stored { .. }");
}
