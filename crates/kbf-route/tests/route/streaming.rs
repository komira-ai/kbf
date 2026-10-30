//! A blob streamed to its write owner (`Router::put_streamed`): taken from a source a piece at a
//! time, sent whole to the primary only, sent again from its first byte when an owner cannot be
//! reached or the map is stale, and never believed to be stored on a node's say-so alone.

use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use bytes::Bytes;
use futures::StreamExt as _;
use futures::stream::{self, BoxStream};
use kbf_map::MemberId;
use kbf_storage::StorageError;

use super::support::{
    Cluster, active, blob_where, blob_written_to, map, router, sha, unavailable, view,
};

/// A source of `blob` in pieces of 5 bytes, and how many times it was opened.
fn source(
    blob: &[u8],
) -> (
    impl Fn() -> BoxStream<'static, io::Result<Bytes>> + use<>,
    Arc<AtomicUsize>,
) {
    let blob = Bytes::copy_from_slice(blob);
    let opened = Arc::new(AtomicUsize::new(0));
    let count = Arc::clone(&opened);
    let open = move || {
        count.fetch_add(1, Ordering::SeqCst);
        let pieces: Vec<io::Result<Bytes>> = (0..blob.len())
            .step_by(5)
            .map(|at| Ok(blob.slice(at..blob.len().min(at + 5))))
            .collect();
        stream::iter(pieces).boxed()
    };
    (open, opened)
}

/// Asserts that a streamed write goes to the primary only, once, as the blob's bytes in order, and
/// that the primary holds the whole blob afterwards: nothing goes to the replica or to a member
/// outside the owners.
#[tokio::test]
async fn a_streamed_write_goes_to_the_primary_only_and_is_stored_whole() {
    let map = map(1, 2, active(&[1, 2, 3]));
    let cluster = Cluster::of(3);
    let (router, _) = router(&map, &cluster);
    let (blob, digest) = blob_written_to(&map, &[2, 3]);
    let (open, opened) = source(&blob);

    router.put_streamed(&digest, &open).await.expect("stored");

    assert_eq!(cluster.node(2).put_log(), [digest]);
    assert_eq!(cluster.node(2).blob(&digest), Some(Bytes::from(blob)));
    assert!(cluster.node(1).put_log().is_empty());
    assert!(cluster.node(3).put_log().is_empty());
    assert_eq!(cluster.dialled(), [(2, 1)]);
    assert_eq!(opened.load(Ordering::SeqCst), 1);
}

/// Asserts that an owner that cannot be reached is followed by the next owner, which is sent the
/// blob from its first byte: the source is opened for each attempt, and the blob is whole at the
/// second owner. Catches a retry that sends the rest of a spent stream.
#[tokio::test]
async fn a_streamed_write_goes_on_to_the_next_owner_from_the_first_byte() {
    let map = map(1, 2, active(&[1, 2, 3]));
    let cluster = Cluster::of(3);
    let (router, _) = router(&map, &cluster);
    let (blob, digest) = blob_written_to(&map, &[2, 3]);
    cluster.node(2).fail_next_put(unavailable());
    let (open, opened) = source(&blob);

    router.put_streamed(&digest, &open).await.expect("stored");

    assert!(cluster.node(2).put_log().is_empty());
    assert_eq!(cluster.node(3).blob(&digest), Some(Bytes::from(blob)));
    assert_eq!(cluster.dialled(), [(2, 1), (3, 1)]);
    assert_eq!(opened.load(Ordering::SeqCst), 2);
}

/// Asserts that an answer from a node that was reached is the answer: a refusal is returned as
/// the node gave it, the next owner is not dialled and the source is opened once.
#[tokio::test]
async fn a_streamed_write_an_owner_refused_is_not_tried_at_the_next() {
    let map = map(1, 2, active(&[1, 2, 3]));
    let cluster = Cluster::of(3);
    let (router, _) = router(&map, &cluster);
    let (blob, digest) = blob_written_to(&map, &[2, 3]);
    cluster
        .node(2)
        .fail_next_put(StorageError::Invalid("a blob over the largest".to_owned()));
    let (open, opened) = source(&blob);

    let error = router
        .put_streamed(&digest, &open)
        .await
        .expect_err("refused");

    assert!(matches!(error, StorageError::Invalid(_)), "{error:?}");
    assert!(cluster.node(3).put_log().is_empty());
    assert_eq!(cluster.dialled(), [(2, 1)]);
    assert_eq!(opened.load(Ordering::SeqCst), 1);
}

/// Asserts that bytes that are not the digest are refused by the owner and not stored anywhere:
/// the router names the digest it was given, not the bytes' own.
#[tokio::test]
async fn bytes_that_are_not_the_digest_are_not_stored() {
    let map = map(1, 2, active(&[1, 2, 3]));
    let cluster = Cluster::of(3);
    let (router, _) = router(&map, &cluster);
    let (blob, digest) = blob_written_to(&map, &[2, 3]);
    let mut wrong = blob.clone();
    wrong[0] ^= 1;
    let (open, _) = source(&wrong);

    let error = router
        .put_streamed(&digest, &open)
        .await
        .expect_err("refused");

    assert!(
        matches!(error, StorageError::DigestMismatch { .. }),
        "{error:?}"
    );
    assert!(cluster.node(2).put_log().is_empty());
    assert_eq!(cluster.node(2).blob(&digest), None);
}

/// Asserts that a node that answers without having stored the blob (the digest of another blob,
/// or none) is an internal error, not a success.
#[tokio::test]
async fn a_node_that_stored_another_blob_is_an_internal_error() {
    let map = map(1, 1, active(&[1]));
    let cluster = Cluster::of(1);
    let (router, _) = router(&map, &cluster);
    let (blob, digest) = blob_where(|_| true);
    cluster.node(1).answer_puts_with(sha(b"other"));
    let (open, _) = source(&blob);

    let error = router
        .put_streamed(&digest, &open)
        .await
        .expect_err("wrong answer");

    assert!(matches!(error, StorageError::Internal(_)), "{error:?}");
}

/// Asserts that a stale answer fetches the newer map and sends the blob again from its first
/// byte to the new owner: the source is opened for each attempt.
#[tokio::test]
async fn a_stale_answer_sends_the_blob_again_to_the_new_owner() {
    let one = map(1, 1, active(&[1]));
    let two = map(2, 1, active(&[2]));
    let cluster = Cluster::of(2);
    let (router, cell) = router(&one, &cluster);
    cell.answer_fetch(Ok(view(&two)));
    let (blob, digest) = blob_where(|d| {
        one.write_owners(d) == [MemberId(1)] && two.write_owners(d) == [MemberId(2)]
    });
    cluster
        .node(1)
        .fail_next_put(StorageError::StaleMap { current: 2 });
    let (open, opened) = source(&blob);

    router.put_streamed(&digest, &open).await.expect("stored");

    assert_eq!(cell.fetched(), [2]);
    assert_eq!(cluster.node(2).blob(&digest), Some(Bytes::from(blob)));
    assert!(cluster.node(1).put_log().is_empty());
    assert_eq!(opened.load(Ordering::SeqCst), 2);
}
