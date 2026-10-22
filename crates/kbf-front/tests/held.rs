//! The Held set (`docs/design/storage-ram-ack.md`): blobs a node holds in RAM and has
//! acknowledged but not yet written to its disk. These tests put blobs there through
//! `Cache::hold_for_test` and check what each reader does: every one consults the set before the index (rule 1, never report
//! present what you cannot serve), and the durable question does not consult it.
//!
//! Each reader's test holds a blob that is **only** held (not in the index), so a reader
//! that skips the set answers as if the blob were absent. The order test holds a blob and
//! drains it between a reader's two steps. Segments `Locate` is in `held_segments.rs`.

use std::sync::{Arc, Mutex, PoisonError};

use bytes::Bytes;
use kbf_front::{
    BetweenHook, Cache, CacheError, CacheStorage, MemoryMetaLog, READ_WINDOW_BYTES, SegmentsConfig,
    VerifiedBlob,
};
use kbf_objstore::MemoryStore;
use kbf_storage::Storage;
use kbf_storage::service::{Admission, Config, RemoteStorage, StorageService};
use kbf_types::Digest;
use tonic::transport::server::TcpIncoming;
use tonic::transport::{Channel, Endpoint, Server};

type MemCache = Cache<MemoryMetaLog, MemoryStore>;

/// A blob and its bytes.
#[derive(Clone)]
struct Blob {
    bytes: Bytes,
    verified: VerifiedBlob,
}

impl Blob {
    fn digest(&self) -> Digest {
        self.verified.digest()
    }
}

/// A blob of `len` bytes that differ from place to place, different for each `seed`.
fn blob(seed: u8, len: usize) -> Blob {
    let bytes: Vec<u8> = (0..len)
        .map(|i| (i * 31 % 251) as u8 ^ seed.wrapping_mul(37))
        .collect();
    let bytes = Bytes::from(bytes);
    Blob {
        verified: VerifiedBlob::hashed(bytes.clone()),
        bytes,
    }
}

/// A cache with `held` held in RAM only and `indexed` committed to the index.
async fn cache_with(held: &[&Blob], indexed: &[&Blob]) -> Arc<MemCache> {
    let cache = Arc::new(Cache::memory());
    for blob in held {
        assert!(cache.hold_for_test(&blob.verified), "held once");
    }
    let indexed: Vec<VerifiedBlob> = indexed.iter().map(|b| b.verified.clone()).collect();
    cache.store_blobs(indexed).await.expect("stored");
    cache
}

fn absent() -> Digest {
    kbf_segments::sha256(b"nowhere")
}

/// Catches `find_missing` (rule 1, and the write dedup and `QueryWriteStatus` that read
/// it) skipping the set: a blob that is only held is present, one that is indexed is
/// present, and the absent one and the empty blob are as before. Mutant: `find_missing`
/// asks the index for every digest.
#[tokio::test]
async fn find_missing_counts_a_held_blob_present() {
    let (held, indexed) = (blob(1, 100), blob(2, 100));
    let cache = cache_with(&[&held], &[&indexed]).await;
    let asked = [absent(), held.digest(), indexed.digest()];
    assert_eq!(cache.find_missing(&asked).await.expect("found"), [absent()]);
    // A digest asked twice is answered twice, held or not.
    let twice = [held.digest(), absent(), held.digest(), absent()];
    assert_eq!(
        cache.find_missing(&twice).await.expect("found"),
        [absent(), absent()]
    );
}

/// Catches `present` skipping the set. Mutant: `present` asks the index alone.
#[tokio::test]
async fn present_counts_a_held_blob_present() {
    let (held, indexed) = (blob(3, 100), blob(4, 100));
    let cache = cache_with(&[&held], &[&indexed]).await;
    let asked = [held.digest(), absent(), indexed.digest()];
    assert_eq!(
        cache.present(&asked).await.expect("present"),
        [true, false, true]
    );
}

/// Catches `is_complete` (the `QueryWriteStatus` answer of a cache) skipping the set.
/// Mutant: `is_complete` asks the index alone.
#[tokio::test]
async fn a_held_blob_is_complete_for_a_write_status() {
    let held = blob(5, 100);
    let cache = cache_with(&[&held], &[]).await;
    assert!(cache.is_complete(&held.digest()).await.expect("status"));
    assert!(!cache.is_complete(&absent()).await.expect("status"));
}

/// Catches `read_windows` and `read_blob` skipping the set (the blob would be NOT_FOUND
/// though the node can serve it), and a held read that serves wrong bytes: the whole blob,
/// a range inside one window, and a range across the boundary of two windows of a blob of
/// more than one all read back the held bytes, hashed as any read is; an absent blob is
/// still NOT_FOUND. Mutant: `read_windows` asks the index alone.
#[tokio::test]
async fn a_held_blob_is_read_from_memory() {
    let small = blob(6, 1000);
    let large = blob(7, READ_WINDOW_BYTES as usize + 1000);
    let cache = cache_with(&[&small, &large], &[]).await;

    assert_eq!(
        cache.read_blob(&small.digest()).await.expect("read"),
        small.bytes
    );
    let mut ranged = cache
        .read_windows(&small.digest(), 10, 500)
        .await
        .expect("open");
    let piece = ranged.next().await.expect("a piece").expect("read");
    assert_eq!(piece, small.bytes.slice(10..500));
    assert!(ranged.next().await.is_none());

    let at = READ_WINDOW_BYTES;
    let mut across = cache
        .read_windows(&large.digest(), at - 10, at + 10)
        .await
        .expect("open");
    let mut got = Vec::new();
    while let Some(piece) = across.next().await {
        got.extend_from_slice(&piece.expect("read"));
    }
    assert_eq!(got, large.bytes.slice(at as usize - 10..at as usize + 10));

    let missing = cache.read_blob(&absent()).await.expect_err("absent");
    assert!(matches!(missing, CacheError::NotFound(_)), "{missing:?}");
}

/// Catches the batch read skipping the set, and answering a held blob wrongly beside
/// indexed and absent ones: one call over a held blob (asked twice), an indexed blob and an
/// absent one answers each by its own rule. Mutant: `read_blobs` asks the index for every
/// digest.
#[tokio::test]
async fn a_batch_read_serves_held_blobs_beside_indexed_and_absent_ones() {
    let (held, indexed) = (blob(8, 300), blob(9, 300));
    let cache = cache_with(&[&held], &[&indexed]).await;
    let asked = [held.digest(), indexed.digest(), absent(), held.digest()];
    let answers = cache.read_blobs(&asked).await.expect("answered");
    assert_eq!(answers.len(), 3, "one answer per distinct digest");
    assert_eq!(answers[&held.digest()].as_ref().expect("held"), &held.bytes);
    assert_eq!(
        answers[&indexed.digest()].as_ref().expect("indexed"),
        &indexed.bytes
    );
    assert!(matches!(
        answers[&absent()],
        Err(CacheError::NotFound(d)) if d == absent()
    ));
}

/// Catches the durable question consulting the set (a record that names a blob could then
/// be written for one a restart loses), and one that counts nothing for the empty blob:
/// `durable` names the held-only blob and the absent one missing, not the indexed one or
/// the empty one. Mutant: `durable` skips the digests the set holds (the Held-consulting
/// durable reader).
#[tokio::test]
async fn the_durable_question_does_not_count_a_held_blob() {
    let (held, indexed) = (blob(10, 100), blob(11, 100));
    let cache = cache_with(&[&held], &[&indexed]).await;
    let empty = kbf_segments::sha256(b"");
    let asked = [held.digest(), indexed.digest(), absent(), empty];
    assert_eq!(
        cache.durable(&asked).await.expect("durable"),
        [held.digest(), absent()]
    );
    // The plain question beside it counts the held one present.
    assert_eq!(cache.find_missing(&asked).await.expect("found"), [absent()]);
}

/// Catches a store that trusts the set (a blob stored while only held would be answered
/// as stored and never reach the index, so a caller that wrote something durable naming it
/// would be wrong): `store_blobs` of a held blob writes it, and `durable` then counts it.
/// Mutant: `held()`, the store's dedup, looks at the set.
#[tokio::test]
async fn storing_a_held_blob_writes_it() {
    let held = blob(12, 100);
    let cache = cache_with(&[&held], &[]).await;
    assert_eq!(
        cache.durable(&[held.digest()]).await.expect("durable"),
        [held.digest()]
    );
    cache
        .store_blobs(vec![held.verified.clone()])
        .await
        .expect("stored");
    assert_eq!(cache.durable(&[held.digest()]).await.expect("durable"), []);
}

/// Catches an insert that overwrites (the second writer of a digest would take the first
/// writer's entry, and the first's drain would remove an entry the second depends on), and
/// a drop that reports a blob that was not held: the second `hold` of a digest reports it
/// was held already, the first release removes it, the second finds nothing, and the
/// blob is then absent to every reader. Mutant: `try_insert` always inserts and reports
/// true.
#[tokio::test]
async fn a_digest_is_held_once_and_released_once() {
    let held = blob(13, 100);
    let cache = Arc::new(Cache::memory());
    assert!(cache.hold_for_test(&held.verified));
    assert!(
        !cache.hold_for_test(&held.verified),
        "a second writer is a dedup"
    );
    assert!(cache.release_for_test(&held.digest()));
    assert!(!cache.release_for_test(&held.digest()));
    assert_eq!(
        cache.find_missing(&[held.digest()]).await.expect("found"),
        [held.digest()]
    );
}

/// The readers, each asked for one blob the drain moves between the reader's Held step and
/// its index step.
#[derive(Clone, Copy, Debug)]
enum Reader {
    FindMissing,
    Present,
    IsComplete,
    ReadBlob,
    ReadBlobs,
}

impl Reader {
    const ALL: [Reader; 5] = [
        Self::FindMissing,
        Self::Present,
        Self::IsComplete,
        Self::ReadBlob,
        Self::ReadBlobs,
    ];

    /// Whether this reader finds `blob` in `cache`.
    async fn finds(self, cache: &MemCache, blob: &Blob) -> bool {
        let digest = blob.digest();
        match self {
            Self::FindMissing => cache
                .find_missing(&[digest])
                .await
                .expect("found")
                .is_empty(),
            Self::Present => cache.present(&[digest]).await.expect("present")[0],
            Self::IsComplete => cache.is_complete(&digest).await.expect("status"),
            Self::ReadBlob => cache
                .read_blob(&digest)
                .await
                .is_ok_and(|b| b == blob.bytes),
            Self::ReadBlobs => cache.read_blobs(&[digest]).await.expect("read")[&digest]
                .as_ref()
                .is_ok_and(|b| *b == blob.bytes),
        }
    }
}

/// A cache holding `blob` in RAM only, whose first reader to finish its Held step finds
/// the drain run before it takes its index step: the index takes the blob, then the set
/// drops it, in the order the drain does them.
fn draining_between_the_steps(blob: &Blob) -> Arc<MemCache> {
    let cell: Arc<Mutex<Option<Arc<MemCache>>>> = Arc::default();
    let hook: BetweenHook = {
        let (cell, blob) = (Arc::clone(&cell), blob.clone());
        Arc::new(move || {
            let (cell, blob) = (Arc::clone(&cell), blob.clone());
            Box::pin(async move {
                let cache = cell
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .clone()
                    .expect("the cache is set before a read");
                cache
                    .store_blobs(vec![blob.verified.clone()])
                    .await
                    .expect("stored");
                cache.release_for_test(&blob.digest());
            })
        })
    };
    let cache = Arc::new(Cache::memory().with_between_hook(hook));
    assert!(cache.hold_for_test(&blob.verified));
    *cell.lock().unwrap_or_else(PoisonError::into_inner) = Some(Arc::clone(&cache));
    cache
}

/// The drain race (T6): a reader that looks at the set and then the index never misses a
/// blob that moves from one to the other between the two steps. Each reader is parked
/// between its steps by the hook, which lets the index take the blob and the set drop it;
/// every reader still finds the blob. Catches a reader that looks at the index before the
/// set (it would see the blob in neither). Mutants: `find_missing` and `read_windows` with
/// the steps swapped.
#[tokio::test]
async fn a_blob_moving_from_the_set_to_the_index_between_the_steps_is_never_missed() {
    for reader in Reader::ALL {
        let moving = blob(14, 200);
        let cache = draining_between_the_steps(&moving);
        assert!(reader.finds(&cache, &moving).await, "{reader:?} missed it");
    }
}

/// The drain race for a batch (T6, batch form): `read_blobs` over a held blob that moves
/// to the index mid-read, an indexed blob, and an absent one answers every digest. The
/// held bytes and the unheld list come from one look at the set, so a blob the drain
/// moves between that look and the index query is in the set's part or the index's,
/// never in neither. Mutant: `read_blobs` looks at the set twice (the unheld list, then
/// the held bytes) with the drain between them: the moving blob is in no answer.
#[tokio::test]
async fn a_batch_read_never_loses_a_blob_the_drain_moves_mid_read() {
    let moving = blob(15, 300);
    let indexed = blob(16, 120);
    let absent = blob(17, 50);
    let cache = draining_between_the_steps(&moving);
    cache
        .store_blobs(vec![indexed.verified.clone()])
        .await
        .expect("stored");
    let digests = [moving.digest(), indexed.digest(), absent.digest()];
    let read = cache.read_blobs(&digests).await.expect("read");
    assert_eq!(read.len(), 3, "one answer per digest");
    assert_eq!(
        read[&moving.digest()].as_ref().expect("moving"),
        &moving.bytes
    );
    assert_eq!(
        read[&indexed.digest()].as_ref().expect("indexed"),
        &indexed.bytes
    );
    assert!(matches!(
        read[&absent.digest()],
        Err(CacheError::NotFound(_))
    ));
}

/// Several readers of one cache race one drain: each parks between its steps, the first
/// hook to run moves the blob, and every reader of every kind still finds it. Catches a
/// reader whose answer depends on the drain having run before or after it, not during.
#[tokio::test]
async fn several_readers_racing_one_drain_all_find_the_blob() {
    let moving = blob(18, 250);
    let cache = draining_between_the_steps(&moving);
    let found = futures::future::join_all(
        Reader::ALL
            .iter()
            .cycle()
            .take(15)
            .map(|reader| reader.finds(&cache, &moving)),
    )
    .await;
    assert!(found.iter().all(|f| *f), "{found:?}");
}

fn serve(service: StorageService) -> Channel {
    let incoming = TcpIncoming::bind(([127, 0, 0, 1], 0).into()).expect("bind");
    let addr = incoming.local_addr().expect("address");
    tokio::spawn(
        Server::builder()
            .add_routes(service.routes())
            .serve_with_incoming(incoming),
    );
    Endpoint::from_shared(format!("http://{addr}"))
        .expect("endpoint")
        .connect_lazy()
}

/// A storage node over `cache`, and a client of it.
fn node(cache: &Arc<MemCache>) -> RemoteStorage {
    let storage: Arc<dyn Storage> = Arc::new(CacheStorage::new(
        Arc::clone(cache),
        SegmentsConfig::DEFAULT,
    ));
    let config = Config {
        write_bytes_in_flight: 64 << 20,
    };
    RemoteStorage::new(
        serve(StorageService::new(storage, Admission::Open, config)),
        0,
    )
}

/// Catches the header not reaching the cache, and a plain FindMissing that does not
/// count a held blob: over the wire, `FindMissingBlobs` counts the held blob present, and
/// with `kbf-durable: only` names it missing, the indexed blob present in both. Mutant:
/// `CacheStorage::find_durable` answers `find_missing`.
#[tokio::test]
async fn the_durable_header_ignores_the_set_over_the_wire() {
    let (held, indexed) = (blob(15, 100), blob(16, 100));
    let cache = cache_with(&[&held], &[&indexed]).await;
    let remote = node(&cache);
    let asked = [held.digest(), indexed.digest(), absent()];
    assert_eq!(
        remote.find_missing(&asked).await.expect("plain"),
        [absent()]
    );
    assert_eq!(
        remote.find_durable(&asked).await.expect("durable"),
        [held.digest(), absent()]
    );
}

/// Catches a write's dedup (`write_in`, which asks the plain FindMissing before it opens a
/// stream) writing a blob the node already holds in RAM: a `Write` of a held blob is
/// acknowledged with its digest and stores nothing (the index still lacks it), as one of
/// an indexed blob is; a `Write` of a blob not held is stored and durable. Mutant:
/// `Cache::find_missing` skips the set (the write would store it).
#[tokio::test]
async fn a_write_of_a_held_blob_is_acknowledged_without_a_second_copy() {
    let (held, fresh) = (blob(17, 100), blob(18, 100));
    let cache = cache_with(&[&held], &[]).await;
    let remote = node(&cache);

    let digest = remote.put(held.bytes.clone()).await.expect("acknowledged");
    assert_eq!(digest, held.digest());
    assert_eq!(
        cache.durable(&[held.digest()]).await.expect("durable"),
        [held.digest()],
        "nothing was stored: the earlier writer's drain writes it"
    );

    remote.put(fresh.bytes.clone()).await.expect("stored");
    assert_eq!(cache.durable(&[fresh.digest()]).await.expect("durable"), []);
}
