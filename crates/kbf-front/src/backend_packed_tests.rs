//! [`Backend`] given packs ([`Backend::remote_over`], [`Backend::routed_over`]): the blobs
//! Execute reads are found in the pack index first and on the nodes after, over one node and
//! over a map.

use std::time::Duration;

use kbf_objstore::{Capabilities, MemoryStore};
use kbf_pack::{PackKeys, WriterKind};
use kbf_state::pack_index::memory::MemoryPackIndex;
use kbf_state::{
    BlobDigest, Location, ManualClock, NewPack, PackIndex, PackIndexError, PackInfo, Recorded,
};

use super::*;
use crate::{PackSettings, Packs};

/// Packs over an in-memory store and index that hold `blobs`.
async fn packs_holding(blobs: &[&(Digest, Bytes)]) -> Packs {
    let create_only = Capabilities {
        conditional_put: true,
        object_lock: false,
    };
    let store = Arc::new(MemoryStore::new(create_only));
    let clock = Arc::new(ManualClock::default());
    let index: Arc<dyn PackIndex> = Arc::new(MemoryPackIndex::new(clock));
    let keys = PackKeys::new(WriterKind::Frontend, "fe1", "boot1").expect("keys");
    let made = Packs::new(store, index, keys, PackSettings::default());
    let packs = made.expect("packs");
    let blobs = blobs.iter().map(|(d, b)| (*d, b.clone())).collect();
    packs.put(blobs).await.expect("packed");
    packs
}

/// A pack index that answers nothing.
struct Down;

impl PackIndex for Down {
    fn record_pack(&self, _: &NewPack) -> Result<Recorded, PackIndexError> {
        Err(PackIndexError::NotMigrated)
    }

    fn pack(&self, _: &str) -> Result<Option<PackInfo>, PackIndexError> {
        Err(PackIndexError::NotMigrated)
    }

    fn locate(&self, _: &[BlobDigest]) -> Result<Vec<Option<Location>>, PackIndexError> {
        Err(PackIndexError::NotMigrated)
    }

    fn find_missing(&self, _: &[BlobDigest]) -> Result<Vec<BlobDigest>, PackIndexError> {
        Err(PackIndexError::NotMigrated)
    }

    fn touch(&self, _: &[BlobDigest]) -> Result<usize, PackIndexError> {
        Err(PackIndexError::NotMigrated)
    }

    fn dead_packs(&self, _: Duration, _: usize) -> Result<Vec<PackInfo>, PackIndexError> {
        Err(PackIndexError::NotMigrated)
    }

    fn unreferenced_keys(&self, _: &[String]) -> Result<Vec<String>, PackIndexError> {
        Err(PackIndexError::NotMigrated)
    }
}

/// Asserts that the store behind Execute looks in the packs of its frontend: over one node that
/// holds neither the blob nor the directory, every scheduler call is answered as it is over a
/// node that holds them (a read, FindMissing with a digest nobody holds, the walk of the
/// directory).
#[tokio::test]
async fn packs_in_front_of_one_node_answer_the_scheduler_calls_alike() {
    let cache = Arc::new(Cache::memory());
    let held = blob("an action held in a pack");
    let root = dir(&[], &[]);
    let packs = packs_holding(&[&held, &root]).await;
    let storage = CacheStorage::new(Arc::clone(&cache), SegmentsConfig::DEFAULT);
    let backend = Backend::remote_over(Arc::new(storage), Some(packs));

    scheduler_calls(&backend, held, root).await;
}

/// Asserts that a read, a FindMissing and a walk look on the node for what the packs lack: a
/// blob on the node alone, and a tree whose directory is in a pack and whose files are on the
/// node, are found; and FindMissing asks the node about no digest the packs hold.
#[tokio::test]
async fn what_the_packs_lack_is_asked_of_the_node() {
    let cache = Arc::new(Cache::memory());
    let (in_pack, on_node) = (blob("file in a pack"), blob("file on the node"));
    let root = dir(&[in_pack.0, on_node.0], &[]);
    put(&cache, &[&on_node]).await;
    let packs = packs_holding(&[&in_pack, &root]).await;
    let (backend, spy) = {
        let (_, spy) = spy(&cache, Fault::None);
        let storage: Arc<dyn Storage> = spy.clone();
        (Backend::remote_over(storage, Some(packs)), spy)
    };

    let walked = backend.walk_inputs(root.0).await.expect("walk");
    assert!(walked.absent.is_empty(), "{:?}", walked.absent);
    let read = backend.read_blob(&on_node.0).await;
    assert_eq!(read.expect("read"), on_node.1);
    let asks = spy.asks.load(Ordering::SeqCst);

    let none = backend.find_missing(&[in_pack.0, root.0]).await;
    assert!(none.expect("answered").is_empty());
    assert_eq!(spy.asks.load(Ordering::SeqCst), asks, "the node was asked");

    let nowhere = blob("held nowhere").0;
    let some = backend.find_missing(&[in_pack.0, on_node.0, nowhere]).await;
    assert_eq!(some.expect("answered"), vec![nowhere]);
    assert_eq!(spy.asks.load(Ordering::SeqCst), asks + 1);
}

/// Asserts that an index that cannot answer is not taken for "not in a pack": a read and a
/// FindMissing fail Unavailable.
#[tokio::test]
async fn an_index_that_cannot_answer_fails_the_call() {
    let keys = PackKeys::new(WriterKind::Frontend, "fe1", "boot1").expect("keys");
    let create_only = Capabilities {
        conditional_put: true,
        object_lock: false,
    };
    let store = Arc::new(MemoryStore::new(create_only));
    let made = Packs::new(store, Arc::new(Down), keys, PackSettings::default());
    let packs = made.expect("packs");
    let cache = Arc::new(Cache::memory());
    let storage = CacheStorage::new(cache, SegmentsConfig::DEFAULT);
    let backend = Backend::remote_over(Arc::new(storage), Some(packs));
    let digest = blob("anything").0;

    let read = backend.read_blob(&digest).await;
    let unavailable = matches!(read, Err(StorageError::Unavailable(_)));
    assert!(unavailable, "{read:?}");
    let asked = backend.find_missing(&[digest]).await;
    let unavailable = matches!(asked, Err(StorageError::Unavailable(_)));
    assert!(unavailable, "{asked:?}");
}

/// Asserts that the routed kind looks in the packs: over a map of three nodes that hold neither
/// the blob nor the directory, the scheduler calls are answered as they are over nodes that
/// hold them, and what is held on the node the map puts it on is found beside what is in a
/// pack, by the walk and by a read.
#[tokio::test]
async fn packs_in_front_of_a_map_answer_the_scheduler_calls_alike() {
    let trio = Arc::new(Trio::new());
    let held = blob("a routed action held in a pack");
    let root = dir(&[], &[]);
    let packs = packs_holding(&[&held, &root]).await;
    let routed = Arc::new(Routed::new(trio.clone(), trio.clone(), trio.clone()));
    let first = Arc::clone(&trio.nodes[&MemberId(1)]);
    let backend = Backend::routed_over(routed, first, Some(packs));

    scheduler_calls(&backend, held, root).await;

    let on_node = blob("a file on the node the map names");
    let tree = dir(&[on_node.0], &[]);
    trio.store(&on_node).await;
    let packs = packs_holding(&[&tree]).await;
    let routed = Arc::new(Routed::new(trio.clone(), trio.clone(), trio.clone()));
    let first = Arc::clone(&trio.nodes[&MemberId(1)]);
    let backend = Backend::routed_over(routed, first, Some(packs));
    let walked = backend.walk_inputs(tree.0).await.expect("walk");
    assert!(walked.absent.is_empty(), "{:?}", walked.absent);
    let read = backend.read_blob(&on_node.0).await;
    assert_eq!(read.expect("read"), on_node.1);
    let found = backend.find_missing(&[tree.0, on_node.0]).await;
    assert!(found.expect("answered").is_empty());
}

/// Asserts that the packs also front the `--storage` node's store while the map has fewer than
/// two members: the calls are the one node's, and the packs are looked in first.
#[tokio::test]
async fn packs_in_front_of_the_storage_node_while_the_map_is_small() {
    struct One(Trio);
    impl MapSource for One {
        fn current(&self) -> Option<MapView> {
            let member = self.0.map.members()[1].clone();
            let map = Map::new(1, vec![member], Rule::Rendezvous, 1, 1, Vec::new()).expect("map");
            Some(MapView {
                map: Arc::new(map),
                previous: None,
                earlier: Vec::new(),
            })
        }
        fn refetch(&self, at_least: u64) -> BoxFuture<'_, Result<MapView, RefetchError>> {
            self.0.refetch(at_least)
        }
    }
    let trio = Arc::new(Trio::new());
    let first = Arc::clone(&trio.nodes[&MemberId(1)]);
    let alone = Arc::new(One(Trio::new()));
    let routed = Arc::new(Routed::new(alone, trio.clone(), trio.clone()));
    assert!(!routed.active());
    let held = blob("held in a pack while the map is small");
    let root = dir(&[], &[]);
    let packs = packs_holding(&[&held, &root]).await;
    let backend = Backend::routed_over(routed, first, Some(packs));

    scheduler_calls(&backend, held, root).await;
}
