//! A tree's writes over the frontend's own packs ([`Packs`]), with every one of the pack store's
//! upload slots (`--pack-store-writes`) held by an upload whose body has not come: the tree's
//! manifests and root, and a tree list, still reach the store, in slots of their own, while the
//! uploads hold theirs and one more upload waits for a slot.
//!
//! The uploads are streamed writes through the packs, as the upload of a blob past the packs'
//! bound is: each takes its slot before it reads its body, and holds it until the body comes. The
//! tree's write is polled by hand: the farm and the packs' store answer every call from memory
//! when polled, so a write that waits for a slot stays pending however often it is polled, and
//! the test says so instead of hanging. The input roots hold only symlinks, so a tree writes no
//! blob (a blob written whole shares the uploads' slots).

use std::num::NonZeroUsize;
use std::pin::Pin;
use std::task::Poll;

use futures::{Future, poll};
use kbf_front::{PackSettings, Packs};
use kbf_objstore::{Capabilities, MemoryStore, ObjectStore as _};
use kbf_pack::{PackKeys, WriterKind};
use kbf_pack_index::PackIndex;
use kbf_pack_index::memory::MemoryPackIndex;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

use super::listing::listed_base;
use super::memo::{holding, link_root};
use super::*;

/// The pack store's upload slots in these tests.
const UPLOAD_SLOTS: usize = 2;

/// The most times a tree's write is polled before it is taken to be waiting for a slot. A write
/// that waits for nothing ends on its first poll or soon after; one that waits for a slot never
/// does.
const POLLS: usize = 64;

/// An upload through the packs that holds its slot: its body comes once `release` is sent.
struct Held {
    release: oneshot::Sender<()>,
    upload: JoinHandle<Result<(), ObjectStoreError>>,
}

/// The rig's farm as the tree's source, the packs over `memory` as its store, every upload slot
/// held.
struct Packed {
    memory: Arc<MemoryStore>,
    packs: Packs,
    held: Vec<Held>,
}

impl Packed {
    /// Packs of [`UPLOAD_SLOTS`] upload slots over an empty create-only memory store.
    fn new() -> Self {
        let memory = Arc::new(MemoryStore::new(Capabilities {
            conditional_put: true,
            object_lock: false,
        }));
        let clock = Arc::new(kbf_state::ManualClock::default());
        let index: Arc<dyn PackIndex> = Arc::new(MemoryPackIndex::new(clock));
        let keys = PackKeys::new(WriterKind::Frontend, "fe1", "boot1").expect("keys");
        let settings = PackSettings {
            store_writes: NonZeroUsize::new(UPLOAD_SLOTS).expect("not zero"),
            ..PackSettings::default()
        };
        let packs = Packs::new(Arc::clone(&memory), index, keys, settings).expect("packs");
        Self {
            memory,
            packs,
            held: Vec::new(),
        }
    }

    /// Starts the upload `name`, a streamed write through the packs whose one-byte body comes
    /// once its release is sent; answers with it and a signal that it has taken its slot (its
    /// body was asked for).
    fn upload(&self, name: &str) -> (Held, oneshot::Receiver<()>) {
        let (release, released) = oneshot::channel::<()>();
        let (took, taken) = oneshot::channel::<()>();
        let body = stream::once(async move {
            let _ = took.send(());
            let _ = released.await;
            Ok(Bytes::from_static(b"u"))
        });
        let key = ObjectKey::new(format!("cas/f/w/{name}")).expect("a key");
        let packs = self.packs.clone();
        let upload =
            tokio::spawn(async move { LargeStore::put(&packs, &key, 1, body.boxed()).await });
        (Held { release, upload }, taken)
    }

    /// Takes every upload slot, each with an upload whose body has not come, and starts one more
    /// upload, which must wait for a slot; answers with that one.
    async fn hold_every_upload_slot(&mut self) -> (Held, oneshot::Receiver<()>) {
        for n in 0..UPLOAD_SLOTS {
            let (held, taken) = self.upload(&format!("held{n}"));
            taken.await.expect("the upload takes a slot");
            self.held.push(held);
        }
        let (waiting, mut taken) = self.upload("waiting");
        for _ in 0..POLLS {
            tokio::task::yield_now().await;
        }
        assert!(
            taken.try_recv().is_err(),
            "an upload past the upload slots reached the store"
        );
        (waiting, taken)
    }

    /// A writer of trees from the rig's farm to these packs.
    fn writer(&self, rig: &Rig) -> TreeWriter {
        let source = Arc::clone(&rig.farm) as Arc<dyn BlobSource>;
        let store: Arc<dyn LargeStore> = Arc::new(self.packs.clone());
        let clock: kbf_front::Clock = Arc::new(|| UNIX_EPOCH + Duration::from_secs(START_SECS));
        TreeWriter::new(source, store, clock)
    }

    /// Whether the store holds `key`.
    async fn holds(&self, key: &str) -> bool {
        let key = ObjectKey::new(key.to_owned()).expect("a key");
        self.memory.len(&key).await.expect("asked").is_some()
    }

    /// Lets every held upload's body come, then the waiting upload's, and checks each is stored.
    async fn release(mut self, waiting: Held, taken: oneshot::Receiver<()>) {
        for held in self.held.drain(..) {
            held.release
                .send(())
                .expect("the upload waits for its body");
            held.upload.await.expect("joined").expect("stored");
        }
        taken.await.expect("the waiting upload takes a freed slot");
        waiting
            .release
            .send(())
            .expect("the upload waits for its body");
        waiting.upload.await.expect("joined").expect("stored");
        for name in ["held0", "held1", "waiting"] {
            assert!(self.holds(&format!("cas/f/w/{name}")).await, "{name}");
        }
    }
}

/// `write` polled by hand, yielding between polls, until it ends or [`POLLS`] polls have left
/// it pending: its answer, or `None` when it is waiting for something that never comes.
async fn ended<T>(mut write: Pin<&mut impl Future<Output = T>>) -> Option<T> {
    for _ in 0..POLLS {
        if let Poll::Ready(answer) = poll!(write.as_mut()) {
            return Some(answer);
        }
        tokio::task::yield_now().await;
    }
    None
}

/// Catches a tree's manifests or root queued behind the uploads for one of the pack store's
/// slots: with both upload slots held by uploads whose bodies have not come and a third upload
/// waiting for a slot, a tree of two directories is written over the packs (its three manifests,
/// `.kbf` among them, then its root), and the store holds its root while every upload still
/// waits; the uploads are stored once their bodies come.
#[tokio::test]
async fn a_trees_root_reaches_the_store_while_every_upload_slot_is_held() {
    let rig = Rig::new().await;
    let leaf = link_root(&rig, "a").await;
    let kept = rig.kept(&holding(&rig, "z", &leaf).await).await;
    let mut packed = Packed::new();
    let (waiting, taken) = packed.hold_every_upload_slot().await;
    let writer = packed.writer(&rig);

    let mut write = std::pin::pin!(writer.write(&kept, &rig.base.tree));
    let tree = ended(write.as_mut()).await;

    let tree = tree.expect("the tree's writes waited behind the uploads");
    let tree = tree.expect("a tree");
    assert!(packed.holds(&whole_key(&tree.digest().to_string())).await);
    assert!(packed.held.iter().all(|held| !held.upload.is_finished()));
    packed.release(waiting, taken).await;
}

/// Catches a tree list queued behind the uploads for one of the pack store's slots: with both
/// upload slots held and a third upload waiting, an Action's tree list is written over the packs,
/// and the store holds the list while every upload still waits; the uploads are stored once their
/// bodies come.
#[tokio::test]
async fn a_tree_list_reaches_the_store_while_every_upload_slot_is_held() {
    let rig = Rig::new().await;
    let kept = rig.kept(&link_root(&rig, "a").await).await;
    let base = listed_base(&rig);
    let mut packed = Packed::new();
    let (waiting, taken) = packed.hold_every_upload_slot().await;
    let writer = packed.writer(&rig);

    let mut write = std::pin::pin!(writer.write_listing(&kept, &base));
    let listing = ended(write.as_mut()).await;

    let listing = listing.expect("the list's write waited behind the uploads");
    let listing = listing.expect("a list");
    assert!(packed.holds(listing.key()).await);
    assert!(packed.held.iter().all(|held| !held.upload.is_finished()));
    packed.release(waiting, taken).await;
}
