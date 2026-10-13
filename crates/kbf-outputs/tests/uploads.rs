//! How `collect` stores an output tree: many blobs at once, never more than
//! `UPLOADS_IN_FLIGHT`, each distinct digest once, and all of them stored before it
//! returns; and how many blobs its queue, `Uploads`, holds at once.

use std::collections::HashMap;
use std::convert::Infallible;
use std::io::Read as _;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use futures::FutureExt as _;
use kbf_outputs::{
    CHUNK_BYTES, OutputLimits, Store, StoreError, UPLOADS_IN_FLIGHT, Uploads, collect, digest_of,
};
use kbf_proto::reapi::{ActionResult, Digest, Directory, Tree};
use prost::Message;

/// Blobs in memory, counting the stores in flight at most and the stores of each
/// digest. A store yields a few times before it answers, so stores started together
/// are in flight together.
#[derive(Default)]
struct CountingStore {
    blobs: Mutex<HashMap<String, Vec<u8>>>,
    stores: Mutex<HashMap<String, usize>>,
    in_flight: AtomicUsize,
    most_in_flight: AtomicUsize,
}

impl CountingStore {
    async fn keep(&self, bytes: Vec<u8>) -> Digest {
        let digest = digest_of(&bytes);
        *self
            .stores
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .entry(digest.hash.clone())
            .or_default() += 1;
        let now = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
        self.most_in_flight.fetch_max(now, Ordering::SeqCst);
        for _ in 0..4 {
            tokio::task::yield_now().await;
        }
        self.blobs
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(digest.hash.clone(), bytes);
        self.in_flight.fetch_sub(1, Ordering::SeqCst);
        digest
    }

    fn stores_of(&self, digest: &Digest) -> usize {
        let stores = self.stores.lock().unwrap_or_else(PoisonError::into_inner);
        stores.get(&digest.hash).copied().unwrap_or(0)
    }

    fn holds(&self, digest: &Digest) -> bool {
        let blobs = self.blobs.lock().unwrap_or_else(PoisonError::into_inner);
        blobs.contains_key(&digest.hash)
    }

    fn tree(&self, result: &ActionResult) -> Tree {
        let digest = result.output_directories[0]
            .tree_digest
            .as_ref()
            .expect("tree digest");
        let blobs = self.blobs.lock().unwrap_or_else(PoisonError::into_inner);
        Tree::decode(blobs[&digest.hash].as_slice()).expect("a Tree")
    }
}

impl Store for CountingStore {
    async fn put(&self, bytes: Vec<u8>) -> Result<Digest, StoreError> {
        Ok(self.keep(bytes).await)
    }

    async fn put_file(
        &self,
        mut file: std::fs::File,
        digest: Digest,
    ) -> Result<Digest, StoreError> {
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        let stored = self.keep(bytes).await;
        if stored != digest {
            return Err("the file does not hash to its digest".into());
        }
        Ok(stored)
    }
}

fn scratch(name: &str) -> PathBuf {
    let exe = std::env::current_exe().expect("test binary");
    let dir = exe
        .parent()
        .expect("deps directory")
        .join("kbf-outputs-uploads")
        .join(name);
    if dir.exists() {
        kbf_outputs::remove_tree(&dir).expect("clear old scratch");
    }
    std::fs::create_dir_all(&dir).expect("scratch");
    dir
}

async fn collect_out(store: &CountingStore, root: &std::path::Path) -> ActionResult {
    let mut result = ActionResult::default();
    collect(
        store,
        root,
        "",
        &["out".to_owned()],
        OutputLimits::DEFAULT,
        &mut result,
    )
    .await
    .expect("collect");
    result
}

/// Every digest the Tree of `result` names, the Directories' included.
fn named(result: &ActionResult, tree: &Tree) -> Vec<Digest> {
    let mut named = vec![
        result.output_directories[0]
            .root_directory_digest
            .clone()
            .expect("root digest"),
    ];
    for directory in tree.root.iter().chain(&tree.children) {
        named.extend(directory.files.iter().filter_map(|f| f.digest.clone()));
        named.extend(
            directory
                .directories
                .iter()
                .filter_map(|d| d.digest.clone()),
        );
    }
    named
}

/// Catches an output tree stored one blob at a time (on the pilot a tree of
/// thousands of small files took minutes per lease, the CAS idle), stores with no
/// bound, and `collect` returning before every blob the result names is stored (the
/// action cache refuses such a result). One file is larger than a chunk, so the
/// streamed path is queued too. Mutant seen red: `UPLOADS_IN_FLIGHT` passed as 1.
#[tokio::test]
async fn many_blobs_are_stored_at_once_within_the_bound() {
    let root = scratch("in-flight");
    for d in 0..4 {
        std::fs::create_dir_all(root.join(format!("out/d{d}"))).expect("mkdir");
        for f in 0..50 {
            let path = root.join(format!("out/d{d}/f{f:03}"));
            std::fs::write(path, format!("file {f} of directory {d}")).expect("write");
        }
    }
    let large: Vec<u8> = (0..=CHUNK_BYTES).map(|i| (i % 251) as u8).collect();
    std::fs::write(root.join("out/large"), &large).expect("write");
    let store = CountingStore::default();
    let result = collect_out(&store, &root).await;
    let most = store.most_in_flight.load(Ordering::SeqCst);
    assert!(most > 1, "one store at a time");
    assert!(most <= UPLOADS_IN_FLIGHT, "{most} stores in flight");
    let tree = store.tree(&result);
    let named = named(&result, &tree);
    assert_eq!(
        named.len(),
        1 + 201 + 4,
        "root, 201 files, four directories"
    );
    let absent: Vec<&Digest> = named.iter().filter(|d| !store.holds(d)).collect();
    assert!(absent.is_empty(), "named but not stored: {absent:?}");
    assert!(store.holds(&digest_of(&large)));
}

/// Catches a blob stored once per file that holds it: each is a round trip to the CAS
/// for bytes it already has. Also keeps a Directory that appears in three places
/// stored once, as it was before uploads were queued. Mutant seen red: the queue's
/// digest check removed.
#[tokio::test]
async fn a_digest_named_many_times_is_stored_once() {
    let root = scratch("shared");
    for sub in ["a", "b", "c"] {
        std::fs::create_dir_all(root.join(format!("out/{sub}"))).expect("mkdir");
        for f in 0..100 {
            let path = root.join(format!("out/{sub}/f{f:03}"));
            std::fs::write(path, b"the same bytes").expect("write");
        }
    }
    let store = CountingStore::default();
    let result = collect_out(&store, &root).await;
    assert_eq!(
        store.stores_of(&digest_of(b"the same bytes")),
        1,
        "the file"
    );
    let tree = store.tree(&result);
    let leaf: &Directory = &tree.children[0];
    assert_eq!(leaf.files.len(), 100);
    assert_eq!(
        store.stores_of(&digest_of(&leaf.encode_to_vec())),
        1,
        "the Directory a, b and c share"
    );
}

/// One blob a walk holds, counted while it is alive: queued, or being stored.
struct Held(Arc<AtomicUsize>);

impl Held {
    fn new(alive: &Arc<AtomicUsize>, most: &AtomicUsize) -> Self {
        let now = alive.fetch_add(1, Ordering::SeqCst) + 1;
        most.fetch_max(now, Ordering::SeqCst);
        Self(Arc::clone(alive))
    }
}

impl Drop for Held {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Catches a queue that takes every blob the walk hands it while the stores are slow:
/// the walk would then hold the whole tree at once (in the container driver an open
/// descriptor per file; on the pilot, trees of thousands of files), however few are
/// stored at a time. Every store waits on a gate the test opens only once
/// `UPLOADS_IN_FLIGHT` stores have started and the walk had time to run ahead; a
/// blob is counted from before it is queued until its store ends. Mutant seen red:
/// the queue's channel given room for 1_000_000 (`uploads.rs`, `mpsc::channel(0)`).
#[tokio::test]
async fn a_walk_holds_at_most_one_blob_past_the_stores_in_flight() {
    let alive = Arc::new(AtomicUsize::new(0));
    let most = AtomicUsize::new(0);
    let started = Arc::new(AtomicUsize::new(0));
    let (open, gate) = futures::channel::oneshot::channel::<()>();
    let gate = gate.shared();
    let (mut uploads, drain) = Uploads::<Infallible>::new();
    let walk = async {
        for i in 0..300 {
            let held = Held::new(&alive, &most);
            let gate = gate.clone();
            let started = Arc::clone(&started);
            let upload = async move {
                started.fetch_add(1, Ordering::SeqCst);
                let _ = gate.await;
                drop(held);
                Ok(())
            };
            uploads
                .queue(&digest_of(format!("blob {i}").as_bytes()), upload)
                .await;
        }
        drop(uploads);
        Ok::<(), Infallible>(())
    };
    let opener = async {
        while started.load(Ordering::SeqCst) < UPLOADS_IN_FLIGHT {
            tokio::task::yield_now().await;
        }
        for _ in 0..100 {
            tokio::task::yield_now().await;
        }
        let _ = open.send(());
        Ok::<(), Infallible>(())
    };
    futures::try_join!(walk, drain, opener).expect("infallible");
    assert_eq!(started.load(Ordering::SeqCst), 300, "every blob stored");
    let most = most.load(Ordering::SeqCst);
    assert!(
        most >= UPLOADS_IN_FLIGHT,
        "{most}: the gate held the stores"
    );
    assert!(
        most <= UPLOADS_IN_FLIGHT + 1,
        "{most} blobs held at once, {UPLOADS_IN_FLIGHT} stores in flight"
    );
}
