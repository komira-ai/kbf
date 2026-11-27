//! A walk that runs while the packs remove a row: between the loss guard's first count of a
//! loss and the end of the forget or drop it leads to, a walk can still find the row and record
//! its tree as held. The guard counts again once the removal has ended (`kbf_pack::LossGuard`),
//! so that walk ends under another epoch than it began, records nothing, and the next walk asks
//! the index again and finds the blob gone. One test per way this process removes a row: a pack
//! forgotten after its second absence, and a damaged blob's location dropped, by a read from a
//! pack and by a read of a large object.

use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, SystemTime};

use bytes::Bytes;
use futures::StreamExt as _;
use kbf_objstore::{Capabilities, MemoryStore, ObjectKey, ObjectStore as _};
use kbf_pack::{CONFIRM_AFTER, Loss, MIN_AGE, PackKeys, WriterKind};
use kbf_pack_index::action::ActionStore;
use kbf_pack_index::memory::MemoryPackIndex;
use kbf_pack_index::{
    BlobDigest, Forgot, Location, NewDirectory, NewPack, PackBlob, PackIndex, PackIndexError,
    PackInfo, Recorded,
};
use kbf_state::{Clock as _, ManualClock};
use kbf_types::Digest;

use super::tests::{blob, dir};
use super::*;
use crate::backend::Backend;
use crate::{PackSettings, Packs, large_key};

/// The two ends of one hold: told when the held call has begun, and what lets it go on.
struct Held {
    began: SyncSender<()>,
    go: Receiver<()>,
}

/// An in-memory pack index whose next `forget_pack` or `drop_location`, once armed, says it has
/// begun and waits to be let go before it removes anything.
struct Gate {
    inner: Arc<MemoryPackIndex>,
    hold: Mutex<Option<Held>>,
}

impl Gate {
    /// Arms the hold: the receiver hears the held call begin, the sender lets it go on.
    fn arm(&self) -> (Receiver<()>, SyncSender<()>) {
        let (began, begun) = sync_channel(1);
        let (release, go) = sync_channel(1);
        *self.hold.lock().unwrap_or_else(PoisonError::into_inner) = Some(Held { began, go });
        (begun, release)
    }

    fn wait(&self) {
        let held = self.hold.lock().unwrap_or_else(PoisonError::into_inner).take();
        if let Some(Held { began, go }) = held {
            began.send(()).expect("the test listens");
            go.recv().expect("the test lets it go");
        }
    }
}

impl PackIndex for Gate {
    fn record_pack(&self, pack: &NewPack) -> Result<Recorded, PackIndexError> {
        self.inner.record_pack(pack)
    }

    fn pack(&self, key: &str) -> Result<Option<PackInfo>, PackIndexError> {
        self.inner.pack(key)
    }

    fn locate(&self, digests: &[BlobDigest]) -> Result<Vec<Option<Location>>, PackIndexError> {
        self.inner.locate(digests)
    }

    fn find_missing(&self, digests: &[BlobDigest]) -> Result<Vec<BlobDigest>, PackIndexError> {
        self.inner.find_missing(digests)
    }

    fn touch(&self, digests: &[BlobDigest]) -> Result<usize, PackIndexError> {
        self.inner.touch(digests)
    }

    fn dead_packs(&self, grace: Duration, limit: usize) -> Result<Vec<PackInfo>, PackIndexError> {
        self.inner.dead_packs(grace, limit)
    }

    fn unreferenced_keys(&self, keys: &[String]) -> Result<Vec<String>, PackIndexError> {
        self.inner.unreferenced_keys(keys)
    }

    fn drop_location(&self, digest: &BlobDigest, pack_key: &str) -> Result<bool, PackIndexError> {
        self.wait();
        self.inner.drop_location(digest, pack_key)
    }

    fn forget_pack(&self, pack_key: &str, min_age: Duration) -> Result<Forgot, PackIndexError> {
        self.wait();
        self.inner.forget_pack(pack_key, min_age)
    }

    fn directories(&self, digests: &[BlobDigest]) -> Result<Vec<Option<Vec<u8>>>, PackIndexError> {
        self.inner.directories(digests)
    }

    fn record_directories(&self, directories: &[NewDirectory]) -> Result<usize, PackIndexError> {
        self.inner.record_directories(directories)
    }
}

/// A frontend with no storage node over a [`Gate`] and one manual clock, which the index and
/// the packs' loss guard both read; blobs over `max_blob_bytes` are large objects.
struct Rig {
    store: Arc<MemoryStore>,
    gate: Arc<Gate>,
    clock: Arc<ManualClock>,
    stored: Arc<Stored>,
    backend: Backend,
}

impl Rig {
    fn new(max_blob_bytes: u64) -> Self {
        let create_only = Capabilities {
            conditional_put: true,
            object_lock: false,
        };
        let store = Arc::new(MemoryStore::new(create_only));
        let clock = Arc::new(ManualClock::default());
        let inner = Arc::new(MemoryPackIndex::new(Arc::clone(&clock) as _));
        let gate = Arc::new(Gate {
            inner: Arc::clone(&inner),
            hold: Mutex::default(),
        });
        let keys = PackKeys::new(WriterKind::Frontend, "fe1", "boot1").expect("keys");
        let settings = PackSettings {
            max_blob_bytes,
            ..PackSettings::default()
        };
        let index: Arc<dyn PackIndex> = Arc::clone(&gate) as _;
        let wall = Arc::clone(&clock);
        let packs = Packs::new(Arc::clone(&store), index, keys, settings)
            .expect("packs")
            .with_clock(Arc::new(move || {
                SystemTime::UNIX_EPOCH + wall.now().as_duration()
            }));
        let actions: Arc<dyn ActionStore> = inner;
        let stored = Arc::new(Stored::over(packs, actions));
        let backend = Backend::stored(Arc::clone(&stored));
        Self {
            store,
            gate,
            clock,
            stored,
            backend,
        }
    }

    fn packs(&self) -> &Packs {
        self.stored.packs()
    }

    /// Records `blob` as the one blob of the pack `key`, and puts `body` under `key` in the
    /// store, unless `body` is `None`.
    async fn row(&self, key: &str, blob: &(Digest, Bytes), body: Option<Vec<u8>>) {
        let digest = BlobDigest {
            hash: blob.0.hash,
            size: blob.0.size_bytes,
        };
        let pack = NewPack {
            key: key.to_owned(),
            writer: "fe1".to_owned(),
            size: blob.0.size_bytes,
            blobs: vec![PackBlob { digest, offset: 0 }],
        };
        self.gate.record_pack(&pack).expect("recorded");
        if let Some(body) = body {
            let key = ObjectKey::new(key).expect("a key");
            self.store
                .put_new(&key, Bytes::from(body), None)
                .await
                .expect("stored");
        }
    }

    /// A root Directory naming `file`, uploaded through the packs (so its walk reads nothing);
    /// a first walk finds the tree held, which is remembered.
    async fn tree_of(&self, file: &(Digest, Bytes)) -> Digest {
        let root = dir(&[file.0], &[]);
        self.packs().put(vec![root.clone()]).await.expect("packed");
        let first = self.backend.walk_inputs(root.0).await.expect("walked");
        assert!(first.absent.is_empty(), "{:?}", first.absent);
        root.0
    }

    /// Runs `removal` (which reaches the gate), and a walk of `root` while the gate holds it;
    /// then lets the removal end, and walks `root` again: what the walk in the gap found, and
    /// what the next walk found.
    async fn walk_across<F>(&self, root: Digest, removal: F) -> (Walked, Walked)
    where
        F: std::future::Future<Output = ()> + Send + 'static,
    {
        let (begun, release) = self.gate.arm();
        let removing = tokio::spawn(removal);
        tokio::task::spawn_blocking(move || begun.recv())
            .await
            .expect("joined")
            .expect("the removal reached the index");
        let during = self.backend.walk_inputs(root).await.expect("walked");
        release.send(()).expect("the removal waits");
        removing.await.expect("the removal ended");
        let after = self.backend.walk_inputs(root).await.expect("walked");
        (during, after)
    }
}

/// Catches a walk that records a tree as held while its pack is being forgotten: the pack of the
/// file has gone from the store; after its first absence is noted, the second, some time later,
/// forgets the pack, and a walk runs while the forget is under way (it still finds the row). The
/// walk after the forget does not skip the tree on that walk's word: it asks, and finds the file
/// absent.
#[tokio::test]
async fn a_walk_during_a_forget_is_not_believed_after_it() {
    let rig = Rig::new(PackSettings::default().max_blob_bytes);
    let file = blob("a file whose pack the store lost");
    let key = "cas/f/fe1/gone/0000";
    rig.row(key, &file, None).await;
    let root = rig.tree_of(&file).await;
    rig.clock.advance(MIN_AGE);
    let first = rig.packs().absent(key, 0).await;
    rig.clock.advance(CONFIRM_AFTER);

    let packs = rig.packs().clone();
    let forget = async move {
        let second = packs.absent(key, 0).await;
        assert_eq!(second, Loss::Forgotten { rows: 1 });
    };
    let (during, after) = rig.walk_across(root, forget).await;

    assert_eq!(first, Loss::Noted);
    assert!(during.absent.is_empty(), "the row was still there");
    assert_eq!(after.absent, vec![file.0]);
    assert_eq!(after.dirs_skipped, 0);
}

/// Catches a walk that records a tree as held while a damaged blob's location is being dropped:
/// the pack holds other bytes than the file's digest; a read of the file finds that and drops
/// its location, and a walk runs while the drop is under way. The walk after the drop asks, and
/// finds the file absent.
#[tokio::test]
async fn a_walk_during_a_damaged_read_is_not_believed_after_it() {
    let rig = Rig::new(PackSettings::default().max_blob_bytes);
    let file = blob("a file whose pack holds other bytes");
    let key = "cas/f/fe1/bad/0000";
    rig.row(key, &file, Some(vec![b'X'; file.1.len()])).await;
    let root = rig.tree_of(&file).await;

    let (packs, digest) = (rig.packs().clone(), file.0);
    let read = async move {
        let refused = packs.read(&digest).await;
        assert!(refused.is_err(), "not served: {refused:?}");
    };
    let (during, after) = rig.walk_across(root, read).await;

    assert!(during.absent.is_empty(), "the row was still there");
    assert_eq!(after.absent, vec![file.0]);
    assert_eq!(after.dirs_skipped, 0);
}

/// Catches the same through a large object: a blob over the packs' bound is stored as its own
/// object, which holds other bytes than its digest; a whole read of it drops its location, a walk
/// runs while the drop is under way, and the walk after it finds the blob absent.
#[tokio::test]
async fn a_walk_during_a_damaged_large_read_is_not_believed_after_it() {
    let rig = Rig::new(1024);
    let large = blob(&"a large blob ".repeat(200));
    let size = large.0.size_bytes;
    assert!(size > 1024, "large");
    let key = large_key(&large.0);
    rig.row(&key, &large, Some(vec![b'X'; large.1.len()])).await;
    let root = rig.tree_of(&large).await;

    let (packs, digest) = (rig.packs().clone(), large.0);
    let read = async move {
        let windows = packs.read_large(&digest, 0, size).await.expect("located");
        let windows: Vec<_> = windows.expect("a row").collect().await;
        assert!(windows.iter().any(Result::is_err), "not served");
    };
    let (during, after) = rig.walk_across(root, read).await;

    assert!(during.absent.is_empty(), "the row was still there");
    assert_eq!(after.absent, vec![large.0]);
    assert_eq!(after.dirs_skipped, 0);
}
