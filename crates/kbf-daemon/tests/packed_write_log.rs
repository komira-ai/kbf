//! The plane's warning that a pack could not be stored. Alone in its test binary: a tracing
//! callsite that another test's thread reached first, with no subscriber installed, is
//! cached as uninteresting, and a subscriber installed after would miss it.

mod support;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use bytes::Bytes;
use futures::future::BoxFuture;
use kbf_daemon::cas::Cas as _;
use kbf_daemon::{PackCommit, PackReads, PackWriter, PackedCas, WriteSettings};
use kbf_objstore::{
    ByteRange, Capabilities, KeyPrefix, ListPage, ListToken, MemoryStore, ObjectKey, ObjectStore,
    ObjectStoreError, PageSize,
};
use kbf_pack::{Fetch, PackKeys, ReadError, WriterKind};
use kbf_pack_index::memory::MemoryPackIndex;
use kbf_pack_index::{BlobDigest, PackIndex};
use kbf_state::ManualClock;
use support::memory::MemoryCas;

/// A memory object store that refuses every put while `refuse` is set.
struct Flaky {
    inner: MemoryStore,
    refuse: AtomicBool,
}

impl ObjectStore for Flaky {
    fn capabilities(&self) -> Capabilities {
        self.inner.capabilities()
    }

    async fn put_new(
        &self,
        key: &ObjectKey,
        body: Bytes,
        retain_until: Option<SystemTime>,
    ) -> Result<(), ObjectStoreError> {
        if self.refuse.load(Ordering::SeqCst) {
            return Err(ObjectStoreError::Service {
                status: 500,
                code: "InternalError".to_owned(),
                message: "scripted".to_owned(),
            });
        }
        self.inner.put_new(key, body, retain_until).await
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

/// Packs that have no blob.
struct Empty;

impl PackReads for Empty {
    fn read_blobs<'a>(
        &'a self,
        digests: &'a [BlobDigest],
    ) -> BoxFuture<'a, Result<Vec<Fetch>, ReadError>> {
        Box::pin(std::future::ready(Ok(digests
            .iter()
            .map(|_| Fetch::Missing)
            .collect())))
    }
}

/// Asserts that a store that cannot take a pack is warned of at the first put that finds it so,
/// then only logged at debug level, and that the first failure after a put that worked is a
/// warning again; every put still returns the plane underneath's result. Four puts against a
/// store that fails, fails, works, fails log WARN, DEBUG, WARN for the three failures.
/// (A current-thread runtime, so the test's subscriber sees the plane's calls.)
#[tokio::test]
async fn a_store_that_fails_is_warned_of_once_until_a_put_works() {
    let log = Log::default();
    let _installed = tracing::subscriber::set_default(
        tracing_subscriber::fmt()
            .with_writer(log.clone())
            .with_ansi(false)
            .with_max_level(tracing::Level::DEBUG)
            .finish(),
    );
    let caps = Capabilities {
        conditional_put: true,
        object_lock: false,
    };
    let store = Arc::new(Flaky {
        inner: MemoryStore::new(caps),
        refuse: AtomicBool::new(true),
    });
    let index: Arc<dyn PackIndex> =
        Arc::new(MemoryPackIndex::new(Arc::new(ManualClock::default())));
    let keys = PackKeys::new(WriterKind::Daemon, "node1", "boot1").expect("keys");
    let writer = PackWriter::new(Arc::clone(&store), index, keys, WriteSettings::default());
    let commit = PackCommit::new();
    commit.welcomed(true);
    let underneath = Arc::new(MemoryCas::default());
    let cas = PackedCas::new(Arc::clone(&underneath), Arc::new(Empty))
        .with_writes(Arc::new(writer.expect("writer")), commit);

    for (bytes, refuse) in [
        (&b"one"[..], true),
        (&b"two"[..], true),
        (&b"three"[..], false),
        (&b"four"[..], true),
    ] {
        store.refuse.store(refuse, Ordering::SeqCst);
        cas.put(bytes.to_vec()).await.expect("put");
    }

    assert_eq!(
        log.levels("the object store failed a pack"),
        ["WARN", "DEBUG", "WARN"],
        "{}",
        log.text()
    );
    let stats = cas.stats().writes;
    assert_eq!((stats.packs, stats.fallbacks.store), (1, 3));
}

/// What a subscriber wrote, for the test above.
#[derive(Clone, Default)]
struct Log(Arc<Mutex<Vec<u8>>>);

impl Log {
    fn text(&self) -> String {
        String::from_utf8(self.0.lock().expect("lock").clone()).expect("UTF-8")
    }

    /// The levels of the lines that contain `needle`, in order.
    fn levels(&self, needle: &str) -> Vec<String> {
        let text = self.text();
        let lines = text.lines().filter(|line| line.contains(needle));
        lines
            .map(|line| line.split_whitespace().nth(1).expect("a level").to_owned())
            .collect()
    }
}

impl std::io::Write for Log {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().expect("lock").extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Log {
    type Writer = Self;

    fn make_writer(&'a self) -> Self {
        self.clone()
    }
}
