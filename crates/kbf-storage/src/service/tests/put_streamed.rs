//! `RemoteStorage::put_streamed`: a blob sent to a node as ByteStream messages taken from a
//! stream of pieces, never gathered, under the digest the caller names before the first byte.

use std::io;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use bytes::Bytes;
use futures::future::BoxFuture;
use futures::stream::{self, BoxStream};
use futures::{FutureExt as _, StreamExt as _};
use kbf_types::Digest;

use super::fake::{Mem, patterned, serve, sha};
use super::{RemoteStorage, STREAM_FROM_BYTES};
use crate::{Storage as _, StorageError};

/// More than the node buffers whole, so the node itself streams the blob.
const LARGE: usize = STREAM_FROM_BYTES as usize + (3 << 20);
const PIECE: usize = 5 << 20;

fn pieces(bytes: &Bytes, piece: usize) -> BoxStream<'static, io::Result<Bytes>> {
    let all: Vec<io::Result<Bytes>> = (0..bytes.len())
        .step_by(piece)
        .map(|at| Ok(bytes.slice(at..bytes.len().min(at + piece))))
        .collect();
    stream::iter(all).boxed()
}

fn named(digest: Digest) -> BoxFuture<'static, Option<Digest>> {
    futures::future::ready(Some(digest)).boxed()
}

async fn remote() -> (Arc<Mem>, RemoteStorage) {
    let mem = Arc::new(Mem::default());
    let channel = serve(&mem, 64 << 20).await;
    (mem, RemoteStorage::new(channel, 0))
}

/// Asserts that a blob larger than the node buffers goes through in pieces of any size (here 5 MiB,
/// which the client cuts into messages of at most 1 MiB), is stored under its digest with every byte
/// in place, and that the answer is that digest. Catches a client that loses, repeats or reorders
/// a piece, or sends the blob under another name.
#[tokio::test]
async fn a_large_blob_is_stored_from_pieces_under_its_digest() {
    let (mem, remote) = remote().await;
    let blob = Bytes::from(patterned(LARGE));
    let digest = sha(&blob);

    let stored = remote
        .put_streamed(blob.len() as u64, pieces(&blob, PIECE), named(digest))
        .await
        .expect("stored");

    assert_eq!(stored, Some(digest));
    assert_eq!(mem.blobs.lock().expect("lock").get(&digest), Some(&blob));
    assert_eq!(mem.streamed.load(Ordering::SeqCst), 1);
}

/// Asserts that bytes that are not the digest named are refused and nothing is stored under that
/// digest: the node checks the digest, and the client reports its refusal.
#[tokio::test]
async fn bytes_that_are_not_the_named_digest_are_refused_and_not_stored() {
    let (mem, remote) = remote().await;
    let blob = Bytes::from(patterned(LARGE));
    let digest = sha(&blob);
    let mut wrong = blob.to_vec();
    wrong[LARGE / 2] ^= 1;
    let wrong = Bytes::from(wrong);

    let error = remote
        .put_streamed(wrong.len() as u64, pieces(&wrong, PIECE), named(digest))
        .await
        .expect_err("refused");

    assert!(matches!(error, StorageError::Invalid(_)), "{error:?}");
    assert!(mem.blobs.lock().expect("lock").is_empty());
}

/// Asserts that a body that fails part way ends the write without `finish_write`, so the node
/// stores nothing, and the caller gets an error that is not the node's refusal of a digest.
#[tokio::test]
async fn a_body_that_fails_stores_nothing() {
    let (mem, remote) = remote().await;
    let blob = Bytes::from(patterned(LARGE));
    let digest = sha(&blob);
    let half = blob.slice(..LARGE / 2);
    let body = stream::iter(vec![
        Ok(half),
        Err(io::Error::new(io::ErrorKind::TimedOut, "the object store")),
    ])
    .boxed();

    let error = remote
        .put_streamed(blob.len() as u64, body, named(digest))
        .await
        .expect_err("failed");

    assert!(matches!(error, StorageError::Unavailable(_)), "{error:?}");
    assert!(mem.blobs.lock().expect("lock").is_empty());
}

/// Asserts that the digest must be known before the first byte (ByteStream names it in the resource
/// name): a claim that is not ready is refused without sending, and a claim withdrawn (`None`)
/// answers `None`, sends nothing and stores nothing.
#[tokio::test]
async fn the_digest_is_taken_before_the_first_byte() {
    let (mem, remote) = remote().await;
    let blob = Bytes::from(patterned(LARGE));
    let size = blob.len() as u64;

    let late = futures::future::pending::<Option<Digest>>().boxed();
    let error = remote
        .put_streamed(size, pieces(&blob, PIECE), late)
        .await
        .expect_err("not ready");
    assert!(matches!(error, StorageError::Invalid(_)), "{error:?}");

    let withdrawn = futures::future::ready(None).boxed();
    let answer = remote
        .put_streamed(size, pieces(&blob, PIECE), withdrawn)
        .await
        .expect("withdrawn is not an error");
    assert_eq!(answer, None);
    assert!(mem.blobs.lock().expect("lock").is_empty());
}

/// Asserts that a digest whose size is not the size said is refused before anything is sent.
#[tokio::test]
async fn a_size_that_is_not_the_digests_is_refused() {
    let (mem, remote) = remote().await;
    let blob = Bytes::from(patterned(LARGE));
    let digest = sha(&blob);

    let error = remote
        .put_streamed(blob.len() as u64 + 1, pieces(&blob, PIECE), named(digest))
        .await
        .expect_err("refused");

    assert!(matches!(error, StorageError::Invalid(_)), "{error:?}");
    assert!(mem.blobs.lock().expect("lock").is_empty());
}

/// Asserts that an empty blob is refused: it has no bytes to stream and is `Storage::put`'s, and
/// nothing is sent.
#[tokio::test]
async fn an_empty_blob_is_not_streamed() {
    let (mem, remote) = remote().await;

    let error = remote
        .put_streamed(0, stream::empty().boxed(), named(sha(b"")))
        .await
        .expect_err("refused");

    assert!(matches!(error, StorageError::Invalid(_)), "{error:?}");
    assert!(mem.blobs.lock().expect("lock").is_empty());
}
