//! How `materialize` fetches an input tree: many blobs at once, never more than
//! `FETCHES_IN_FLIGHT`, and each distinct digest once.

mod support;

use std::collections::HashMap;
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, PoisonError};

use kbf_daemon::cas::{Cas, CasError, label};
use kbf_daemon::tree::{FETCHES_IN_FLIGHT, materialize};
use kbf_proto::reapi::{Digest, Directory, DirectoryNode, FileNode};
use support::memory::MemoryCas;
use support::scratch;

/// A [`MemoryCas`] that counts its reads: how many are in flight at most, and how many
/// ask for each digest. A read yields a few times before it answers, so reads that are
/// started together are in flight together.
#[derive(Default)]
struct CountingCas {
    blobs: MemoryCas,
    in_flight: AtomicUsize,
    most_in_flight: AtomicUsize,
    reads: Mutex<HashMap<String, usize>>,
}

impl CountingCas {
    fn most_in_flight(&self) -> usize {
        self.most_in_flight.load(Ordering::SeqCst)
    }

    fn reads_of(&self, digest: &Digest) -> usize {
        let reads = self.reads.lock().unwrap_or_else(PoisonError::into_inner);
        reads.get(&label(digest)).copied().unwrap_or(0)
    }
}

impl Cas for CountingCas {
    async fn get(&self, digest: &Digest) -> Result<Vec<u8>, CasError> {
        *self
            .reads
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .entry(label(digest))
            .or_default() += 1;
        let now = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
        self.most_in_flight.fetch_max(now, Ordering::SeqCst);
        for _ in 0..4 {
            tokio::task::yield_now().await;
        }
        let bytes = self.blobs.get(digest).await;
        self.in_flight.fetch_sub(1, Ordering::SeqCst);
        bytes
    }

    async fn put(&self, bytes: Vec<u8>) -> Result<Digest, CasError> {
        self.blobs.put(bytes).await
    }
}

fn file(name: String, digest: Digest, is_executable: bool) -> FileNode {
    FileNode {
        name,
        digest: Some(digest),
        is_executable,
        ..FileNode::default()
    }
}

/// Catches the input tree's files fetched one blob at a time (on the pilot a tree of
/// thousands of small files took about 14 minutes per lease, the CAS idle), and a
/// fetch with no bound (a tree of a million files would open a million requests).
/// The files are all in the root, which is fetched alone first, so what is counted is
/// the file fetches. Mutant seen red: the file fetches given a bound of 1.
#[tokio::test]
async fn many_blobs_are_fetched_at_once_within_the_bound() {
    let cas = CountingCas::default();
    let mut root = Directory::default();
    for f in 0..200 {
        let bytes = format!("file {f}").into_bytes();
        root.files
            .push(file(format!("f{f:03}"), cas.blobs.insert(bytes), false));
    }
    let dir = scratch("materialize-in-flight");
    materialize(&cas, &cas.blobs.message(&root), &dir)
        .await
        .expect("written");
    let most = cas.most_in_flight();
    assert!(most > 1, "one fetch at a time");
    assert!(most <= FETCHES_IN_FLIGHT, "{most} fetches in flight");
    for f in 0..200 {
        let bytes = std::fs::read(dir.join(format!("f{f:03}"))).expect("written");
        assert_eq!(bytes, format!("file {f}").into_bytes());
    }
}

/// Catches a blob fetched once per file that names it (a toolchain tree repeats the
/// same small files in many places), a Directory fetched once per place it appears,
/// and the copies written from the one fetch sharing an inode (a write to one would
/// change the others, and the executable bit is per copy).
/// Mutant seen red: the files grouped by path rather than by digest.
#[tokio::test]
async fn a_digest_named_many_times_is_fetched_once() {
    let cas = CountingCas::default();
    let shared = cas.blobs.insert(b"the same bytes".to_vec());
    let mut leaf = Directory::default();
    for f in 0..100 {
        leaf.files
            .push(file(format!("f{f:03}"), shared.clone(), f % 2 == 0));
    }
    let leaf_digest = cas.blobs.message(&leaf);
    let root = Directory {
        directories: ["a", "b", "c"]
            .into_iter()
            .map(|name| DirectoryNode {
                name: name.to_owned(),
                digest: Some(leaf_digest.clone()),
            })
            .collect(),
        ..Directory::default()
    };
    let dir = scratch("materialize-shared");
    materialize(&cas, &cas.blobs.message(&root), &dir)
        .await
        .expect("written");
    assert_eq!(cas.reads_of(&shared), 1, "the shared file blob");
    assert_eq!(cas.reads_of(&leaf_digest), 1, "the shared Directory");
    let mut inodes = std::collections::HashSet::new();
    for sub in ["a", "b", "c"] {
        for f in 0..100 {
            let path = dir.join(format!("{sub}/f{f:03}"));
            assert_eq!(std::fs::read(&path).expect("written"), b"the same bytes");
            let meta = std::fs::metadata(&path).expect("stat");
            let mode = meta.permissions().mode() & 0o777;
            assert_eq!(mode, if f % 2 == 0 { 0o755 } else { 0o644 }, "{path:?}");
            assert!(inodes.insert(meta.ino()), "{path:?} shares an inode");
        }
    }
}
