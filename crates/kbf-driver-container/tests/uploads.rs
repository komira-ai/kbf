//! How the container driver's `collect` stores an output tree: many blobs at once,
//! never more than `UPLOADS_IN_FLIGHT`, each distinct digest once, and all of them
//! stored before it returns.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, PoisonError};

use futures::StreamExt as _;
use kbf_daemon::cas::{Cas, CasError, Chunks, digest_of, label};
use kbf_driver_container::tree::{OutputLimits, collect};
use kbf_outputs::UPLOADS_IN_FLIGHT;
use kbf_proto::reapi::{ActionResult, Digest, Tree};
use prost::Message;

/// Blobs in memory, counting the stores in flight at most and the stores of each
/// digest. A store yields a few times before it answers, so stores started together
/// are in flight together.
#[derive(Default)]
struct CountingCas {
    blobs: Mutex<HashMap<String, Vec<u8>>>,
    stores: Mutex<HashMap<String, usize>>,
    in_flight: AtomicUsize,
    most_in_flight: AtomicUsize,
}

impl CountingCas {
    async fn keep(&self, digest: &Digest, bytes: Vec<u8>) {
        *self
            .stores
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .entry(label(digest))
            .or_default() += 1;
        let now = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
        self.most_in_flight.fetch_max(now, Ordering::SeqCst);
        for _ in 0..4 {
            tokio::task::yield_now().await;
        }
        self.blobs
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(label(digest), bytes);
        self.in_flight.fetch_sub(1, Ordering::SeqCst);
    }

    fn stores_of(&self, digest: &Digest) -> usize {
        let stores = self.stores.lock().unwrap_or_else(PoisonError::into_inner);
        stores.get(&label(digest)).copied().unwrap_or(0)
    }

    fn blob(&self, digest: &Digest) -> Option<Vec<u8>> {
        let blobs = self.blobs.lock().unwrap_or_else(PoisonError::into_inner);
        blobs.get(&label(digest)).cloned()
    }
}

impl Cas for CountingCas {
    async fn get(&self, digest: &Digest) -> Result<Vec<u8>, CasError> {
        self.blob(digest)
            .ok_or_else(|| CasError::Missing(label(digest)))
    }

    async fn put(&self, bytes: Vec<u8>) -> Result<Digest, CasError> {
        let digest = digest_of(&bytes);
        self.keep(&digest, bytes).await;
        Ok(digest)
    }

    async fn put_chunks(&self, digest: Digest, mut chunks: Chunks) -> Result<Digest, CasError> {
        let mut bytes = Vec::new();
        while let Some(chunk) = chunks.next().await {
            let chunk = chunk.map_err(|e| CasError::Read(label(&digest), e.to_string()))?;
            bytes.extend_from_slice(&chunk);
        }
        if digest_of(&bytes) != digest {
            return Err(CasError::Corrupt(label(&digest), label(&digest_of(&bytes))));
        }
        self.keep(&digest, bytes).await;
        Ok(digest)
    }
}

fn scratch(name: &str) -> std::path::PathBuf {
    let dir = std::path::Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("kbf-driver-container-uploads")
        .join(format!("{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch");
    dir
}

async fn collect_out(cas: &CountingCas, upper: &std::path::Path) -> (ActionResult, Tree) {
    let mut result = ActionResult::default();
    collect(
        cas,
        upper,
        "",
        &["out".to_owned()],
        OutputLimits::DEFAULT,
        &mut result,
    )
    .await
    .expect("collect");
    let digest = result.output_directories[0]
        .tree_digest
        .clone()
        .expect("tree digest");
    let tree = Tree::decode(cas.blob(&digest).expect("the Tree").as_slice()).expect("a Tree");
    (result, tree)
}

/// Catches an output tree stored one blob at a time (on the pilot a tree of
/// thousands of small files took minutes per lease, the CAS idle), stores with no
/// bound, and `collect` returning before every blob the result names is stored (the
/// action cache refuses such a result). Mutant seen red: `UPLOADS_IN_FLIGHT` passed
/// as 1.
#[tokio::test]
async fn many_blobs_are_stored_at_once_within_the_bound() {
    let upper = scratch("in-flight");
    for d in 0..4 {
        std::fs::create_dir_all(upper.join(format!("out/d{d}"))).expect("mkdir");
        for f in 0..50 {
            let path = upper.join(format!("out/d{d}/f{f:03}"));
            std::fs::write(path, format!("file {f} of directory {d}")).expect("write");
        }
    }
    let cas = CountingCas::default();
    let (result, tree) = collect_out(&cas, &upper).await;
    let most = cas.most_in_flight.load(Ordering::SeqCst);
    assert!(most > 1, "one store at a time");
    assert!(most <= UPLOADS_IN_FLIGHT, "{most} stores in flight");
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
    assert_eq!(
        named.len(),
        1 + 200 + 4,
        "root, 200 files, four directories"
    );
    let absent: Vec<&Digest> = named.iter().filter(|d| cas.blob(d).is_none()).collect();
    assert!(absent.is_empty(), "named but not stored: {absent:?}");
}

/// Catches a blob stored once per file that holds it, and a Directory stored once per
/// place it appears: each is a round trip to the CAS for bytes it already has.
/// Mutant seen red: the queue's digest check removed.
#[tokio::test]
async fn a_digest_named_many_times_is_stored_once() {
    let upper = scratch("shared");
    for sub in ["a", "b", "c"] {
        std::fs::create_dir_all(upper.join(format!("out/{sub}"))).expect("mkdir");
        for f in 0..100 {
            let path = upper.join(format!("out/{sub}/f{f:03}"));
            std::fs::write(path, b"the same bytes").expect("write");
        }
    }
    let cas = CountingCas::default();
    let (_, tree) = collect_out(&cas, &upper).await;
    assert_eq!(cas.stores_of(&digest_of(b"the same bytes")), 1, "the file");
    let leaf = &tree.children[0];
    assert_eq!(leaf.files.len(), 100);
    assert_eq!(
        cas.stores_of(&digest_of(&leaf.encode_to_vec())),
        1,
        "the Directory a, b and c share"
    );
}
