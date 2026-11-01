//! The job-facing read of a blob over the whole-blob bound: it is streamed from the source,
//! hashed as it passes, and never held whole.
//!
//! The bounds are scaled down so that a test blob is kilobytes: `stream_over` is 1 KiB and the
//! cap 16 KiB, and the source hands a blob over in chunks of 1 KiB, counting the chunks the
//! listener has pulled. The code that runs is the one a 70 MiB blob runs.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use bytes::Bytes;
use futures::future::BoxFuture;
use futures::{StreamExt as _, stream};
use kbf_front::grants::{GrantKey, Grants, MIN_SECRET_LEN};
use kbf_front::job_read::{
    BlobSource, BlobStream, DEFAULT_MAX_BLOB_BYTES, JobRead, Limits, Reply, STREAM_OVER_BYTES,
    StreamError,
};
use kbf_front::{Backend, Cache, VerifiedBlob, digest_to_proto};
use kbf_proto::reapi::{Action, Directory, FileNode};
use kbf_storage::{ReadStream, StorageError};
use kbf_types::{Digest, DigestFunction};
use prost::Message as _;
use tonic::codegen::http::{Method, StatusCode};

const BUILD: &str = "build-0009";
const NOT_AFTER: u64 = 1_000;
const CHUNK: usize = 1024;
const STREAM_OVER: u64 = 1024;
const CAP: u64 = 16 * 1024;

fn limits() -> Limits {
    Limits {
        stream_over: STREAM_OVER,
        max_blob_bytes: CAP,
        ..Limits::default()
    }
}

/// What the source does to the bytes of a streamed blob.
#[derive(Clone, Copy)]
enum Fault {
    None,
    /// Changes the byte at this offset.
    Flip(usize),
    /// Ends after this many chunks.
    Truncate(usize),
    /// Sends one chunk more than the blob has.
    Extend,
    /// Fails in place of this chunk.
    FailAt(usize),
    /// Sends an empty chunk after each chunk.
    EmptyChunks,
    /// Fails in place of this chunk with the store's own report that the bytes are not the
    /// blob's: `Corrupt` when `corrupt`, else `DigestMismatch`.
    Reported {
        at: usize,
        corrupt: bool,
    },
}

/// A source of blobs that streams in chunks of [`CHUNK`] bytes, logs what is asked of it and
/// counts the chunks pulled.
struct Source {
    blobs: HashMap<Digest, Bytes>,
    whole: Mutex<Vec<Digest>>,
    opened: Mutex<Vec<Digest>>,
    pulled: Arc<AtomicUsize>,
    fault: Mutex<Fault>,
    open_error: Mutex<Option<StorageError>>,
}

impl Source {
    fn count(log: &Mutex<Vec<Digest>>, digest: &Digest) -> usize {
        let log = log.lock().unwrap_or_else(PoisonError::into_inner);
        log.iter().filter(|d| *d == digest).count()
    }

    fn whole_reads_of(&self, digest: &Digest) -> usize {
        Self::count(&self.whole, digest)
    }

    fn opens_of(&self, digest: &Digest) -> usize {
        Self::count(&self.opened, digest)
    }

    fn pulled(&self) -> usize {
        self.pulled.load(Ordering::SeqCst)
    }

    fn set_fault(&self, fault: Fault) {
        *self.fault.lock().unwrap_or_else(PoisonError::into_inner) = fault;
    }
}

impl BlobSource for Source {
    fn read_blob<'a>(&'a self, digest: &'a Digest) -> BoxFuture<'a, Result<Bytes, StorageError>> {
        Box::pin(async move {
            self.whole
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(*digest);
            self.blobs
                .get(digest)
                .cloned()
                .ok_or(StorageError::NotFound(*digest))
        })
    }

    fn read_stream<'a>(
        &'a self,
        digest: &'a Digest,
    ) -> BoxFuture<'a, Result<ReadStream, StorageError>> {
        Box::pin(async move {
            self.opened
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(*digest);
            if let Some(error) = self
                .open_error
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .take()
            {
                return Err(error);
            }
            let blob = self
                .blobs
                .get(digest)
                .ok_or(StorageError::NotFound(*digest))?;
            let fault = *self.fault.lock().unwrap_or_else(PoisonError::into_inner);
            let mut bytes = blob.to_vec();
            if let Fault::Flip(at) = fault {
                bytes[at] ^= 0xff;
            }
            let mut items: Vec<Result<Bytes, StorageError>> = bytes
                .chunks(CHUNK)
                .map(|chunk| Ok(Bytes::copy_from_slice(chunk)))
                .collect();
            match fault {
                Fault::Truncate(n) => items.truncate(n),
                Fault::Extend => items.push(Ok(Bytes::from_static(b"extra"))),
                Fault::FailAt(n) => {
                    items.truncate(n);
                    items.push(Err(StorageError::Unreachable(*digest)));
                }
                Fault::Reported { at, corrupt } => {
                    items.truncate(at);
                    let actual = kbf_segments::sha256(b"other");
                    items.push(Err(if corrupt {
                        StorageError::Corrupt {
                            digest: *digest,
                            actual,
                        }
                    } else {
                        StorageError::DigestMismatch {
                            claimed: *digest,
                            actual,
                        }
                    }));
                }
                Fault::EmptyChunks => {
                    items = items
                        .into_iter()
                        .flat_map(|chunk| [chunk, Ok(Bytes::new())])
                        .collect();
                }
                Fault::None | Fault::Flip(_) => {}
            }
            let pulled = Arc::clone(&self.pulled);
            Ok(stream::iter(items)
                .inspect(move |_| {
                    pulled.fetch_add(1, Ordering::SeqCst);
                })
                .boxed())
        })
    }
}

fn put(blobs: &mut HashMap<Digest, Bytes>, bytes: Vec<u8>) -> Digest {
    let digest = kbf_segments::sha256(&bytes);
    blobs.insert(digest, Bytes::from(bytes));
    digest
}

fn file(name: &str, digest: &Digest) -> FileNode {
    FileNode {
        name: name.to_owned(),
        digest: Some(digest_to_proto(digest)),
        ..FileNode::default()
    }
}

fn path(digest: &Digest) -> String {
    format!("/{digest}")
}

/// An Action whose inputs are a small blob, a 10 KiB blob, one exactly at `STREAM_OVER`, and
/// two no one holds: one exactly at the cap and one a byte over it.
struct World {
    action: Digest,
    small: Digest,
    big: Digest,
    big_bytes: Bytes,
    at_threshold: Digest,
    at_cap: Digest,
    over_cap: Digest,
    blobs: HashMap<Digest, Bytes>,
}

fn world() -> World {
    let mut blobs = HashMap::new();
    let small = put(&mut blobs, b"small".to_vec());
    let pattern = |n: usize| -> Vec<u8> { (0..n).map(|i| (i * 31 % 251) as u8).collect() };
    let big = put(&mut blobs, pattern(10 * CHUNK));
    let big_bytes = blobs[&big].clone();
    let at_threshold = put(&mut blobs, pattern(STREAM_OVER as usize));
    let at_cap = Digest::new(DigestFunction::Sha256, [8; 32], CAP);
    let over_cap = Digest::new(DigestFunction::Sha256, [9; 32], CAP + 1);
    let root = Directory {
        files: vec![
            file("small", &small),
            file("big", &big),
            file("at_threshold", &at_threshold),
            file("at_cap", &at_cap),
            file("over_cap", &over_cap),
        ],
        ..Directory::default()
    };
    let root = put(&mut blobs, root.encode_to_vec());
    let command = put(&mut blobs, b"the command".to_vec());
    let action = Action {
        command_digest: Some(digest_to_proto(&command)),
        input_root_digest: Some(digest_to_proto(&root)),
        ..Action::default()
    };
    let action = put(&mut blobs, action.encode_to_vec());
    World {
        action,
        small,
        big,
        big_bytes,
        at_threshold,
        at_cap,
        over_cap,
        blobs,
    }
}

struct Rig {
    source: Arc<Source>,
    read: JobRead,
    grants: Arc<Grants>,
}

fn grants() -> Arc<Grants> {
    let key = GrantKey::from_secret(&[1; MIN_SECRET_LEN]).expect("a long enough secret");
    let clock = Arc::new(AtomicU64::new(100));
    Arc::new(Grants::new(
        key,
        Arc::new(move || clock.load(Ordering::SeqCst)),
    ))
}

fn rig(blobs: HashMap<Digest, Bytes>, limits: Limits) -> Rig {
    let grants = grants();
    let source = Arc::new(Source {
        blobs,
        whole: Mutex::new(Vec::new()),
        opened: Mutex::new(Vec::new()),
        pulled: Arc::new(AtomicUsize::new(0)),
        fault: Mutex::new(Fault::None),
        open_error: Mutex::new(None),
    });
    let read = JobRead::new(source.clone(), Arc::clone(&grants), limits);
    Rig {
        source,
        read,
        grants,
    }
}

impl Rig {
    fn token(&self, action: Digest) -> String {
        self.grants
            .issue(BUILD, action, NOT_AFTER)
            .expect("a valid build id")
            .expose()
            .to_owned()
    }

    async fn get(&self, digest: &Digest, token: &str) -> Reply {
        let header = format!("Bearer {token}");
        self.read
            .respond(&Method::GET, &path(digest), Some(&header))
            .await
    }

    fn metrics(&self) -> String {
        let mut out = String::new();
        self.read.render_metrics(&mut out);
        out
    }
}

/// Every chunk the stream yields, in order, and the error it ended with if it ended in one.
async fn drain(stream: BlobStream) -> (Vec<Bytes>, Option<StreamError>) {
    let mut chunks = stream.chunks;
    let mut got = Vec::new();
    while let Some(item) = chunks.next().await {
        match item {
            Ok(chunk) => got.push(chunk),
            Err(error) => return (got, Some(error)),
        }
    }
    (got, None)
}

fn joined(chunks: &[Bytes]) -> Vec<u8> {
    chunks.iter().flat_map(|c| c.iter().copied()).collect()
}

fn has_line(out: &str, line: &str) {
    assert!(out.lines().any(|l| l == line), "{line} is not in\n{out}");
}

/// Catches: a blob over the whole-blob bound being read whole instead of streamed (the
/// source's whole read is never asked for it), served with wrong bytes or a wrong length, or
/// re-chunked into one piece; and bytes served not counted.
#[tokio::test]
async fn a_blob_over_the_bound_is_streamed_byte_exact_without_a_whole_read() {
    let w = world();
    let rig = rig(w.blobs.clone(), limits());
    let token = rig.token(w.action);
    let reply = rig.get(&w.big, &token).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert!(reply.body.is_empty(), "the bytes are in the stream");
    let stream = reply.stream.expect("a streamed reply");
    assert_eq!(stream.size, w.big.size_bytes);
    let (chunks, ended) = drain(stream).await;
    assert!(ended.is_none(), "{ended:?}");
    assert_eq!(chunks.len(), 10, "the source's chunks are passed on");
    assert_eq!(joined(&chunks), w.big_bytes);
    assert_eq!(rig.source.whole_reads_of(&w.big), 0);
    assert_eq!(rig.source.opens_of(&w.big), 1);
    let out = rig.metrics();
    has_line(&out, "kbf_job_read_requests_total{outcome=\"ok\"} 1");
    has_line(
        &out,
        &format!("kbf_job_read_bytes_total {}", w.big.size_bytes),
    );
}

/// Catches: a listener that gathers the blob before it answers (memory the size of the blob):
/// the reply is made with no chunk of the body pulled, and the source is never more than one
/// chunk ahead of what the client has taken (one is held back until the digest checks).
#[tokio::test]
async fn the_response_is_produced_incrementally_not_after_the_blob_is_read() {
    let w = world();
    let rig = rig(w.blobs.clone(), limits());
    let token = rig.token(w.action);
    let reply = rig.get(&w.big, &token).await;
    assert_eq!(rig.source.pulled(), 0, "answering reads none of the body");
    let mut chunks = reply.stream.expect("a streamed reply").chunks;
    let mut taken = 0;
    while let Some(item) = chunks.next().await {
        item.expect("a chunk");
        taken += 1;
        assert!(
            rig.source.pulled() <= taken + 1,
            "{} pulled after {taken} taken",
            rig.source.pulled()
        );
    }
    assert_eq!(taken, 10);
    assert_eq!(rig.source.pulled(), 10);
}

/// Catches: a blob whose bytes are not the digest's being served to the end (the client's own
/// hash check would refuse it, but the listener must not hand over a whole wrong blob), a
/// mismatch not counted, and a flip in the last chunk getting through because the last chunk was
/// sent before the digest was known. A source that gives too few bytes or too many is the same.
#[tokio::test]
async fn a_corrupted_large_blob_ends_the_response_short_and_is_counted() {
    let faults = [
        ("a flipped byte in the middle", Fault::Flip(5 * CHUNK + 7)),
        ("a flipped last byte", Fault::Flip(10 * CHUNK - 1)),
        ("a source that ends early", Fault::Truncate(7)),
        ("a source that sends more", Fault::Extend),
    ];
    for (what, fault) in faults {
        let w = world();
        let rig = rig(w.blobs.clone(), limits());
        rig.source.set_fault(fault);
        let token = rig.token(w.action);
        let stream = rig.get(&w.big, &token).await.stream.expect("streamed");
        let (chunks, ended) = drain(stream).await;
        assert!(
            matches!(ended, Some(StreamError::Corrupt)),
            "{what}: {ended:?}"
        );
        let received = joined(&chunks).len() as u64;
        assert!(
            received <= w.big.size_bytes - CHUNK as u64,
            "{what}: {received} of {} bytes were sent",
            w.big.size_bytes
        );
        let out = rig.metrics();
        has_line(
            &out,
            "kbf_job_read_stream_ended_early_total{reason=\"digest_mismatch\"} 1",
        );
        has_line(
            &out,
            "kbf_job_read_stream_ended_early_total{reason=\"store_error\"} 0",
        );
    }
}

/// Catches: a store that says itself that the bytes are not the blob's (a router that found
/// a bad owner, a node that failed its own check) being counted as a store failure, not as a
/// mismatch, or served on.
#[tokio::test]
async fn a_store_that_reports_corruption_is_counted_as_a_mismatch() {
    for corrupt in [true, false] {
        let w = world();
        let rig = rig(w.blobs.clone(), limits());
        rig.source.set_fault(Fault::Reported { at: 4, corrupt });
        let token = rig.token(w.action);
        let stream = rig.get(&w.big, &token).await.stream.expect("streamed");
        let (chunks, ended) = drain(stream).await;
        assert!(matches!(ended, Some(StreamError::Corrupt)), "{ended:?}");
        assert!(joined(&chunks).len() < 10 * CHUNK);
        let out = rig.metrics();
        has_line(
            &out,
            "kbf_job_read_stream_ended_early_total{reason=\"digest_mismatch\"} 1",
        );
        has_line(
            &out,
            "kbf_job_read_stream_ended_early_total{reason=\"store_error\"} 0",
        );
    }
}

/// Catches: an empty chunk from the source (a store may send one) ending the stream or being
/// passed on as a chunk: the blob is served byte-exact and no empty chunk reaches the response.
#[tokio::test]
async fn empty_chunks_from_the_source_are_dropped() {
    let w = world();
    let rig = rig(w.blobs.clone(), limits());
    rig.source.set_fault(Fault::EmptyChunks);
    let token = rig.token(w.action);
    let stream = rig.get(&w.big, &token).await.stream.expect("streamed");
    let (chunks, ended) = drain(stream).await;
    assert!(ended.is_none(), "{ended:?}");
    assert_eq!(joined(&chunks), w.big_bytes);
    assert!(chunks.iter().all(|c| !c.is_empty()));
}

/// Catches: a store that fails part way being served as a complete blob, and counted as a
/// digest mismatch (the two have different causes and different fixes).
#[tokio::test]
async fn a_store_failure_while_streaming_ends_the_response_and_is_counted() {
    let w = world();
    let rig = rig(w.blobs.clone(), limits());
    rig.source.set_fault(Fault::FailAt(4));
    let token = rig.token(w.action);
    let stream = rig.get(&w.big, &token).await.stream.expect("streamed");
    let (chunks, ended) = drain(stream).await;
    assert!(matches!(ended, Some(StreamError::Store(_))), "{ended:?}");
    assert!(joined(&chunks).len() < 10 * CHUNK);
    let out = rig.metrics();
    has_line(
        &out,
        "kbf_job_read_stream_ended_early_total{reason=\"store_error\"} 1",
    );
    has_line(
        &out,
        "kbf_job_read_stream_ended_early_total{reason=\"digest_mismatch\"} 0",
    );
}

/// Catches: a response over the cap being opened (it is 413 before anything is read), a cap
/// that is off by one (a blob of exactly the cap is read, here from a source that lacks it), and
/// a cap that is the old whole-blob bound.
#[tokio::test]
async fn a_blob_over_the_cap_is_too_large_before_anything_is_read() {
    let w = world();
    let rig = rig(w.blobs.clone(), limits());
    let token = rig.token(w.action);
    let reply = rig.get(&w.over_cap, &token).await;
    assert_eq!(reply.status, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(&reply.body[..], b"too_large\n");
    assert!(reply.stream.is_none());
    assert_eq!(rig.source.opens_of(&w.over_cap), 0);
    assert_eq!(rig.source.whole_reads_of(&w.over_cap), 0);
    let reply = rig.get(&w.at_cap, &token).await;
    assert_eq!(
        reply.status,
        StatusCode::NOT_FOUND,
        "the cap itself is read"
    );
    assert_eq!(rig.source.opens_of(&w.at_cap), 1);
}

/// Catches: the default cap not being 512 MiB, and the default streaming bound not being the
/// old 64 MiB whole-blob bound.
#[test]
fn the_default_bounds_are_the_ones_the_design_gives() {
    assert_eq!(DEFAULT_MAX_BLOB_BYTES, 512 << 20);
    assert_eq!(STREAM_OVER_BYTES, 64 << 20);
    let limits = Limits::default();
    assert_eq!(limits.max_blob_bytes, DEFAULT_MAX_BLOB_BYTES);
    assert_eq!(limits.stream_over, STREAM_OVER_BYTES);
    assert_eq!(limits.reads_in_flight, 32);
}

/// Catches: a blob at the bound taking the streaming path (the bound is "over", so a blob of
/// exactly it is served whole as before), and a blob over it taking the whole path.
#[tokio::test]
async fn a_blob_at_the_bound_keeps_the_whole_blob_path() {
    let w = world();
    let rig = rig(w.blobs.clone(), limits());
    let token = rig.token(w.action);
    let reply = rig.get(&w.at_threshold, &token).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert!(reply.stream.is_none());
    assert_eq!(reply.body, w.blobs[&w.at_threshold]);
    assert_eq!(rig.source.opens_of(&w.at_threshold), 0);
    let reply = rig.get(&w.small, &token).await;
    assert_eq!(reply.body, w.blobs[&w.small]);
    assert_eq!(rig.source.opens_of(&w.small), 0);
}

/// Catches: a streamed read that does not count against the reads in flight (a task opening
/// large reads without end), and one that never gives its place back: with one read allowed, a
/// second waits while the first stream is open, and goes on when it is dropped; a stream read to
/// its end gives its place back too.
#[tokio::test]
async fn an_open_stream_holds_a_read_place_until_it_ends_or_is_dropped() {
    let w = world();
    let bounded = Limits {
        reads_in_flight: 1,
        ..limits()
    };
    let rig = rig(w.blobs.clone(), bounded);
    let token = rig.token(w.action);
    let open = rig.get(&w.big, &token).await.stream.expect("streamed");
    let mut second = Box::pin(rig.get(&w.small, &token));
    assert!(
        futures::poll!(second.as_mut()).is_pending(),
        "the second read waits for the open stream"
    );
    drop(open);
    assert_eq!(second.await.status, StatusCode::OK);

    let finished = rig.get(&w.big, &token).await.stream.expect("streamed");
    let (_, ended) = drain(finished).await;
    assert!(ended.is_none());
    let third = rig.get(&w.small, &token);
    let third = tokio::time::timeout(std::time::Duration::from_secs(20), third).await;
    assert_eq!(
        third.expect("a place was given back").status,
        StatusCode::OK
    );
}

/// Catches: a blob that cannot be opened for streaming being answered as anything but 404
/// (not stored) or 503 (the store failed), and a failure remembered.
#[tokio::test]
async fn a_blob_that_cannot_be_opened_is_not_found_or_unavailable() {
    let w = world();
    let rig = rig(w.blobs.clone(), limits());
    let token = rig.token(w.action);
    *rig.source
        .open_error
        .lock()
        .unwrap_or_else(PoisonError::into_inner) = Some(StorageError::NotFound(w.big));
    let reply = rig.get(&w.big, &token).await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
    assert_eq!(&reply.body[..], b"not_found\n");
    *rig.source
        .open_error
        .lock()
        .unwrap_or_else(PoisonError::into_inner) = Some(StorageError::Unreachable(w.big));
    let reply = rig.get(&w.big, &token).await;
    assert_eq!(reply.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(&reply.body[..], b"store_unavailable\n");
    let reply = rig.get(&w.big, &token).await;
    assert_eq!(reply.status, StatusCode::OK, "the next ask is served");
}

/// Catches: the listener streaming from anywhere but the frontend's own windowed read: over a
/// [`Backend`] on a cache, a blob over the bound is served byte-exact from a stream and a blob
/// the cache lacks is 404.
#[tokio::test]
async fn a_large_blob_streams_through_the_backend() {
    let w = world();
    let cache = Arc::new(Cache::memory());
    let held: Vec<VerifiedBlob> = w
        .blobs
        .iter()
        .map(|(digest, bytes)| VerifiedBlob::new(*digest, bytes.clone()).expect("verified"))
        .collect();
    cache.store_blobs(held).await.expect("stored");
    let grants = grants();
    let token = grants
        .issue(BUILD, w.action, NOT_AFTER)
        .expect("a token")
        .expose()
        .to_owned();
    let read = JobRead::new(Arc::new(Backend::from(cache)), grants, limits());
    let header = format!("Bearer {token}");
    let reply = read
        .respond(&Method::GET, &path(&w.big), Some(&header))
        .await;
    assert_eq!(reply.status, StatusCode::OK);
    let (chunks, ended) = drain(reply.stream.expect("streamed")).await;
    assert!(ended.is_none(), "{ended:?}");
    assert_eq!(joined(&chunks), w.big_bytes);
    let reply = read
        .respond(&Method::GET, &path(&w.at_cap), Some(&header))
        .await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
}
