//! A blob streamed straight to the object store, which refuses it: the caller gets the
//! store's own error (its kind and its message), not a note that the upload stopped.
//! The store here is an in-memory fake that refuses on cue, so no test depends on a
//! disk or on timing.

use std::sync::Arc;

use bytes::Bytes;
use futures::StreamExt as _;
use kbf_front::{Cache, CacheError, CacheStorage, MemoryMetaLog, SegmentsConfig};
use kbf_meta::Retention;
use kbf_objstore::{
    BodyStream, ByteRange, Capabilities, KeyPrefix, ListPage, ListToken, MemoryStore, ObjectKey,
    ObjectStore, ObjectStoreError, PageSize,
};
use kbf_segments::RecordHasher;
use kbf_storage::{CommitMode, Storage, StorageError};
use tonic::{Code, Status};

const CHUNK: usize = 64 << 10;
const CHUNKS: usize = 16;
const FULL: &str =
    "cold/objects: 100 bytes free, under the floor of 1000 bytes plus the 7 bytes of this write";

/// What the fake store's streamed write does.
#[derive(Clone, Copy)]
enum Plan {
    /// Takes the object.
    Take,
    /// Refuses before reading a byte, as a store with no room does at `begin`.
    Refuse,
    /// Reads this many chunks, then fails.
    FailAfter(usize),
}

/// A store whose streamed writes follow a [`Plan`]; everything else is a [`MemoryStore`].
struct Fake {
    inner: MemoryStore,
    plan: Plan,
}

fn no_room() -> ObjectStoreError {
    ObjectStoreError::Transport(Arc::new(std::io::Error::other(FULL)))
}

impl ObjectStore for Fake {
    fn capabilities(&self) -> Capabilities {
        self.inner.capabilities()
    }

    async fn put_new(
        &self,
        key: &ObjectKey,
        body: Bytes,
        retain_until: Option<std::time::SystemTime>,
    ) -> Result<(), ObjectStoreError> {
        self.inner.put_new(key, body, retain_until).await
    }

    async fn put_stream(
        &self,
        key: &ObjectKey,
        len: u64,
        mut body: BodyStream,
    ) -> Result<(), ObjectStoreError> {
        match self.plan {
            Plan::Take => {
                let bytes = kbf_objstore::gather(body, len).await?;
                self.inner.put_new(key, bytes, None).await
            }
            Plan::Refuse => Err(no_room()),
            Plan::FailAfter(n) => {
                for _ in 0..n {
                    body.next().await;
                }
                Err(no_room())
            }
        }
    }

    async fn get_range(
        &self,
        key: &ObjectKey,
        range: ByteRange,
    ) -> Result<Bytes, ObjectStoreError> {
        self.inner.get_range(key, range).await
    }

    async fn delete(&self, key: &ObjectKey) -> Result<(), ObjectStoreError> {
        self.inner.delete(key).await
    }

    async fn list(
        &self,
        prefix: &KeyPrefix,
        after: Option<&ListToken>,
        max_keys: PageSize,
    ) -> Result<ListPage, ObjectStoreError> {
        self.inner.list(prefix, after, max_keys).await
    }
}

type FakeCache = Cache<MemoryMetaLog, Fake>;

/// A cache with no write tier over a store that follows `plan`, made on this runtime.
async fn cache(plan: Plan) -> Arc<FakeCache> {
    let store = Fake {
        inner: MemoryStore::new(Capabilities::default()),
        plan,
    };
    let meta = MemoryMetaLog::new(Retention::default());
    let cache = Cache::open(meta, store, KeyPrefix::default())
        .await
        .expect("open");
    Arc::new(cache)
}

fn blob() -> Vec<u8> {
    (0..CHUNK * CHUNKS).map(|i| (i * 31 % 251) as u8).collect()
}

/// Appends the blob a chunk at a time off the runtime, answering the first append that
/// failed, and the chunks it took before.
fn first_refusal(cache: &Arc<FakeCache>) -> (usize, Option<CacheError>) {
    let bytes = blob();
    let mut stream = cache.stream(bytes.len() as u64).expect("a stream");
    for (n, chunk) in bytes.chunks(CHUNK).enumerate() {
        if let Err(e) = stream.append(chunk) {
            return (n, Some(e));
        }
    }
    (CHUNKS, None)
}

/// Catches: an append that finds the upload gone saying only that the store stopped,
/// which hides that the store was full. A store refusing at `begin` is `Store` with its
/// own text, and a gRPC `Unavailable` (so a client retries), as the buffered path is.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refused_begin_gives_the_stores_error() {
    let cache = cache(Plan::Refuse).await;
    let (_, refusal) = tokio::task::spawn_blocking(move || first_refusal(&cache))
        .await
        .expect("joined");
    let err = refusal.expect("the append that finds the upload gone");
    assert!(matches!(err, CacheError::Store(_)), "{err:?}");
    assert!(err.to_string().contains(FULL), "{err}");
    let status = Status::from(err);
    assert_eq!(status.code(), Code::Unavailable, "{status:?}");
    assert!(status.message().contains(FULL), "{status:?}");
}

/// Catches: the store's error lost when it fails after taking some chunks.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_store_failing_mid_stream_gives_its_own_error() {
    let cache = cache(Plan::FailAfter(2)).await;
    let (taken, refusal) = tokio::task::spawn_blocking(move || first_refusal(&cache))
        .await
        .expect("joined");
    assert!(taken >= 2, "{taken} chunks taken before the failure");
    let err = refusal.expect("the append that finds the upload gone");
    assert!(matches!(err, CacheError::Store(_)), "{err:?}");
    assert!(err.to_string().contains(FULL), "{err}");
}

/// Catches: a second append after the refusal polling a finished task (a panic), or
/// answering something other than the same error.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn appends_after_the_refusal_repeat_it() {
    let cache = cache(Plan::Refuse).await;
    let (first, second) = tokio::task::spawn_blocking(move || {
        let bytes = blob();
        let mut stream = cache.stream(bytes.len() as u64).expect("a stream");
        let mut errors = bytes.chunks(CHUNK).filter_map(|c| stream.append(c).err());
        let first = errors.next().expect("a refusal");
        let second = errors.next().expect("the refusal again");
        (first.to_string(), second.to_string())
    })
    .await
    .expect("joined");
    assert_eq!(first, second);
    assert!(first.contains(FULL), "{first}");
}

/// Catches: an empty append after the last byte, once the upload has ended, consuming
/// the upload task so that a caller who ignored the error and went on to finish and
/// commit panics polling it again. The commit answers the refusal.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn commit_after_a_refused_empty_append_answers_the_refusal() {
    // The store reads the one chunk and fails, so the chunk is always taken and the
    // stream is complete when the upload ends.
    let cache = cache(Plan::FailAfter(1)).await;
    let bytes = vec![7u8; CHUNK];
    let mut hasher = RecordHasher::new();
    hasher.update(&bytes);
    let (digest, crc32c) = hasher.finish();
    let streamed = tokio::task::spawn_blocking(move || {
        let mut stream = cache.stream(bytes.len() as u64).expect("a stream");
        stream.append(&bytes).expect("the chunk is taken");
        // Empty appends queue until the upload ends, then fail with its error.
        while stream.append(&[]).is_ok() {}
        stream
            .finish(digest, crc32c)
            .expect("every byte was written")
    })
    .await
    .expect("joined");
    let err = streamed.commit().await.expect_err("the refusal");
    assert!(matches!(err, CacheError::Store(_)), "{err:?}");
    assert!(err.to_string().contains(FULL), "{err}");
}

/// Catches: the storage boundary turning the store's refusal into `Invalid`
/// (INVALID_ARGUMENT, which a client does not retry): it is `Unavailable`, carrying the
/// store's text.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_storage_boundary_answers_unavailable() {
    let cache = cache(Plan::Refuse).await;
    let storage: Arc<dyn Storage> = Arc::new(CacheStorage::new(cache, SegmentsConfig::DEFAULT));
    let err = tokio::task::spawn_blocking(move || {
        let bytes = blob();
        let mut stream = storage.stream(bytes.len() as u64).expect("a stream");
        bytes
            .chunks(CHUNK)
            .find_map(|c| stream.append(c).err())
            .expect("a refusal")
    })
    .await
    .expect("joined");
    assert!(matches!(err, StorageError::Unavailable(_)), "{err:?}");
    let status = Status::from(err);
    assert_eq!(status.code(), Code::Unavailable, "{status:?}");
    assert!(status.message().contains(FULL), "{status:?}");
}

/// A store that takes the blob: the stream appends, finishes and commits, and the blob is
/// held.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_store_that_takes_the_blob_still_commits_it() {
    let cache = cache(Plan::Take).await;
    let bytes = blob();
    let mut hasher = RecordHasher::new();
    hasher.update(&bytes);
    let (digest, crc32c) = hasher.finish();
    let storage: Arc<dyn Storage> = Arc::new(CacheStorage::new(
        Arc::clone(&cache),
        SegmentsConfig::DEFAULT,
    ));
    let streamed = tokio::task::spawn_blocking(move || {
        let mut stream = storage.stream(bytes.len() as u64).expect("a stream");
        for chunk in bytes.chunks(CHUNK) {
            stream.append(chunk).expect("append");
        }
        stream.finish(digest, crc32c).expect("finish")
    })
    .await
    .expect("joined");
    streamed.commit(CommitMode::Durable).await.expect("commit");
    assert!(
        cache
            .find_missing(&[digest])
            .await
            .expect("find")
            .is_empty()
    );
}
