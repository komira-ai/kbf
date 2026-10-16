//! [`Backend`] over a cache in this process and over a [`Storage`]: the same answers,
//! and the input check over a storage node.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use bytes::Bytes;
use kbf_meta::{ActionRecord, Role};
use kbf_proto::reapi::{ActionResult, Directory, DirectoryNode, FileNode};
use kbf_proto::worker::segments_server::Segments;
use kbf_storage::{Answer, PutStream, ReadStream, Storage, StorageError};
use kbf_types::Digest;
use prost::Message as _;

use super::Backend;
use crate::cache::{Cache, VerifiedBlob};
use crate::meta_log::MemoryMetaLog;
use crate::{CacheStorage, SegmentsConfig};

type Mem = Cache<MemoryMetaLog, kbf_objstore::MemoryStore>;

fn proto(d: &Digest) -> kbf_proto::reapi::Digest {
    crate::digest_to_proto(d)
}

fn blob(text: &str) -> (Digest, Bytes) {
    let bytes = Bytes::from(text.to_owned());
    (kbf_segments::sha256(&bytes), bytes)
}

fn dir(files: &[Digest], dirs: &[Digest]) -> (Digest, Bytes) {
    let message = Directory {
        files: files
            .iter()
            .map(|d| FileNode {
                name: d.hash_hex(),
                digest: Some(proto(d)),
                ..FileNode::default()
            })
            .collect(),
        directories: dirs
            .iter()
            .map(|d| DirectoryNode {
                name: d.hash_hex(),
                digest: Some(proto(d)),
            })
            .collect(),
        ..Directory::default()
    };
    let bytes = Bytes::from(message.encode_to_vec());
    (kbf_segments::sha256(&bytes), bytes)
}

async fn put(cache: &Mem, blobs: &[&(Digest, Bytes)]) {
    let blobs = blobs
        .iter()
        .map(|(d, b)| VerifiedBlob::new(*d, b.clone()).expect("verified"))
        .collect();
    cache.store_blobs(blobs).await.expect("stored");
}

/// A cache, and a backend of each kind over it: in this process, and through a storage.
fn both(cache: &Arc<Mem>) -> (Backend, Backend) {
    let storage = CacheStorage::new(Arc::clone(cache), SegmentsConfig::DEFAULT);
    (
        Backend::from(Arc::clone(cache)),
        Backend::remote(Arc::new(storage)),
    )
}

/// What a [`Spy`] gets wrong.
#[derive(Clone, Copy)]
enum Fault {
    /// Nothing.
    None,
    /// Reading this digest says it is held but cannot be read.
    Unreadable(Digest),
    /// Reading this digest fails in the store.
    ReadFails(Digest),
    /// Every FindMissing fails.
    AskFails,
}

/// A [`Storage`] over another, that counts FindMissing calls and gets one thing wrong.
struct Spy {
    inner: Arc<dyn Storage>,
    asks: AtomicUsize,
    fault: Fault,
}

impl Storage for Spy {
    fn find_missing<'a>(&'a self, digests: &'a [Digest]) -> Answer<'a, Vec<Digest>> {
        self.asks.fetch_add(1, Ordering::SeqCst);
        if matches!(self.fault, Fault::AskFails) {
            return Box::pin(async { Err(StorageError::Internal("down".to_owned())) });
        }
        self.inner.find_missing(digests)
    }

    fn read_blob<'a>(&'a self, digest: &'a Digest) -> Answer<'a, Bytes> {
        match self.fault {
            Fault::Unreadable(d) if d == *digest => {
                Box::pin(async move { Err(StorageError::Unreachable(*digest)) })
            }
            Fault::ReadFails(d) if d == *digest => {
                Box::pin(async { Err(StorageError::Internal("disk".to_owned())) })
            }
            _ => self.inner.read_blob(digest),
        }
    }

    fn read_range<'a>(&'a self, d: &'a Digest, offset: u64, limit: u64) -> Answer<'a, ReadStream> {
        self.inner.read_range(d, offset, limit)
    }

    fn put(&self, bytes: Bytes) -> Answer<'_, Digest> {
        self.inner.put(bytes)
    }

    fn stream(&self, size: u64) -> Option<Box<dyn PutStream>> {
        self.inner.stream(size)
    }

    fn put_streamed(
        &self,
        size: u64,
        bytes: futures::stream::BoxStream<'static, std::io::Result<Bytes>>,
        claim: futures::future::BoxFuture<'static, Option<Digest>>,
    ) -> Answer<'_, Option<Digest>> {
        self.inner.put_streamed(size, bytes, claim)
    }

    fn segments(&self) -> Arc<dyn Segments> {
        self.inner.segments()
    }

    fn action_result<'a>(&'a self, a: &'a Digest) -> Answer<'a, Option<(Digest, ActionResult)>> {
        self.inner.action_result(a)
    }

    fn prepare_action_result<'a>(&'a self, r: &'a ActionResult) -> Answer<'a, ActionRecord> {
        self.inner.prepare_action_result(r)
    }

    fn commit_action_record(&self, role: Role, a: Digest, r: ActionRecord) -> Answer<'_, ()> {
        self.inner.commit_action_record(role, a, r)
    }
}

fn spy(cache: &Arc<Mem>, fault: Fault) -> (Backend, Arc<Spy>) {
    let inner = Arc::new(CacheStorage::new(
        Arc::clone(cache),
        SegmentsConfig::DEFAULT,
    ));
    let spy = Arc::new(Spy {
        inner,
        asks: AtomicUsize::new(0),
        fault,
    });
    (Backend::remote(Arc::clone(&spy) as Arc<dyn Storage>), spy)
}

/// Catches: the input check over a storage node answering differently from the cache's
/// own: a missing file, a missing directory (nothing under it is looked at) and a
/// directory reached twice must give the same absent digests, in the same order
/// (directories first), and the same count of directories read.
#[tokio::test]
async fn the_walk_over_storage_finds_what_the_cache_walk_finds() {
    let cache = Arc::new(Cache::memory());
    let (held, lost) = (blob("held"), blob("lost"));
    let shared = dir(&[held.0], &[]);
    let (gone_file, gone_dir) = (blob("never stored"), dir(&[lost.0], &[]));
    let d1 = dir(&[held.0, lost.0], &[shared.0]);
    let d2 = dir(&[gone_file.0], &[shared.0, gone_dir.0]);
    let root = dir(&[held.0], &[d1.0, d2.0]);
    put(&cache, &[&held, &shared, &d1, &d2, &root]).await;
    let (local, remote) = both(&cache);

    let want = local.walk_inputs(root.0).await.expect("cache walk");
    let got = remote.walk_inputs(root.0).await.expect("storage walk");
    assert_eq!(got.absent, want.absent);
    assert_eq!(got.absent.len(), 3, "{:?}", got.absent);
    assert_eq!(got.absent[0], gone_dir.0, "directories come first");
    assert_eq!(got.dirs_read, want.dirs_read);
    assert_eq!(got.dirs_read, 4, "root, d1, d2 and the shared one once");

    put(&cache, &[&lost, &gone_file, &gone_dir]).await;
    assert!(
        remote
            .walk_inputs(root.0)
            .await
            .expect("walk")
            .absent
            .is_empty()
    );
}

/// Catches: a directory the node holds but cannot read stopping the Execute with an
/// error (it must be reported absent so the client uploads it again), a store failure
/// reading one reported as absent (the client would upload what the node holds), a
/// directory that does not decode passing as an empty one, and a node that cannot answer
/// FindMissing reported as "nothing missing".
#[tokio::test]
async fn the_walk_over_storage_reports_what_it_cannot_read() {
    let cache = Arc::new(Cache::memory());
    let file = blob("file");
    let leaf = dir(&[file.0], &[]);
    let root = dir(&[], &[leaf.0]);
    put(&cache, &[&file, &leaf, &root]).await;

    let (remote, _) = spy(&cache, Fault::Unreadable(leaf.0));
    let walked = remote.walk_inputs(root.0).await.expect("walk");
    assert_eq!(walked.absent, vec![leaf.0]);

    let junk = blob("not a Directory message \u{ff}\u{ff}");
    put(&cache, &[&junk]).await;
    let (remote, _) = spy(&cache, Fault::None);
    let e = remote
        .walk_inputs(junk.0)
        .await
        .expect_err("does not decode");
    assert!(matches!(e, StorageError::Invalid(m) if m.contains("does not decode")));

    let (remote, _) = spy(&cache, Fault::ReadFails(leaf.0));
    let e = remote.walk_inputs(root.0).await.expect_err("a read fails");
    assert!(matches!(e, StorageError::Internal(m) if m == "disk"));

    let (remote, _) = spy(&cache, Fault::AskFails);
    let e = remote.walk_inputs(root.0).await.expect_err("node down");
    assert!(matches!(e, StorageError::Internal(m) if m == "down"));
}

/// Catches: every file of a tree named in one FindMissing (a tree of many thousand files
/// overflowing the message limit): 4097 files go in two calls (at most 4096 digests,
/// about 300 KiB, in the first), and all of them are asked about.
#[tokio::test]
async fn files_are_asked_about_in_batches() {
    let cache = Arc::new(Cache::memory());
    let files: Vec<Digest> = (0..4097)
        .map(|n| kbf_segments::sha256(n.to_string().as_bytes()))
        .collect();
    let root = dir(&files, &[]);
    put(&cache, &[&root]).await;
    let (remote, spy) = spy(&cache, Fault::None);
    let walked = remote.walk_inputs(root.0).await.expect("walk");
    assert_eq!(walked.absent.len(), files.len());
    assert_eq!(spy.asks.load(Ordering::SeqCst), 2);
}

/// Catches: a streamed put over the cache's storage that does not land the blob where
/// FindMissing and reads see it, or that answers another digest than the one claimed.
#[tokio::test]
async fn a_streamed_put_over_storage_is_found_and_read_back() {
    use futures::StreamExt as _;
    let cache = Arc::new(Cache::memory());
    let (_, spy) = spy(&cache, Fault::None);
    let (digest, bytes) = blob("streamed");
    let size = bytes.len() as u64;
    let pieces = futures::stream::iter([Ok(bytes.slice(..3)), Ok(bytes.slice(3..))]).boxed();
    let claim = Box::pin(async move { Some(digest) });
    let stored = spy.put_streamed(size, pieces, claim).await.expect("put");
    assert_eq!(stored, Some(digest));
    assert!(spy.find_missing(&[digest]).await.expect("find").is_empty());
    assert_eq!(spy.read_blob(&digest).await.expect("read"), bytes);
}

/// Catches: the two kinds answering a call differently: the action-cache write (both
/// halves, as a daemon's result) and read, blob reads, FindMissing, and the leadership
/// and metrics each reports (a cache in memory has no log to lead; the metrics render).
#[tokio::test]
async fn both_kinds_answer_the_scheduler_calls_alike() {
    for remote in [false, true] {
        let cache = Arc::new(Cache::memory());
        let (local, through) = both(&cache);
        let backend = if remote { through } else { local };
        let (digest, bytes) = blob("an action");
        put(&cache, &[&(digest, bytes.clone())]).await;
        assert_eq!(backend.read_blob(&digest).await.expect("held"), bytes);
        let absent = blob("absent").0;
        assert_eq!(
            backend.find_missing(&[digest, absent]).await.expect("ask"),
            vec![absent]
        );
        assert!(matches!(
            backend.read_blob(&absent).await,
            Err(StorageError::NotFound(_))
        ));

        assert!(
            backend
                .action_result(&digest)
                .await
                .expect("miss")
                .is_none()
        );
        let record = backend
            .prepare_action_result(&ActionResult::default())
            .await
            .expect("prepared");
        backend
            .commit_action_record(digest, record)
            .await
            .expect("committed");
        let (_, hit) = backend
            .action_result(&digest)
            .await
            .expect("lookup")
            .expect("a hit");
        assert_eq!(hit, ActionResult::default());

        assert!(backend.leadership().is_none());
        assert!(backend.render_metrics().contains("kbf_"), "{remote}");
        assert_eq!(format!("{backend:?}"), "Backend(..)");
    }
}
