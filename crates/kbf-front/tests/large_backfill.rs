//! The backfill of a large blob to tierstore: a blob over the whole-blob bound that a job reads
//! from a storage node is copied once, in the background, to `cas/l/<hex>` as the listener
//! verifies it, and never otherwise.
//!
//! The bounds are scaled down as in `job_read_stream`: `stream_over` is 1 KiB and the source
//! hands a blob over in chunks of 1 KiB. The code that runs is the one a 70 MiB blob runs; the
//! object store is the in-memory bucket with create-only writes, behind a [`LargeStore`] that
//! counts what it is asked and can fail, hide a key from the length check, or hold a put open.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use bytes::Bytes;
use futures::future::BoxFuture;
use futures::{StreamExt as _, stream};
use kbf_front::digest_to_proto;
use kbf_front::grants::{GrantKey, Grants, MIN_SECRET_LEN};
use kbf_front::job_read::{BlobSource, BlobStream, JobRead, Limits, Reply, StreamError};
use kbf_front::large_backfill::{LargeBackfill, LargeStore, large_key};
use kbf_objstore::{
    BodyStream, ByteRange, Capabilities, MemoryStore, ObjectKey, ObjectStore, ObjectStoreError,
};
use kbf_proto::reapi::{Action, Directory, FileNode};
use kbf_storage::{ReadStream, StorageError};
use kbf_types::Digest;
use prost::Message as _;
use tokio::sync::Notify;
use tonic::codegen::http::{Method, StatusCode};

const BUILD: &str = "build-0010";
const CHUNK: usize = 1024;
const STREAM_OVER: u64 = 1024;
const BLOB_CHUNKS: usize = 10;
const LONG_CHUNKS: usize = 40;

fn limits() -> Limits {
    Limits {
        stream_over: STREAM_OVER,
        max_blob_bytes: 1 << 20,
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
    /// Fails in place of this chunk.
    FailAt(usize),
}

/// A source whose blobs stream in chunks of [`CHUNK`] bytes, with a fault when told.
struct Source {
    blobs: HashMap<Digest, Bytes>,
    fault: Mutex<Fault>,
    opened: AtomicUsize,
}

impl BlobSource for Source {
    fn read_blob<'a>(&'a self, digest: &'a Digest) -> BoxFuture<'a, Result<Bytes, StorageError>> {
        Box::pin(async move {
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
            self.opened.fetch_add(1, Ordering::SeqCst);
            let mut bytes = self
                .blobs
                .get(digest)
                .ok_or(StorageError::NotFound(*digest))?
                .to_vec();
            let fault = *self.fault.lock().unwrap_or_else(PoisonError::into_inner);
            if let Fault::Flip(at) = fault {
                bytes[at] ^= 0xff;
            }
            let mut items: Vec<Result<Bytes, StorageError>> = bytes
                .chunks(CHUNK)
                .map(|chunk| Ok(Bytes::copy_from_slice(chunk)))
                .collect();
            match fault {
                Fault::Truncate(n) => items.truncate(n),
                Fault::FailAt(n) => {
                    items.truncate(n);
                    items.push(Err(StorageError::Unreachable(*digest)));
                }
                Fault::None | Fault::Flip(_) => {}
            }
            Ok(stream::iter(items).boxed())
        })
    }
}

/// What the store does to a put.
#[derive(Clone, Copy, PartialEq)]
enum Put {
    Store,
    Fail,
}

/// The bucket behind the backfill, as the frontend's packs store is.
struct Bucket {
    memory: MemoryStore,
    puts: AtomicUsize,
    heads: AtomicUsize,
    mode: Mutex<Put>,
    /// The length check answers "no object" whatever is there, so that the create-only write is
    /// what finds the key taken.
    blind: Mutex<bool>,
    /// A put waits here before it takes its body.
    gate: Mutex<Option<Arc<Notify>>>,
    waiting: AtomicU64,
}

impl Bucket {
    fn new(conditional: bool) -> Arc<Self> {
        Arc::new(Self {
            memory: MemoryStore::new(Capabilities {
                conditional_put: conditional,
                object_lock: false,
            }),
            puts: AtomicUsize::new(0),
            heads: AtomicUsize::new(0),
            mode: Mutex::new(Put::Store),
            blind: Mutex::new(false),
            gate: Mutex::new(None),
            waiting: AtomicU64::new(0),
        })
    }

    fn set_mode(&self, mode: Put) {
        *self.mode.lock().unwrap_or_else(PoisonError::into_inner) = mode;
    }

    fn set_blind(&self) {
        *self.blind.lock().unwrap_or_else(PoisonError::into_inner) = true;
    }

    fn hold_puts(&self) -> Arc<Notify> {
        let gate = Arc::new(Notify::new());
        *self.gate.lock().unwrap_or_else(PoisonError::into_inner) = Some(Arc::clone(&gate));
        gate
    }

    async fn object(&self, digest: &Digest) -> Option<Bytes> {
        let key = large_key(digest);
        let len = self.memory.len(&key).await.expect("length")?;
        let range = ByteRange::new(0, len).expect("a non-empty object");
        Some(
            self.memory
                .get_range(&key, range)
                .await
                .expect("the object"),
        )
    }

    async fn keys(&self) -> usize {
        let prefix = kbf_objstore::KeyPrefix::new("").expect("a prefix");
        let page = self
            .memory
            .list(&prefix, None, kbf_objstore::PageSize::MAX)
            .await
            .expect("a listing");
        page.objects.len()
    }
}

impl LargeStore for Bucket {
    fn len<'a>(
        &'a self,
        key: &'a ObjectKey,
    ) -> BoxFuture<'a, Result<Option<u64>, ObjectStoreError>> {
        Box::pin(async move {
            self.heads.fetch_add(1, Ordering::SeqCst);
            if *self.blind.lock().unwrap_or_else(PoisonError::into_inner) {
                return Ok(None);
            }
            self.memory.len(key).await
        })
    }

    fn put<'a>(
        &'a self,
        key: &'a ObjectKey,
        len: u64,
        body: BodyStream,
    ) -> BoxFuture<'a, Result<(), ObjectStoreError>> {
        Box::pin(async move {
            self.puts.fetch_add(1, Ordering::SeqCst);
            let gate = self
                .gate
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone();
            if let Some(gate) = gate {
                self.waiting.fetch_add(1, Ordering::SeqCst);
                gate.notified().await;
            }
            if *self.mode.lock().unwrap_or_else(PoisonError::into_inner) == Put::Fail {
                return Err(ObjectStoreError::Service {
                    status: 500,
                    code: "InternalError".to_owned(),
                    message: "refused".to_owned(),
                });
            }
            self.memory.put_stream(key, len, body).await
        })
    }
}

fn pattern(seed: usize) -> Vec<u8> {
    pattern_of(BLOB_CHUNKS, seed)
}

fn pattern_of(chunks: usize, seed: usize) -> Vec<u8> {
    (0..chunks * CHUNK)
        .map(|i| ((i + seed) * 31 % 251) as u8)
        .collect()
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

/// An Action whose inputs are two large blobs.
struct World {
    action: Digest,
    one: Digest,
    two: Digest,
    /// More chunks than a tee buffers.
    long: Digest,
    blobs: HashMap<Digest, Bytes>,
}

fn world() -> World {
    let mut blobs = HashMap::new();
    let one = put(&mut blobs, pattern(1));
    let two = put(&mut blobs, pattern(2));
    let long = put(&mut blobs, pattern_of(LONG_CHUNKS, 3));
    let root = Directory {
        files: vec![file("one", &one), file("two", &two), file("long", &long)],
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
        one,
        two,
        long,
        blobs,
    }
}

struct Rig {
    source: Arc<Source>,
    bucket: Arc<Bucket>,
    read: JobRead,
    backfill: LargeBackfill,
    grants: Arc<Grants>,
}

/// The bytes a tee buffers in these tests: 32 pieces of a chunk.
const BUFFER: usize = 32 * CHUNK;

fn rig(blobs: HashMap<Digest, Bytes>, concurrency: usize) -> Rig {
    rig_with(blobs, concurrency, BUFFER, true)
}

/// A rig whose tees buffer `buffer` bytes, over a bucket that refuses an existing key when
/// `conditional`.
fn rig_with(
    blobs: HashMap<Digest, Bytes>,
    concurrency: usize,
    buffer: usize,
    conditional: bool,
) -> Rig {
    let key = GrantKey::from_secret(&[1; MIN_SECRET_LEN]).expect("a long enough secret");
    let clock = Arc::new(AtomicU64::new(100));
    let grants = Arc::new(Grants::new(
        key,
        Arc::new(move || clock.load(Ordering::SeqCst)),
    ));
    let source = Arc::new(Source {
        blobs,
        fault: Mutex::new(Fault::None),
        opened: AtomicUsize::new(0),
    });
    let bucket = Bucket::new(conditional);
    let backfill = LargeBackfill::with_buffer_bytes(bucket.clone(), concurrency, buffer);
    let read = JobRead::new(source.clone(), Arc::clone(&grants), limits())
        .with_backfill(Some(backfill.clone()));
    Rig {
        source,
        bucket,
        read,
        backfill,
        grants,
    }
}

impl Rig {
    fn token(&self, action: Digest) -> String {
        self.grants
            .issue(BUILD, action, 1_000)
            .expect("a valid build id")
            .expose()
            .to_owned()
    }

    async fn get(&self, digest: &Digest, token: &str) -> Reply {
        let header = format!("Bearer {token}");
        self.read
            .respond(&Method::GET, &format!("/{digest}"), Some(&header))
            .await
    }

    fn set_fault(&self, fault: Fault) {
        *self
            .source
            .fault
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = fault;
    }

    /// The count of `kbf_large_backfill_total{result}`.
    fn count(&self, result: &str) -> u64 {
        let mut out = String::new();
        self.read.render_metrics(&mut out);
        let prefix = format!("kbf_large_backfill_total{{result=\"{result}\"}} ");
        out.lines()
            .find_map(|line| line.strip_prefix(&prefix))
            .unwrap_or_else(|| panic!("no {result} line in\n{out}"))
            .parse()
            .expect("a count")
    }

    /// Waits for the background puts to have ended `n` times in all: the put signals each end.
    async fn settled(&self, n: u64) {
        self.backfill.ended(n).await;
    }
}

async fn drain(stream: BlobStream) -> (Vec<u8>, Option<StreamError>) {
    let mut chunks = stream.chunks;
    let mut got = Vec::new();
    while let Some(item) = chunks.next().await {
        match item {
            Ok(chunk) => got.extend_from_slice(&chunk),
            Err(error) => return (got, Some(error)),
        }
    }
    (got, None)
}

/// A read of `digest` served in full: its status and bytes.
async fn read_all(rig: &Rig, digest: &Digest, token: &str) -> (StatusCode, Vec<u8>) {
    let reply = rig.get(digest, token).await;
    let stream = reply.stream.expect("a streamed reply");
    let (bytes, ended) = drain(stream).await;
    assert!(ended.is_none(), "{ended:?}");
    (reply.status, bytes)
}

/// Catches: a verified large blob never reaching `cas/l/<hex>`, the object holding other bytes
/// than the blob's, a put per read instead of one per blob (the second read finds the object
/// by the length check and sends nothing), and a backfill that is not counted.
#[tokio::test]
async fn a_streamed_read_copies_the_blob_once_to_its_large_key() {
    let w = world();
    let rig = rig(w.blobs.clone(), 2);
    let token = rig.token(w.action);
    let (status, bytes) = read_all(&rig, &w.one, &token).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(bytes, w.blobs[&w.one]);
    rig.settled(1).await;
    assert_eq!(rig.count("backfilled"), 1);
    assert_eq!(
        rig.bucket.object(&w.one).await,
        Some(w.blobs[&w.one].clone())
    );
    assert_eq!(rig.bucket.puts.load(Ordering::SeqCst), 1);
    let (_, again) = read_all(&rig, &w.one, &token).await;
    assert_eq!(again, w.blobs[&w.one]);
    rig.settled(2).await;
    assert_eq!(rig.count("already_present"), 1);
    assert_eq!(rig.count("backfilled"), 1);
    assert_eq!(
        rig.bucket.puts.load(Ordering::SeqCst),
        1,
        "an object that is there is not sent again"
    );
}

/// Catches: two readers of one blob each uploading it. The first put is held open while the
/// second read is served; only one put is ever started and the second read is counted skipped.
#[tokio::test]
async fn two_concurrent_reads_of_one_blob_cause_one_put() {
    let w = world();
    let rig = rig(w.blobs.clone(), 2);
    let gate = rig.bucket.hold_puts();
    let token = rig.token(w.action);
    let (_, first) = read_all(&rig, &w.one, &token).await;
    let (_, second) = read_all(&rig, &w.one, &token).await;
    assert_eq!(first, second);
    assert_eq!(rig.count("skipped"), 1);
    gate.notify_one();
    rig.settled(2).await;
    assert_eq!(rig.bucket.puts.load(Ordering::SeqCst), 1);
    assert_eq!(rig.count("backfilled"), 1);
    assert_eq!(
        rig.bucket.object(&w.one).await,
        Some(w.blobs[&w.one].clone())
    );
}

/// Catches: no bound on the puts at once. With one place and the first put held open, a read of
/// another blob is served and not copied (skipped), and once the place is free it is.
#[tokio::test]
async fn the_puts_at_once_are_bounded() {
    let w = world();
    let rig = rig(w.blobs.clone(), 1);
    let gate = rig.bucket.hold_puts();
    let token = rig.token(w.action);
    read_all(&rig, &w.one, &token).await;
    let (_, bytes) = read_all(&rig, &w.two, &token).await;
    assert_eq!(
        bytes, w.blobs[&w.two],
        "the read is served whatever the puts do"
    );
    assert_eq!(rig.count("skipped"), 1);
    gate.notify_one();
    rig.settled(2).await;
    assert_eq!(rig.bucket.keys().await, 1);
    assert_eq!(rig.bucket.object(&w.two).await, None);
}

/// Catches: a create-only refusal counted as a failure. The length check is blind, so the put
/// is what finds the key taken, and that is `already_present`; the object is left as it was.
#[tokio::test]
async fn a_refused_create_only_put_counts_as_already_present() {
    let w = world();
    let rig = rig(w.blobs.clone(), 2);
    let key = large_key(&w.one);
    let body: BodyStream = stream::iter([Ok(w.blobs[&w.one].clone())]).boxed();
    let len = w.one.size_bytes;
    rig.bucket
        .memory
        .put_stream(&key, len, body)
        .await
        .expect("seeded");
    rig.bucket.set_blind();
    let token = rig.token(w.action);
    let (status, bytes) = read_all(&rig, &w.one, &token).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(bytes, w.blobs[&w.one]);
    rig.settled(1).await;
    assert_eq!(rig.count("already_present"), 1);
    assert_eq!(rig.count("failed"), 0);
    assert_eq!(rig.bucket.puts.load(Ordering::SeqCst), 1);
}

/// Catches: a failing put changing what the reader gets (bytes, status or an error at the end of
/// the stream), and a failure that is not counted.
#[tokio::test]
async fn a_failed_put_does_not_change_the_response() {
    let w = world();
    let rig = rig(w.blobs.clone(), 2);
    rig.bucket.set_mode(Put::Fail);
    let token = rig.token(w.action);
    let (status, bytes) = read_all(&rig, &w.one, &token).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(bytes, w.blobs[&w.one]);
    rig.settled(1).await;
    assert_eq!(rig.count("failed"), 1);
    assert_eq!(rig.count("backfilled"), 0);
    assert_eq!(rig.bucket.keys().await, 0);
    let out = {
        let mut out = String::new();
        rig.read.render_metrics(&mut out);
        out
    };
    assert!(
        out.lines()
            .any(|l| l == "kbf_job_read_stream_ended_early_total{reason=\"store_error\"} 0"),
        "{out}"
    );
}

/// Catches: unverified bytes reaching tierstore. A reader that stops after a few chunks (the
/// client went away) leaves no object under `cas/l/`.
#[tokio::test]
async fn a_reader_that_stops_early_leaves_no_object() {
    let w = world();
    let rig = rig(w.blobs.clone(), 2);
    let token = rig.token(w.action);
    let stream = rig.get(&w.one, &token).await.stream.expect("streamed");
    let mut chunks = stream.chunks;
    for _ in 0..3 {
        chunks.next().await.expect("a chunk").expect("ok");
    }
    drop(chunks);
    rig.settled(1).await;
    assert_eq!(rig.count("backfilled"), 0);
    assert_eq!(rig.count("skipped"), 1);
    assert_eq!(rig.bucket.keys().await, 0);
}

/// Catches: a corrupt storage-node copy landing under `cas/l/<hex>`: the listener refuses the
/// blob, and the put's body ends short of its length, so the store keeps nothing, whether the
/// flipped byte is in the middle or in the last chunk (which is held back until the digest is
/// known).
#[tokio::test]
async fn a_corrupt_source_never_leaves_an_object() {
    for (what, at) in [
        ("the middle", 5 * CHUNK + 7),
        ("the last byte", BLOB_CHUNKS * CHUNK - 1),
    ] {
        let w = world();
        let rig = rig(w.blobs.clone(), 2);
        rig.set_fault(Fault::Flip(at));
        let token = rig.token(w.action);
        let stream = rig.get(&w.one, &token).await.stream.expect("streamed");
        let (_, ended) = drain(stream).await;
        assert!(matches!(ended, Some(StreamError::Corrupt)), "{what}");
        rig.settled(1).await;
        assert_eq!(rig.count("backfilled"), 0, "{what}");
        assert_eq!(rig.bucket.keys().await, 0, "{what}");
        assert_eq!(rig.bucket.object(&w.one).await, None, "{what}");
    }
}

/// Catches: a put that cannot keep up slowing or failing the reader. The put is held before it
/// takes a byte, so the tee's buffer fills; the read is served in full, the copy is abandoned
/// (skipped, never failed) and no object results once the put is let go.
#[tokio::test]
async fn a_put_that_cannot_keep_up_is_abandoned_and_the_read_goes_on() {
    let w = world();
    let rig = rig(w.blobs.clone(), 2);
    let gate = rig.bucket.hold_puts();
    let token = rig.token(w.action);
    let (status, bytes) = read_all(&rig, &w.long, &token).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(bytes, w.blobs[&w.long]);
    gate.notify_one();
    rig.settled(1).await;
    assert_eq!(rig.count("skipped"), 1);
    assert_eq!(rig.count("failed"), 0);
    assert_eq!(rig.count("backfilled"), 0);
    assert_eq!(rig.bucket.keys().await, 0);
}

/// Catches: an object of another length under the key being taken for the blob, or written
/// over. The length check does not match, so nothing is put (a store that does not refuse an
/// existing key would replace the object): the copy is `failed`, and the object that was there is
/// left as it was, over a store that refuses an existing key and over one that does not.
#[tokio::test]
async fn an_object_of_another_length_is_not_written_over() {
    for conditional in [true, false] {
        let w = world();
        let rig = rig_with(w.blobs.clone(), 2, BUFFER, conditional);
        let key = large_key(&w.one);
        let body: BodyStream = stream::iter([Ok(Bytes::from_static(b"short"))]).boxed();
        rig.bucket
            .memory
            .put_stream(&key, 5, body)
            .await
            .expect("seeded");
        let token = rig.token(w.action);
        let (_, bytes) = read_all(&rig, &w.one, &token).await;
        assert_eq!(bytes, w.blobs[&w.one], "conditional {conditional}");
        rig.settled(1).await;
        assert_eq!(rig.count("failed"), 1, "conditional {conditional}");
        assert_eq!(rig.count("already_present"), 0, "conditional {conditional}");
        assert_eq!(
            rig.bucket.puts.load(Ordering::SeqCst),
            0,
            "conditional {conditional}: nothing is put over it"
        );
        assert_eq!(
            rig.bucket.object(&w.one).await,
            Some(Bytes::from_static(b"short")),
            "conditional {conditional}"
        );
    }
}

/// Catches: a put whose body takes a closed channel for the end of the blob. Every chunk of a
/// corrupt blob has been pulled and hashed when the reader has the one before the last, but the
/// digest is checked only on the poll after it; a reader that stops there (the stream dropped
/// without being polled to its end) must leave nothing under `cas/l/`.
#[tokio::test]
async fn a_reader_that_stops_before_the_verdict_on_a_corrupt_source_leaves_no_object() {
    let w = world();
    let rig = rig(w.blobs.clone(), 2);
    rig.set_fault(Fault::Flip(2 * CHUNK + 3));
    let token = rig.token(w.action);
    let stream = rig.get(&w.one, &token).await.stream.expect("streamed");
    let mut chunks = stream.chunks;
    for _ in 0..BLOB_CHUNKS - 1 {
        chunks.next().await.expect("a chunk").expect("ok");
    }
    drop(chunks);
    rig.settled(1).await;
    assert_eq!(rig.count("backfilled"), 0);
    assert_eq!(rig.bucket.keys().await, 0);
}

/// Catches: a tee too small for the pieces it is given. The blob is 40 pieces, more than the
/// 16 the buffer used to count, but fits the byte budget; the put is held before it takes a
/// byte, as a store busy with one part and setting up the next is, and then the whole blob is
/// there to send: it is backfilled.
#[tokio::test]
async fn small_pieces_that_fit_the_byte_budget_are_backfilled_while_the_put_is_slow() {
    let w = world();
    let rig = rig_with(w.blobs.clone(), 2, 2 * LONG_CHUNKS * CHUNK, true);
    let gate = rig.bucket.hold_puts();
    let token = rig.token(w.action);
    let (_, bytes) = read_all(&rig, &w.long, &token).await;
    assert_eq!(bytes, w.blobs[&w.long]);
    gate.notify_one();
    rig.settled(1).await;
    assert_eq!(rig.count("backfilled"), 1);
    assert_eq!(rig.count("skipped"), 0);
    assert_eq!(
        rig.bucket.object(&w.long).await,
        Some(w.blobs[&w.long].clone())
    );
}

/// Catches: a copy lost when the client stops reading at its Content-Length, which is what an
/// HTTP server does: every chunk has been taken, the stream is dropped without being polled
/// for its end. The last chunk is passed on only after the digest checked, and with it the
/// marker, so the object is stored.
#[tokio::test]
async fn a_reader_that_stops_after_the_last_chunk_still_backfills() {
    let w = world();
    let rig = rig(w.blobs.clone(), 2);
    let token = rig.token(w.action);
    let stream = rig.get(&w.one, &token).await.stream.expect("streamed");
    let mut chunks = stream.chunks;
    for _ in 0..BLOB_CHUNKS {
        chunks.next().await.expect("a chunk").expect("ok");
    }
    drop(chunks);
    rig.settled(1).await;
    assert_eq!(rig.count("backfilled"), 1);
    assert_eq!(
        rig.bucket.object(&w.one).await,
        Some(w.blobs[&w.one].clone())
    );
}

/// Catches: a source that fails part way, or ends short of the blob, leaving a partial object.
/// Neither is a failed put: the copy was abandoned before the blob verified, and nothing is
/// under the key.
#[tokio::test]
async fn a_source_that_fails_or_ends_short_leaves_no_object() {
    for (what, fault) in [
        ("an error part way", Fault::FailAt(4)),
        ("an upstream that ends short", Fault::Truncate(7)),
    ] {
        let w = world();
        let rig = rig(w.blobs.clone(), 2);
        rig.set_fault(fault);
        let token = rig.token(w.action);
        let stream = rig.get(&w.one, &token).await.stream.expect("streamed");
        let (_, ended) = drain(stream).await;
        assert!(ended.is_some(), "{what}");
        rig.settled(1).await;
        assert_eq!(rig.count("backfilled"), 0, "{what}");
        assert_eq!(rig.count("failed"), 0, "{what}");
        assert_eq!(rig.count("skipped"), 1, "{what}");
        assert_eq!(rig.bucket.keys().await, 0, "{what}");
    }
}

/// Catches: a failed put keeping the blob's claim or the put's place, so that the blob is never
/// copied again. With one place, a read after a failed put is backfilled.
#[tokio::test]
async fn a_read_after_a_failed_put_backfills_again() {
    let w = world();
    let rig = rig(w.blobs.clone(), 1);
    rig.bucket.set_mode(Put::Fail);
    let token = rig.token(w.action);
    read_all(&rig, &w.one, &token).await;
    rig.settled(1).await;
    assert_eq!(rig.count("failed"), 1);
    rig.bucket.set_mode(Put::Store);
    read_all(&rig, &w.one, &token).await;
    rig.settled(2).await;
    assert_eq!(rig.count("backfilled"), 1);
    assert_eq!(
        rig.count("skipped"),
        0,
        "the claim and the place were given back"
    );
    assert_eq!(
        rig.bucket.object(&w.one).await,
        Some(w.blobs[&w.one].clone())
    );
}

/// Catches: a reader that stops taking chunks (so the listener's idle limit drops its body)
/// leaving a copy behind: with a tee attached, a client that takes three chunks and stalls past
/// the idle limit (on a paused clock, no real waiting) is told `Idle`, the put is abandoned and
/// counted `skipped`, nothing is stored under any large key, and the blob is not counted
/// backfilled.
#[tokio::test(start_paused = true)]
async fn a_reader_that_stalls_past_the_idle_limit_leaves_no_object_and_an_abandoned_put() {
    let w = world();
    let rig = rig(w.blobs.clone(), 2);
    let token = rig.token(w.action);
    let reply = rig.get(&w.one, &token).await;
    let mut chunks = reply.stream.expect("a streamed reply").chunks;
    for _ in 0..3 {
        chunks.next().await.expect("a chunk").expect("ok");
    }
    tokio::time::sleep(limits().held_idle + std::time::Duration::from_secs(1)).await;
    assert!(matches!(chunks.next().await, Some(Err(StreamError::Idle))));
    rig.settled(1).await;
    assert_eq!(rig.count("skipped"), 1);
    assert_eq!(rig.count("backfilled"), 0);
    assert_eq!(rig.bucket.keys().await, 0);
    assert!(rig.bucket.object(&w.one).await.is_none());
}
