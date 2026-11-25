//! A [`Backend`] over the packs a client uploads through: the Execute that names a tree the
//! client just uploaded reads none of its Directories, still asks the store whether it holds
//! them, and answers as a frontend that knew nothing of the upload does (`known_dirs` has the
//! rules).

use std::sync::Mutex;
use std::time::Duration;

use kbf_pack_index::memory::MemoryPackIndex;
use kbf_pack_index::{
    BlobDigest, Location, NewPack, PackIndex, PackIndexError, PackInfo, Recorded,
};
use kbf_state::ManualClock;

use super::packed::{owned, packs_of, store_and_index};
use super::*;
use crate::Packs;

/// An in-memory pack index that counts the digests it is asked to locate: every read of a blob
/// from a pack asks first.
struct Counting {
    inner: MemoryPackIndex,
    located: Mutex<Vec<BlobDigest>>,
}

impl Counting {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: MemoryPackIndex::new(Arc::new(ManualClock::default())),
            located: Mutex::default(),
        })
    }

    fn located(&self) -> usize {
        self.located.lock().expect("unpoisoned").len()
    }
}

impl PackIndex for Counting {
    fn record_pack(&self, pack: &NewPack) -> Result<Recorded, PackIndexError> {
        self.inner.record_pack(pack)
    }

    fn pack(&self, key: &str) -> Result<Option<PackInfo>, PackIndexError> {
        self.inner.pack(key)
    }

    fn locate(&self, digests: &[BlobDigest]) -> Result<Vec<Option<Location>>, PackIndexError> {
        self.located.lock().expect("unpoisoned").extend(digests);
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
        self.inner.drop_location(digest, pack_key)
    }
}

/// A tree of two Directories: `root` names `a` and `sub`, `sub` names `b`.
struct Uploaded {
    root: (Digest, Bytes),
    sub: (Digest, Bytes),
    a: (Digest, Bytes),
    b: (Digest, Bytes),
}

fn tree() -> Uploaded {
    let (a, b) = (blob("file a"), blob("file b"));
    let sub = dir(&[b.0], &[]);
    let root = dir(&[a.0], &[sub.0]);
    Uploaded { root, sub, a, b }
}

impl Uploaded {
    fn blobs(&self) -> Vec<(Digest, Bytes)> {
        owned(&[&self.root, &self.sub, &self.a, &self.b])
    }
}

/// A frontend over `store` and `index`, in front of an empty storage node of its own: its
/// packs, and the backend of its Executes.
fn frontend(
    name: &str,
    index: &Arc<Counting>,
    store: &Arc<kbf_objstore::MemoryStore>,
) -> (Packs, Backend) {
    let index: Arc<dyn PackIndex> = Arc::clone(index) as Arc<dyn PackIndex>;
    let packs = packs_of(name, store, &index);
    let node = CacheStorage::new(Arc::new(Cache::memory()), SegmentsConfig::DEFAULT);
    let backend = Backend::remote_over(Arc::new(node), Some(packs.clone()));
    (packs, backend)
}

/// The line of `packs`' metrics for the counter `name`.
fn counted(packs: &Packs, name: &str) -> String {
    let mut out = String::new();
    packs.metrics().render(&mut out);
    let prefix = format!("{name} ");
    out.lines()
        .find(|l| l.starts_with(&prefix))
        .map_or_else(|| format!("{name} absent"), str::to_owned)
}

/// Catches the Execute of a tree the client just uploaded reading its Directories back from the
/// packs: after the upload, the walk takes both Directories without a read (the index is asked
/// to locate nothing) and still asks the store about them. Before the uploads were learnt, the
/// walk read both.
#[tokio::test]
async fn a_tree_uploaded_through_the_frontend_is_walked_with_no_directory_read() {
    let (store, _) = store_and_index();
    let index = Counting::new();
    let (packs, backend) = frontend("fe1", &index, &store);
    let tree = tree();
    packs.put(tree.blobs()).await.expect("uploaded");
    let located = index.located();

    let walked = backend.walk_inputs(tree.root.0).await.expect("walk");

    assert!(walked.absent.is_empty(), "{:?}", walked.absent);
    assert_eq!(walked.dirs_read, 0);
    assert_eq!(index.located(), located, "a Directory was read from a pack");
    assert_eq!(walked.dir_cache_hits, 2);
    assert_eq!(walked.dirs_present_checked, 2, "both asked about");
    assert_eq!(walked.depth, 0);
    assert_eq!(
        counted(&packs, "kbf_pack_uploaded_directories_total"),
        "kbf_pack_uploaded_directories_total 2"
    );
}

/// Catches a Directory taken for held because it was uploaded: the store loses `sub` after the
/// upload (its pack location is dropped), and the Execute reports exactly `sub` missing, not the
/// file under it, as the Execute of a frontend that knew nothing of the upload does.
#[tokio::test]
async fn an_uploaded_directory_the_store_lost_is_reported_missing() {
    let (store, _) = store_and_index();
    let index = Counting::new();
    let (packs, knowing) = frontend("fe1", &index, &store);
    let (_, unknowing) = frontend("fe2", &index, &store);
    let tree = tree();
    packs.put(tree.blobs()).await.expect("uploaded");
    let sub = kbf_pack::blob_digest(&tree.sub.1);
    let place = index.inner.locate(&[sub]).expect("located")[0].clone();
    let place = place.expect("sub is in a pack");
    assert!(
        index
            .inner
            .drop_location(&sub, &place.pack_key)
            .expect("dropped")
    );

    let knew = knowing.walk_inputs(tree.root.0).await.expect("walk");
    let fresh = unknowing.walk_inputs(tree.root.0).await.expect("walk");

    assert_eq!(knew.absent, vec![tree.sub.0]);
    assert_eq!(knew.absent, fresh.absent);
    assert_eq!(
        (knew.dirs_read, fresh.dirs_read),
        (0, 1),
        "the other frontend read the root, and found sub gone"
    );
}

/// Catches an uploaded blob that only decodes as a Directory changing what an Execute answers:
/// `odd` is a file whose bytes are a Directory naming a file nobody uploaded. Named as a file,
/// it is held; named as a Directory, the file under it is missing; and the frontend that learnt
/// it as a Directory answers both trees exactly as one that did not.
#[tokio::test]
async fn a_file_whose_bytes_decode_as_a_directory_changes_no_answer() {
    let (store, _) = store_and_index();
    let index = Counting::new();
    let (packs, knowing) = frontend("fe1", &index, &store);
    let (_, unknowing) = frontend("fe2", &index, &store);
    let ghost = blob("never uploaded");
    let odd = dir(&[ghost.0], &[]);
    let as_file = dir(&[odd.0], &[]);
    let as_dir = dir(&[], &[odd.0]);
    packs
        .put(owned(&[&odd, &as_file, &as_dir]))
        .await
        .expect("uploaded");

    for root in [as_file.0, as_dir.0] {
        let knew = knowing.walk_inputs(root).await.expect("walk");
        let fresh = unknowing.walk_inputs(root).await.expect("walk");
        assert_eq!(knew.absent, fresh.absent, "{root}");
        assert_eq!(knew.dirs_read, 0, "{root}");
    }
    let named = unknowing.walk_inputs(as_dir.0).await.expect("walk");
    assert_eq!(named.absent, vec![ghost.0]);
}

/// Catches a Directory counted as learnt each time it is uploaded: a second upload of the same
/// tree learns nothing new, and the walk still reads nothing.
#[tokio::test]
async fn a_directory_uploaded_again_is_learnt_once() {
    let (store, _) = store_and_index();
    let index = Counting::new();
    let (packs, backend) = frontend("fe1", &index, &store);
    let tree = tree();

    packs.put(tree.blobs()).await.expect("uploaded");
    packs.put(tree.blobs()).await.expect("uploaded again");
    let walked = backend.walk_inputs(tree.root.0).await.expect("walk");

    assert_eq!((walked.dirs_read, walked.absent.len()), (0, 0));
    assert_eq!(
        counted(&packs, "kbf_pack_uploaded_directories_total"),
        "kbf_pack_uploaded_directories_total 2"
    );
}
