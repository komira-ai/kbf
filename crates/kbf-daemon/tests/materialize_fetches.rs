//! How `materialize` fetches an input tree: many blobs at once, never more than the
//! CAS's `in_flight` (the transfer engine's process-wide limit), each distinct digest
//! once, and on an error nothing more started and nothing left writing once it returns.

mod support;

use std::collections::{HashMap, HashSet};
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use kbf_daemon::cas::{Cas, CasError, label};
use kbf_daemon::tree::{TreeError, materialize};
use kbf_proto::reapi::{Digest, Directory, DirectoryNode, FileNode};
use support::memory::MemoryCas;
use support::scratch;

/// A [`MemoryCas`] that counts its reads: how many are in flight at most, and how many
/// ask for each digest. A read yields a few times before it answers, so reads that are
/// started together are in flight together; a read of a blob it lacks fails at once.
/// A read of a digest in `held` (by label) is counted, then waits until a read of a
/// blob it lacks has been counted, so no held blob can finish before the failure.
#[derive(Default)]
struct CountingCas {
    blobs: MemoryCas,
    held: HashSet<String>,
    missing_read: AtomicBool,
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

/// The in-flight limit [`CountingCas`] answers: not the default, so a walk that kept a
/// bound of its own would show.
const BOUND: usize = 16;

impl Cas for CountingCas {
    type Lease = std::sync::Arc<Self>;

    fn lease(self: &std::sync::Arc<Self>, _: &kbf_types::LeaseId) -> std::sync::Arc<Self> {
        std::sync::Arc::clone(self)
    }

    async fn get(&self, digest: &Digest) -> Result<Vec<u8>, CasError> {
        *self
            .reads
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .entry(label(digest))
            .or_default() += 1;
        if self.blobs.blob(digest).is_none() {
            self.missing_read.store(true, Ordering::SeqCst);
            return Err(CasError::Missing(label(digest)));
        }
        if self.held.contains(&label(digest)) {
            while !self.missing_read.load(Ordering::SeqCst) {
                tokio::task::yield_now().await;
            }
        }
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

    fn in_flight(&self) -> usize {
        BOUND
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
/// walk with no bound, or one of its own rather than the CAS's (a tree of a million
/// files would queue a million requests). The files are all in the root, which is
/// fetched alone first, so what is counted is the file fetches. Mutants seen red: the
/// file fetches given a bound of 1; no bound (`usize::MAX`).
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
    assert!(most <= BOUND, "{most} fetches in flight");
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

/// Catches `materialize` going on fetching the rest of the tree after a blob failed:
/// the action is refused either way, so each fetch after the first error is a round
/// trip and a write spent for nothing (a toolchain tree is thousands of them). The
/// first blob is missing; the reads of the present blobs are held until its read is
/// counted, so the first `BOUND` blobs are all started and none of them finishes before
/// the failure, whatever the order the blocking pool creates their files in. Exactly
/// the root and those `BOUND` blobs are read: no other blob may start. Without the hold
/// a sibling could finish before the missing blob's file was created and let one more
/// start before the failure (kbf#676). Mutants seen red: the check that
/// a blob failed removed (201 reads); the hold replaced by the missing read waiting for
/// `BOUND + 1` present reads, the schedule of #676 (more than `1 + BOUND` reads).
#[tokio::test]
async fn no_blob_is_fetched_after_one_failed() {
    let mut cas = CountingCas::default();
    let mut root = Directory::default();
    let missing = kbf_daemon::cas::digest_of(b"not in the CAS");
    root.files.push(file("f000".to_owned(), missing, false));
    for f in 1..200 {
        let digest = cas.blobs.insert(format!("file {f}").into_bytes());
        cas.held.insert(label(&digest));
        root.files.push(file(format!("f{f:03}"), digest, false));
    }
    let root = cas.blobs.message(&root);
    let dir = scratch("materialize-stops");
    // A deadline, so a materialize that never reads the missing blob fails rather
    // than hangs on the held reads.
    let error = tokio::time::timeout(Duration::from_secs(30), materialize(&cas, &root, &dir))
        .await
        .expect("the missing blob was read in 30 s")
        .expect_err("a blob is missing");
    assert!(
        matches!(error, TreeError::Cas(CasError::Missing(_))),
        "{error}"
    );
    let reads: usize = cas
        .reads
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .values()
        .sum();
    assert_eq!(
        reads,
        1 + BOUND,
        "the root and the first {BOUND} blobs, none started after the error"
    );
}

/// A CAS whose read of `held` ties up tokio's blocking pool (one thread in this test)
/// until the test releases it, and whose read of `failing` fails once `held` was
/// read. The write of `held`'s file is then queued on the pool when the error comes.
///
/// The read of `held` ties up the pool only once `failing` has been asked for: the
/// file of `failing` is created on the pool before it is read, and that create must
/// not queue behind the tied-up thread. `materialize` queues both creates at once, but
/// the create of `held` can finish before the create of `failing` is queued (the
/// runtime thread preempted in between), and the read of `held` would then tie up the
/// thread first: `failing` never read, the test waiting out its deadline (#622).
struct HeldCas {
    blobs: MemoryCas,
    held: Digest,
    failing: Digest,
    release: Mutex<Option<std::sync::mpsc::Receiver<()>>>,
    failing_asked: AtomicBool,
    held_read: AtomicBool,
    failed: AtomicBool,
}

impl Cas for HeldCas {
    type Lease = std::sync::Arc<Self>;

    fn lease(self: &std::sync::Arc<Self>, _: &kbf_types::LeaseId) -> std::sync::Arc<Self> {
        std::sync::Arc::clone(self)
    }

    async fn get(&self, digest: &Digest) -> Result<Vec<u8>, CasError> {
        if *digest == self.failing {
            self.failing_asked.store(true, Ordering::SeqCst);
        }
        if *digest == self.held {
            while !self.failing_asked.load(Ordering::SeqCst) {
                tokio::task::yield_now().await;
            }
            let release = self
                .release
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .take()
                .expect("held is read once");
            // Not awaited: the pool's only thread waits here, so what materialize
            // queues on the pool next runs once the test releases it, in order.
            drop(tokio::task::spawn_blocking(move || release.recv()));
            self.held_read.store(true, Ordering::SeqCst);
        }
        if *digest == self.failing {
            while !self.held_read.load(Ordering::SeqCst) {
                tokio::task::yield_now().await;
            }
            for _ in 0..8 {
                tokio::task::yield_now().await;
            }
            self.failed.store(true, Ordering::SeqCst);
            return Err(CasError::Missing(label(digest)));
        }
        self.blobs.get(digest).await
    }

    async fn put(&self, bytes: Vec<u8>) -> Result<Digest, CasError> {
        self.blobs.put(bytes).await
    }
}

/// The names in `dir` with their sizes, sorted.
fn listing(dir: &Path) -> Vec<(String, u64)> {
    let mut names: Vec<(String, u64)> = std::fs::read_dir(dir)
        .expect("list the lease directory")
        .map(|entry| {
            let entry = entry.expect("an entry");
            let size = entry.metadata().expect("stat").len();
            (entry.file_name().to_string_lossy().into_owned(), size)
        })
        .collect();
    names.sort();
    names
}

/// Catches `materialize` returning its error while a write it started is still
/// queued on tokio's blocking pool: dropping a `tokio::fs` future does not stop the
/// call, so the file's bytes would appear after the error, while the caller clears the
/// lease directory. Here the write of `held` is queued behind a blocked pool thread when
/// `failing` fails; `materialize` must not return before the write ran. Mutant seen
/// red: the first error returned at once (the `try_for_each` drain this replaced).
#[test]
fn no_file_appears_after_materialize_failed() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .max_blocking_threads(1)
        .enable_time()
        .build()
        .expect("a runtime");
    runtime.block_on(async {
        let blobs = MemoryCas::default();
        let held = blobs.insert(b"held".to_vec());
        let failing = kbf_daemon::cas::digest_of(b"failing");
        let root = blobs.message(&Directory {
            files: vec![
                file("held".to_owned(), held.clone(), false),
                file("failing".to_owned(), failing.clone(), false),
            ],
            ..Directory::default()
        });
        let (release, wait) = std::sync::mpsc::channel();
        let cas = Arc::new(HeldCas {
            blobs,
            held,
            failing,
            release: Mutex::new(Some(wait)),
            failing_asked: AtomicBool::new(false),
            held_read: AtomicBool::new(false),
            failed: AtomicBool::new(false),
        });
        let dir = scratch("materialize-settles");
        let task = tokio::spawn({
            let cas = Arc::clone(&cas);
            let dir = dir.clone();
            async move {
                let written = materialize(&*cas, &root, &dir).await;
                (written.is_err(), listing(&dir))
            }
        });
        // A deadline, so a materialize that never reaches the missing blob fails
        // rather than hangs.
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        while !cas.failed.load(Ordering::SeqCst) {
            assert!(
                std::time::Instant::now() < deadline,
                "the missing blob was not read in 30 s"
            );
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        // Time for a materialize that does not wait for its writes to return.
        tokio::time::sleep(Duration::from_millis(200)).await;
        release.send(()).expect("the pool thread waits");
        let (failed, at_return) = task.await.expect("materialize ran");
        assert!(failed, "a blob is missing");
        // Queued after everything materialize queued, so it runs after all of it.
        tokio::task::spawn_blocking(|| ()).await.expect("ran");
        assert_eq!(
            listing(&dir),
            at_return,
            "written after materialize returned"
        );
    });
}
