//! The stream that reads a blob out of the store: chunks, ranges, the digest check at the end,
//! and a store that fails or lies.

use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use http_body_util::BodyExt as _;
use sha2::{Digest as _, Sha256};

use super::OciMetrics;
use super::body::{BlobRead, OciBody};
use super::names::Digest;
use crate::{Backend, ByteRange, Error, FaultyBackend, Listed, MemoryBackend, Meta, Op};

const KEY: &str = "oci/blobs/sha256/x";

fn digest_of(bytes: &[u8]) -> Digest {
    Digest::of_hex(hex::encode(Sha256::digest(bytes)))
}

/// A store that gives one byte more (or one fewer) than a read asks for.
struct Lying {
    inner: MemoryBackend,
    longer: bool,
}

#[async_trait]
impl Backend for Lying {
    async fn put(&self, key: &str, body: Bytes) -> Result<(), Error> {
        self.inner.put(key, body).await
    }
    async fn put_if_match(&self, key: &str, body: Bytes, etag: &str) -> Result<String, Error> {
        self.inner.put_if_match(key, body, etag).await
    }
    async fn get(&self, key: &str, range: Option<ByteRange>) -> Result<Bytes, Error> {
        let got = self.inner.get(key, range).await?;
        let mut bytes = got.to_vec();
        if self.longer {
            bytes.push(b'!');
        } else {
            bytes.pop();
        }
        Ok(Bytes::from(bytes))
    }
    async fn head(&self, key: &str) -> Result<Meta, Error> {
        self.inner.head(key).await
    }
    async fn delete(&self, key: &str) -> Result<(), Error> {
        self.inner.delete(key).await
    }
    async fn list(&self, prefix: &str) -> Result<Vec<Listed>, Error> {
        self.inner.list(prefix).await
    }
}

async fn stored(body: &'static [u8]) -> Arc<MemoryBackend> {
    let backend = Arc::new(MemoryBackend::new());
    backend
        .put(KEY, Bytes::from_static(body))
        .await
        .expect("put");
    backend
}

fn read(
    backend: Arc<dyn Backend>,
    range: (u64, u64),
    verify: Option<Digest>,
    chunk: u64,
) -> (OciBody, Arc<OciMetrics>) {
    let metrics = Arc::new(OciMetrics::default());
    let read = BlobRead {
        backend,
        key: KEY.to_owned(),
        start: range.0,
        end: range.1,
        verify,
        metrics: Arc::clone(&metrics),
        chunk,
    };
    (read.body(), metrics)
}

/// The data frames of a body, the sizes of them, and the error it ended in, if it did.
async fn drain(mut body: OciBody) -> (Vec<Vec<u8>>, Option<String>) {
    let mut frames = Vec::new();
    loop {
        match body.frame().await {
            None => return (frames, None),
            Some(Err(error)) => {
                // The body is over once it has failed.
                assert!(body.frame().await.is_none());
                return (frames, Some(error.to_string()));
            }
            Some(Ok(frame)) => frames.push(frame.into_data().expect("data").to_vec()),
        }
    }
}

/// Asserts that a whole object is read in chunks of the size asked, that the bytes come out in
/// order, that they are counted, and that the digest check at the end passes.
#[tokio::test]
async fn a_blob_is_read_in_chunks_and_checked_at_the_end() {
    let backend = stored(b"0123456789").await;
    let digest = digest_of(b"0123456789");
    let (body, metrics) = read(backend, (0, 10), Some(digest), 4);
    let (frames, error) = drain(body).await;
    assert_eq!(frames, [b"0123".to_vec(), b"4567".to_vec(), b"89".to_vec()]);
    assert_eq!(error, None);
    assert_eq!((metrics.bytes_sent(), metrics.mismatches()), (10, 0));
}

/// Asserts that bytes that are not the digest are never delivered in full: the last chunk is
/// held back and the body fails in its place (so a response that has declared its length is
/// short), the mismatch is counted, and the error names the digest the bytes do have. A blob
/// that is one chunk delivers nothing.
#[tokio::test]
async fn bytes_that_are_not_the_digest_end_the_body_before_the_last_chunk() {
    let backend = stored(b"0123456789").await;
    let wrong = digest_of(b"something else");
    let (body, metrics) = read(backend.clone(), (0, 10), Some(wrong.clone()), 4);
    let (frames, error) = drain(body).await;
    assert_eq!(frames, [b"0123".to_vec(), b"4567".to_vec()]);
    let error = error.expect("a failed body");
    assert!(error.contains(KEY), "{error}");
    assert!(error.contains(digest_of(b"0123456789").hex()), "{error}");
    assert_eq!((metrics.bytes_sent(), metrics.mismatches()), (8, 1));
    let (body, metrics) = read(backend, (0, 10), Some(wrong), 10);
    let (frames, error) = drain(body).await;
    assert!(frames.is_empty());
    assert!(error.is_some());
    assert_eq!((metrics.bytes_sent(), metrics.mismatches()), (0, 1));
}

/// Asserts that a range is read from where it starts to where it ends, with no digest check
/// (a part of an object has no digest of its own), and counted by what was sent.
#[tokio::test]
async fn a_range_is_read_without_a_digest_check() {
    let backend = stored(b"0123456789").await;
    let (body, metrics) = read(backend, (3, 8), None, 4);
    let (frames, error) = drain(body).await;
    assert_eq!(frames, [b"3456".to_vec(), b"7".to_vec()]);
    assert_eq!(error, None);
    assert_eq!((metrics.bytes_sent(), metrics.mismatches()), (5, 0));
}

/// Asserts that an empty object has an empty body that passes the check against the digest of
/// nothing, and fails against any other.
#[tokio::test]
async fn an_empty_blob_is_checked_too() {
    let backend = stored(b"").await;
    let (body, _) = read(backend.clone(), (0, 0), Some(digest_of(b"")), 4);
    assert_eq!(drain(body).await, (Vec::new(), None));
    let (body, metrics) = read(backend, (0, 0), Some(digest_of(b"x")), 4);
    let (frames, error) = drain(body).await;
    assert!(frames.is_empty());
    assert!(error.is_some());
    assert_eq!(metrics.mismatches(), 1);
}

/// Asserts that a store that gives fewer or more bytes than it was asked for ends the body in
/// an error before the digest is looked at, and that no byte of that read is sent.
#[tokio::test]
async fn a_store_that_gives_the_wrong_number_of_bytes_ends_the_body() {
    for longer in [false, true] {
        let inner = MemoryBackend::new();
        inner
            .put(KEY, Bytes::from_static(b"0123456789"))
            .await
            .expect("put");
        let backend = Arc::new(Lying { inner, longer });
        let (body, metrics) = read(backend, (0, 10), Some(digest_of(b"0123456789")), 4);
        let (frames, error) = drain(body).await;
        assert!(frames.is_empty(), "longer={longer}");
        let error = error.expect("a failed body");
        assert!(error.contains("the store gave"), "{error}");
        assert_eq!((metrics.bytes_sent(), metrics.mismatches()), (0, 0));
    }
}

/// Asserts that a store that fails a read ends the body in that error, with the error
/// of the store in it.
#[tokio::test]
async fn a_store_that_fails_a_read_ends_the_body() {
    let memory = stored(b"0123456789").await;
    let faulty = Arc::new(FaultyBackend::new(memory));
    let (body, metrics) = read(faulty.clone(), (0, 10), Some(digest_of(b"0123456789")), 4);
    faulty.fail_next_with(Op::Get, Error::Unavailable("down".to_owned()));
    let (frames, error) = drain(body).await;
    assert!(frames.is_empty());
    assert!(error.expect("a failed body").contains("down"));
    assert_eq!(metrics.bytes_sent(), 0);
}
