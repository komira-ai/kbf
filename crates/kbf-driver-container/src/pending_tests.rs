//! [`super::Pending`]'s unit tests.

use futures::FutureExt as _;

use super::*;
use crate::outputs::DIRECTORY;

/// Catches a missing file opened again without checking it is the one hashed: a
/// file replaced since (another inode at the name), a directory on its path
/// replaced by a symlink, and a file removed must each fail, not be stored as the
/// output. Mutants seen red: the device and inode check removed (the replaced file
/// opens); a symlinked directory followed.
#[test]
fn a_file_is_opened_again_only_if_it_is_the_one_hashed() {
    let exe = std::env::current_exe().expect("test binary");
    let dir = exe
        .parent()
        .expect("deps directory")
        .join("kbf-driver-container-unit")
        .join("pending-open");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("upper/sub")).expect("mkdir");
    std::fs::create_dir_all(dir.join("elsewhere")).expect("mkdir");
    std::fs::write(dir.join("upper/sub/f"), b"hashed").expect("write");
    let upper = rustix::fs::openat(rustix::fs::CWD, dir.join("upper"), DIRECTORY, Mode::empty())
        .expect("open upper");
    let stat = rustix::fs::stat(dir.join("upper/sub/f")).expect("stat");
    let at = FileAt {
        dirs: vec!["sub".to_owned()].into(),
        name: "f".to_owned(),
        id: (stat.st_dev, stat.st_ino),
        shown: dir.join("upper/sub/f"),
    };
    let file = at.open(&upper).expect("the file hashed");
    assert_eq!(std::io::read_to_string(file).expect("read"), "hashed");
    // The directory replaced by a symlink to it: the file at the end is the one
    // hashed, but the path no longer is the action's.
    std::fs::rename(dir.join("upper/sub"), dir.join("elsewhere/sub")).expect("move");
    std::os::unix::fs::symlink(dir.join("elsewhere/sub"), dir.join("upper/sub")).expect("symlink");
    let why = at.open(&upper).expect_err("a symlink on the path");
    assert!(why.to_string().contains("removed or replaced"), "{why}");
    std::fs::remove_file(dir.join("upper/sub")).expect("unlink");
    std::fs::rename(dir.join("elsewhere/sub"), dir.join("upper/sub")).expect("move back");
    at.open(&upper).expect("the file hashed, back in place");
    // Another file renamed over it: same name, same bytes, another inode.
    std::fs::write(dir.join("upper/sub/g"), b"hashed").expect("write");
    std::fs::rename(dir.join("upper/sub/g"), dir.join("upper/sub/f")).expect("rename");
    let why = at.open(&upper).expect_err("replaced");
    assert!(why.to_string().contains("removed or replaced"), "{why}");
    // The file removed from its directory.
    std::fs::remove_file(dir.join("upper/sub/f")).expect("unlink");
    let why = at.open(&upper).expect_err("removed");
    assert_eq!(why.kind(), std::io::ErrorKind::NotFound, "{why}");
}

/// A store whose puts each wait on `gate`, counted in `started` once they wait.
struct GatedStore {
    gate: futures::future::Shared<futures::channel::oneshot::Receiver<()>>,
    started: std::sync::atomic::AtomicUsize,
    stored: std::sync::Mutex<Vec<Digest>>,
}

impl GatedStore {
    async fn keep(&self, digest: Digest) -> Digest {
        use std::sync::atomic::Ordering;
        self.started.fetch_add(1, Ordering::SeqCst);
        let _ = self.gate.clone().await;
        let mut stored = self.stored.lock().expect("stored");
        stored.push(digest.clone());
        digest
    }
}

impl Cas for GatedStore {
    type Lease = Arc<Self>;

    fn lease(self: &Arc<Self>, _: &kbf_types::LeaseId) -> Arc<Self> {
        Arc::clone(self)
    }

    async fn get(&self, digest: &Digest) -> Result<Vec<u8>, kbf_daemon::cas::CasError> {
        Err(kbf_daemon::cas::CasError::Missing(label(digest)))
    }

    async fn put(&self, bytes: Vec<u8>) -> Result<Digest, kbf_daemon::cas::CasError> {
        Ok(self.keep(digest_of(&bytes)).await)
    }

    /// Waits before reading: the file stays open while the store is slow.
    async fn put_chunks(
        &self,
        digest: Digest,
        chunks: kbf_daemon::cas::Chunks,
    ) -> Result<Digest, kbf_daemon::cas::CasError> {
        let digest = self.keep(digest).await;
        drop(chunks);
        Ok(digest)
    }
}

/// Catches the copies of missing blobs into the spool running without a bound: each
/// copy holds its file open until it is written, so a tree of thousands of missing
/// files would hold thousands of descriptors at once (the daemon's `nofile` limit).
/// 40 missing files and a message, copied `in_flight` = 4 at a time into a store whose
/// puts wait on a gate: exactly 4 wait, however long the test lets the copies run
/// ahead, and all 41 are stored once the gate opens. A held file is skipped. Mutant
/// seen red: `buffer_unordered(usize::MAX)` (41 waiting).
#[tokio::test]
async fn missing_blobs_are_copied_in_flight_at_a_time() {
    use std::sync::atomic::Ordering;
    let exe = std::env::current_exe().expect("test binary");
    let dir = exe
        .parent()
        .expect("deps directory")
        .join("kbf-driver-container-unit")
        .join("pending-copy");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("mkdir");
    let mut pending = Pending::default();
    let mut missing = HashSet::new();
    for f in 0..41 {
        let bytes = format!("file {f}").into_bytes();
        std::fs::write(dir.join(format!("f{f}")), &bytes).expect("write");
        let stat = rustix::fs::stat(dir.join(format!("f{f}"))).expect("stat");
        let digest = digest_of(&bytes);
        // The last file is held: not copied.
        if f < 40 {
            missing.insert(label(&digest));
        }
        let at = FileAt {
            dirs: Vec::new().into(),
            name: format!("f{f}"),
            id: (stat.st_dev, stat.st_ino),
            shown: dir.join(format!("f{f}")),
        };
        pending.file(&digest, at);
    }
    missing.insert(label(&pending.bytes(b"a message".to_vec())));
    let upper = Arc::new(
        rustix::fs::openat(rustix::fs::CWD, &dir, DIRECTORY, Mode::empty()).expect("open"),
    );
    let (open, gate) = futures::channel::oneshot::channel::<()>();
    let store = GatedStore {
        gate: gate.shared(),
        started: std::sync::atomic::AtomicUsize::new(0),
        stored: std::sync::Mutex::default(),
    };
    let copy = pending.copy_missing(&missing, &store, &upper, 4);
    let watch = async {
        while store.started.load(Ordering::SeqCst) < 4 {
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
        // Time for copies past the bound to start, if any would.
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        let waiting = store.started.load(Ordering::SeqCst);
        let _ = open.send(());
        Ok::<usize, TreeError>(waiting)
    };
    let ((), waiting) = futures::try_join!(copy, watch).expect("copied");
    assert_eq!(waiting, 4, "copies waiting at once");
    let stored = store.stored.lock().expect("stored").len();
    assert_eq!(stored, 41, "40 missing files and the message");
}

/// A store that holds every blob and records each FindMissing question.
#[derive(Default)]
struct HoldsAll {
    asked: std::sync::Mutex<Vec<Digest>>,
}

impl Cas for HoldsAll {
    type Lease = Arc<Self>;

    fn lease(self: &Arc<Self>, _: &kbf_types::LeaseId) -> Arc<Self> {
        Arc::clone(self)
    }

    async fn get(&self, digest: &Digest) -> Result<Vec<u8>, kbf_daemon::cas::CasError> {
        Err(kbf_daemon::cas::CasError::Missing(label(digest)))
    }

    async fn put(&self, bytes: Vec<u8>) -> Result<Digest, kbf_daemon::cas::CasError> {
        panic!(
            "{} stored, but the store holds every blob",
            label(&digest_of(&bytes))
        );
    }

    async fn missing(&self, digests: &[Digest]) -> Vec<Digest> {
        self.asked.lock().expect("asked").extend_from_slice(digests);
        Vec::new()
    }
}

/// Catches the walk's blobs not being deduplicated: a tree with repeated content (two
/// files with the same bytes, a Directory message added twice) must ask FindMissing
/// about each digest once, so the lease's `find_missing_digests` counter and the
/// question's size count distinct blobs. The spool dedupes what it stores, so only
/// the question shows the defect. Mutant seen red: `found.insert(..) || true` in
/// `Pending::add` (5 digests asked).
#[tokio::test]
async fn repeated_content_is_asked_about_once() {
    let exe = std::env::current_exe().expect("test binary");
    let dir = exe
        .parent()
        .expect("deps directory")
        .join("kbf-driver-container-unit")
        .join("pending-dedupe");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("spools")).expect("mkdir");
    let mut pending = Pending::default();
    for (name, bytes) in [("a", "same"), ("b", "same"), ("c", "other")] {
        std::fs::write(dir.join(name), bytes).expect("write");
        let stat = rustix::fs::stat(dir.join(name)).expect("stat");
        let at = FileAt {
            dirs: Vec::new().into(),
            name: name.to_owned(),
            id: (stat.st_dev, stat.st_ino),
            shown: dir.join(name),
        };
        pending.file(&digest_of(bytes.as_bytes()), at);
    }
    let message = pending.bytes(b"a message".to_vec());
    assert_eq!(pending.bytes(b"a message".to_vec()), message);
    let upper = Arc::new(
        rustix::fs::openat(rustix::fs::CWD, &dir, DIRECTORY, Mode::empty()).expect("open"),
    );
    let store = HoldsAll::default();
    pending
        .store_missing(&store, &upper, &dir.join("spools"))
        .await
        .expect("stored");
    let asked = store.asked.lock().expect("asked").clone();
    let want = vec![digest_of(b"same"), digest_of(b"other"), message];
    assert_eq!(
        asked, want,
        "each distinct digest asked once, in walk order"
    );
}

/// A store that lacks every blob and records each spool it is given.
#[derive(Default)]
struct LacksAll {
    /// How many blobs each spool held.
    spools: std::sync::Mutex<Vec<usize>>,
}

impl Cas for LacksAll {
    type Lease = Arc<Self>;

    fn lease(self: &Arc<Self>, _: &kbf_types::LeaseId) -> Arc<Self> {
        Arc::clone(self)
    }

    async fn get(&self, digest: &Digest) -> Result<Vec<u8>, kbf_daemon::cas::CasError> {
        Err(kbf_daemon::cas::CasError::Missing(label(digest)))
    }

    async fn put(&self, bytes: Vec<u8>) -> Result<Digest, kbf_daemon::cas::CasError> {
        panic!(
            "{} put on its own, not through the spool",
            label(&digest_of(&bytes))
        );
    }

    async fn put_spool(&self, spool: Arc<Spool>) -> Result<(), kbf_daemon::cas::CasError> {
        let held = spool.records().len();
        self.spools.lock().expect("spools").push(held);
        Ok(())
    }
}

/// Catches a blob that could not be copied into the spool being dropped: the action
/// result would name a digest the CAS never got. Each case walks two missing files and
/// spoils one before [`Pending::store_missing`] copies it: replaced by another file of
/// the same bytes (another inode), removed, and its bytes changed in place (the spool
/// refuses bytes that do not hash to the digest). Each must fail and store no spool.
/// Mutant seen red: `let _ = self.copy_missing(..).await;` in `store_missing` (Ok,
/// and a spool of the one good file stored).
#[tokio::test]
async fn a_blob_that_cannot_be_copied_fails_the_store() {
    let exe = std::env::current_exe().expect("test binary");
    let root = exe
        .parent()
        .expect("deps directory")
        .join("kbf-driver-container-unit")
        .join("pending-spoiled");
    let _ = std::fs::remove_dir_all(&root);
    type Spoil = fn(&Path);
    let cases: [(&str, Spoil, &str); 3] = [
        (
            "replaced",
            |f| {
                let other = f.with_extension("new");
                std::fs::write(&other, b"spoiled").expect("write");
                std::fs::rename(&other, f).expect("rename");
            },
            "removed or replaced",
        ),
        (
            "removed",
            |f| std::fs::remove_file(f).expect("unlink"),
            "No such file",
        ),
        (
            "changed",
            |f| {
                use std::io::Write as _;
                let mut file = std::fs::OpenOptions::new()
                    .write(true)
                    .open(f)
                    .expect("open");
                file.write_all(b"SPOILED").expect("write");
            },
            "failed verification",
        ),
    ];
    for (case, spoil, why) in cases {
        let dir = root.join(case);
        std::fs::create_dir_all(dir.join("spools")).expect("mkdir");
        let mut pending = Pending::default();
        for (name, bytes) in [("good", "kept"), ("bad", "spoiled")] {
            std::fs::write(dir.join(name), bytes).expect("write");
            let stat = rustix::fs::stat(dir.join(name)).expect("stat");
            let at = FileAt {
                dirs: Vec::new().into(),
                name: name.to_owned(),
                id: (stat.st_dev, stat.st_ino),
                shown: dir.join(name),
            };
            pending.file(&digest_of(bytes.as_bytes()), at);
        }
        spoil(&dir.join("bad"));
        let upper = Arc::new(
            rustix::fs::openat(rustix::fs::CWD, &dir, DIRECTORY, Mode::empty()).expect("open"),
        );
        let store = LacksAll::default();
        let err = pending
            .store_missing(&store, &upper, &dir.join("spools"))
            .await
            .expect_err(case);
        let shown = err.to_string();
        assert!(shown.contains(why), "{case}: {shown}");
        let spools = store.spools.lock().expect("spools").len();
        assert_eq!(spools, 0, "{case}: a spool stored after a failed copy");
    }
}
