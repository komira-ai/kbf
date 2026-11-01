//! [`Backend`] given packs ([`Backend::remote_over`], [`Backend::routed_over`]): the blobs
//! Execute reads are found in the pack index first and on the nodes after, over one node and
//! over a map.

use std::time::Duration;

use kbf_meta::{ActionWriteError, Closure};
use kbf_objstore::{Capabilities, MemoryStore};
use kbf_pack::{PackKeys, WriterKind};
use kbf_pack_index::memory::MemoryPackIndex;
use kbf_pack_index::{
    BlobDigest, Location, NewPack, PackIndex, PackIndexError, PackInfo, Recorded,
};
use kbf_proto::reapi::{OutputDirectory, OutputFile};
use kbf_state::ManualClock;
use prost::Message as _;

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
    let packed = blob("an output in a pack while the map is small");
    let on_node = blob("an output on the node while the map is small");
    first.put(on_node.1.clone()).await.expect("stored");
    let entry = naming(&lost().0).1;
    first.put(entry.1).await.expect("stored");
    let packs = packs_holding(&[&held, &root, &packed]).await;
    let backend = Backend::routed_over(routed, first, Some(packs));

    scheduler_calls(&backend, held, root).await;
    closure_calls(&backend, &packed, &on_node).await;
}

/// An output file of the blob `digest`.
fn output(digest: &Digest) -> OutputFile {
    OutputFile {
        path: digest.hash_hex(),
        digest: Some(proto(digest)),
        ..OutputFile::default()
    }
}

/// A result that names one output file, and the blob it is stored as.
fn naming(digest: &Digest) -> (ActionResult, (Digest, Bytes)) {
    let result = ActionResult {
        output_files: vec![output(digest)],
        ..ActionResult::default()
    };
    let bytes = Bytes::from(result.encode_to_vec());
    (result, (kbf_segments::sha256(&bytes), bytes))
}

/// The output that no pack and no node holds in [`closure_calls`].
fn lost() -> (Digest, Bytes) {
    blob("an output held nowhere")
}

/// The action-cache calls of `backend` with packs, over a farm whose nodes hold `on_node` and the
/// blob of the result `naming(&lost().0)`, and whose packs hold `packed`: a result with one
/// output of each is accepted and its entry is a hit; a result with an output held nowhere is
/// refused; and an entry that names an output held nowhere, which the nodes took as they check
/// only what the entry names, is a miss.
async fn closure_calls(backend: &Backend, packed: &(Digest, Bytes), on_node: &(Digest, Bytes)) {
    let result = ActionResult {
        output_files: vec![output(&packed.0), output(&on_node.0)],
        ..ActionResult::default()
    };
    let record = backend
        .prepare_action_result(&result)
        .await
        .expect("prepared");
    let wanted: Vec<Digest> = record.closure.iter().copied().collect();
    assert_eq!(wanted.len(), 2, "{wanted:?}");
    assert!(wanted.contains(&packed.0));
    let action = blob("an action with an output in a pack").0;
    backend
        .commit_action_record(action, record)
        .await
        .expect("committed");
    let (_, hit) = backend
        .action_result(&action)
        .await
        .expect("lookup")
        .expect("a hit");
    assert_eq!(hit, result);

    let lost = lost();
    let refused = backend
        .prepare_action_result(&naming(&lost.0).0)
        .await
        .unwrap_err();
    match refused {
        StorageError::ActionWrite(ActionWriteError::Absent(absent)) => {
            assert_eq!(absent, vec![lost.0]);
        }
        other => panic!("expected the output to be absent: {other:?}"),
    }

    let entry = naming(&lost.0).1;
    let record = ActionRecord {
        result: entry.0,
        closure: Closure::new(),
    };
    let action = blob("an action whose result names a lost output").0;
    backend
        .commit_action_record(action, record)
        .await
        .expect("committed");
    let miss = backend.action_result(&action).await.expect("lookup");
    assert!(miss.is_none());
}

/// Asserts that an output only in a pack satisfies the closure of an action-cache write and
/// of a hit over one node: the first half accepts a result with an output in a pack and one on
/// the node, the entry the node is handed names only the blob it holds (it would refuse the
/// other), and the hit is served while the index has the output.
#[tokio::test]
async fn an_output_only_in_a_pack_satisfies_the_closure_over_one_node() {
    let cache = Arc::new(Cache::memory());
    let (packed, on_node) = (blob("an output in a pack"), blob("an output on a node"));
    put(&cache, &[&on_node, &naming(&lost().0).1]).await;
    let packs = packs_holding(&[&packed]).await;
    let storage = CacheStorage::new(Arc::clone(&cache), SegmentsConfig::DEFAULT);
    let backend = Backend::remote_over(Arc::new(storage), Some(packs));

    closure_calls(&backend, &packed, &on_node).await;
}

/// Asserts the same over a map of three nodes: the closure is checked against the index and the
/// owners, and the entry committed at the action's owner names only the blobs the owners hold.
#[tokio::test]
async fn an_output_only_in_a_pack_satisfies_the_closure_over_a_map() {
    let trio = Arc::new(Trio::new());
    let (packed, on_node) = (blob("an output in a pack"), blob("an output on a node"));
    trio.store(&on_node).await;
    trio.store(&naming(&lost().0).1).await;
    let packs = packs_holding(&[&packed]).await;
    let routed = Arc::new(Routed::new(trio.clone(), trio.clone(), trio.clone()));
    let first = Arc::clone(&trio.nodes[&MemberId(1)]);
    let backend = Backend::routed_over(routed, first, Some(packs));

    closure_calls(&backend, &packed, &on_node).await;
}

/// Whether an answer is that the store could not say.
fn is_unavailable<T>(answer: &Result<T, StorageError>) -> bool {
    matches!(answer, Err(StorageError::Unavailable(_)))
}

/// Asserts that an index that cannot answer is not taken for "everything is held" by the
/// action-cache calls: the first half, the second half and the lookup of an entry the node holds
/// each fail as the store could not say, and nothing is committed or served.
#[tokio::test]
async fn an_index_that_cannot_answer_fails_the_action_cache_calls() {
    let cache = Arc::new(Cache::memory());
    let held = blob("an output on the node");
    put(&cache, &[&held]).await;
    let storage = CacheStorage::new(Arc::clone(&cache), SegmentsConfig::DEFAULT);
    let storage = Arc::new(storage);
    let result = naming(&held.0).0;
    let record = storage.prepare_action_result(&result).await;
    let record = record.expect("prepared by the node");
    let action = blob("an action the node holds").0;
    let kept = record.clone();
    let written = storage.commit_action_record(Role::Daemon, action, kept);
    written.await.expect("committed on the node");
    let keys = PackKeys::new(WriterKind::Frontend, "fe1", "boot1").expect("keys");
    let create_only = Capabilities {
        conditional_put: true,
        object_lock: false,
    };
    let store = Arc::new(MemoryStore::new(create_only));
    let down = Packs::new(store, Arc::new(Down), keys, PackSettings::default());
    let backend = Backend::remote_over(storage, Some(down.expect("packs")));

    let prepared = backend.prepare_action_result(&result).await;
    let committed = backend.commit_action_record(action, record).await;
    let looked = backend.action_result(&action).await;

    assert!(is_unavailable(&prepared));
    assert!(is_unavailable(&committed));
    assert!(is_unavailable(&looked));
}

/// Asserts that a result the closure check cannot read is refused with packs on as it is
/// without: a malformed digest and a tree that is not a `Tree` are invalid, and a tree that no
/// pack and no node holds is not found.
#[tokio::test]
async fn a_result_the_closure_check_cannot_read_is_refused_with_packs() {
    let cache = Arc::new(Cache::memory());
    let junk = blob("not a Tree \u{ff}\u{ff}");
    put(&cache, &[&junk]).await;
    let packs = packs_holding(&[&blob("something else")]).await;
    let storage = CacheStorage::new(Arc::clone(&cache), SegmentsConfig::DEFAULT);
    let backend = Backend::remote_over(Arc::new(storage), Some(packs));
    let malformed = ActionResult {
        output_files: vec![OutputFile {
            path: "f".to_owned(),
            digest: Some(kbf_proto::reapi::Digest {
                hash: "not hex".to_owned(),
                size_bytes: 1,
            }),
            ..OutputFile::default()
        }],
        ..ActionResult::default()
    };
    let tree_result = |tree: &Digest| ActionResult {
        output_directories: vec![OutputDirectory {
            path: "d".to_owned(),
            tree_digest: Some(proto(tree)),
            ..OutputDirectory::default()
        }],
        ..ActionResult::default()
    };

    let invalid = backend.prepare_action_result(&malformed).await;
    let invalid = invalid.unwrap_err();
    assert!(matches!(invalid, StorageError::Invalid(_)), "{invalid:?}");
    let undecoded = backend.prepare_action_result(&tree_result(&junk.0)).await;
    let undecoded = undecoded.unwrap_err();
    assert!(matches!(undecoded, StorageError::Invalid(m) if m.contains("does not decode")));
    let nowhere = blob("a tree nobody holds").0;
    let absent = backend.prepare_action_result(&tree_result(&nowhere)).await;
    let absent = absent.unwrap_err();
    assert!(matches!(absent, StorageError::NotFound(_)), "{absent:?}");
}

/// Asserts that `locate_packed` is the pack index's own answer and nothing more: a packed blob
/// has a span of its blob's length that lies inside its pack, one that is in no pack has none,
/// and a store without packs, or one whose index is down, is none and an error: and
/// `object_len` is the length of a stored object, none for a key with no object.
#[tokio::test]
async fn locate_packed_is_the_indexs_answer_and_object_len_a_head() {
    let held = blob("a blob held in a pack");
    let other = blob("a blob held nowhere");
    let packs = packs_holding(&[&held]).await;
    let node = Arc::new(CacheStorage::new(
        Arc::new(Cache::memory()),
        SegmentsConfig::DEFAULT,
    ));
    let backend = Backend::remote_over(node.clone(), Some(packs));

    let located = backend
        .locate_packed(&[held.0, other.0])
        .await
        .expect("located");
    let at = located[0].clone().expect("a span for the packed blob");
    assert_eq!(at.length, held.1.len() as u64);
    assert!(located[1].is_none());
    let size = backend
        .object_len(&at.pack_key)
        .await
        .expect("a head")
        .expect("the pack is stored");
    assert!(at.offset + at.length <= size);
    assert_eq!(
        backend
            .object_len("cas/l/nothing-here")
            .await
            .expect("a head"),
        None
    );

    let bare = Backend::remote_over(node.clone(), None);
    assert_eq!(
        bare.locate_packed(&[held.0]).await.expect("none"),
        vec![None]
    );
    assert_eq!(bare.object_len(&at.pack_key).await.expect("none"), None);

    let store = Arc::new(MemoryStore::new(Capabilities {
        conditional_put: true,
        object_lock: false,
    }));
    let keys = PackKeys::new(WriterKind::Frontend, "fe1", "boot1").expect("keys");
    let down = Packs::new(store, Arc::new(Down), keys, PackSettings::default()).expect("packs");
    let down = Backend::remote_over(node, Some(down));
    let failed = down.locate_packed(&[held.0]).await.unwrap_err();
    assert!(matches!(failed, StorageError::Unavailable(_)), "{failed:?}");
}

/// Asserts that a job read over a [`Backend`] gets the same answers through [`BlobSource`]: a
/// span for a packed blob (over one node and over a map), none for what is in no pack or too
/// large for one, none from a store without packs or an in-process cache, and the length of a
/// stored object for [`BlobSource::large_len`] by the key of the digest (none when it is not
/// there, an error for a key the object store cannot take).
#[tokio::test]
async fn a_backend_answers_a_job_read_locate_as_a_blob_source() {
    use crate::job_read::{BlobSource, Span, large_key};

    let held = blob("a blob a job locates in a pack");
    let too_large = Digest::new(kbf_types::DigestFunction::Sha256, [9; 32], 100 << 20);
    let node = Arc::new(CacheStorage::new(
        Arc::new(Cache::memory()),
        SegmentsConfig::DEFAULT,
    ));
    let alone = Backend::remote_over(node.clone(), Some(packs_holding(&[&held]).await));
    let trio = Arc::new(Trio::new());
    let routed = Arc::new(Routed::new(trio.clone(), trio.clone(), trio.clone()));
    let first = Arc::clone(&trio.nodes[&MemberId(1)]);
    let mapped = Backend::routed_over(routed, first, Some(packs_holding(&[&held]).await));

    for backend in [&alone, &mapped] {
        let spans = BlobSource::locate_packed(backend, &[held.0, too_large])
            .await
            .expect("located");
        let at: &Span = spans[0].as_ref().expect("a span for the packed blob");
        assert_eq!(at.length, held.1.len() as u64);
        assert!(
            spans[1].is_none(),
            "a blob over the pack cap is not asked of the index"
        );
        let size = backend.object_len(&at.key).await.expect("a head");
        assert!(at.offset + at.length <= size.expect("the pack is stored"));
        assert_eq!(
            BlobSource::large_len(backend, &held.0)
                .await
                .expect("a head"),
            None
        );
        assert!(matches!(
            backend.object_len("").await.unwrap_err(),
            StorageError::Unavailable(_)
        ));
    }
    assert!(large_key(&held.0).starts_with("cas/l/"));

    let local = Backend::from(Arc::new(Cache::memory()));
    let spans = BlobSource::locate_packed(&local, &[held.0])
        .await
        .expect("none");
    assert_eq!(spans, vec![None]);
    assert_eq!(
        BlobSource::large_len(&local, &held.0).await.expect("none"),
        None
    );
}
