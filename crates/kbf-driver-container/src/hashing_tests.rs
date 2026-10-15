//! The hash of one file and of a set of files on the pool.

use super::*;

/// A fresh scratch directory for one unit test, beside the test binary, and the
/// directory open.
fn scratch(name: &str) -> (std::path::PathBuf, Arc<OwnedFd>) {
    let exe = std::env::current_exe().expect("test binary");
    let dir = exe
        .parent()
        .expect("deps directory")
        .join("kbf-driver-container-unit")
        .join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch");
    let fd = rustix::fs::openat(
        rustix::fs::CWD,
        &dir,
        crate::outputs::DIRECTORY,
        Mode::empty(),
    )
    .expect("open");
    (dir, Arc::new(fd))
}

fn job(dirs: &Arc<[String]>, name: &str, size: u64) -> Job {
    Job {
        dirs: Arc::clone(dirs),
        name: name.to_owned(),
        size,
    }
}

/// Catches a file read through a descriptor that is not a regular file (an entry
/// replaced by a FIFO between the listing and the open must fail, not block or be
/// stored as a file), the byte bound off by one or ignored (a file of exactly `max`
/// bytes is hashed, one byte more is refused), and a digest or CRC that is not the
/// bytes' (SHA-256 and CRC-32C of a known string).
#[test]
fn a_file_that_is_no_longer_regular_is_not_hashed() {
    let (dir, fd) = scratch("hashing-fifo");
    rustix::fs::mknodat(&*fd, "fifo", FileType::Fifo, Mode::RUSR | Mode::WUSR, 0).expect("mkfifo");
    let mut buffer = Vec::new();
    let why = hash_file(&fd, "fifo", u64::MAX, &mut buffer).expect_err("a FIFO");
    assert!(
        why.to_string().contains("no longer a regular file"),
        "{why}"
    );
    std::fs::write(dir.join("file"), b"bytes").expect("write");
    let hashed = hash_file(&fd, "file", 5, &mut buffer)
        .expect("a regular file")
        .expect("within the bound");
    assert_eq!((hashed.digest, hashed.crc32c), whole(b"bytes"));
    assert!(!hashed.executable);
    assert!(
        hash_file(&fd, "file", 4, &mut buffer)
            .expect("a regular file")
            .is_none()
    );
}

/// The digest and CRC-32C of `bytes` in one piece.
fn whole(bytes: &[u8]) -> (Digest, u32) {
    let mut hasher = RecordHasher::new();
    hasher.update(bytes);
    hasher.finish()
}

/// Catches the walk's old cost, a freshly zeroed megabyte for every file: one worker
/// buffer serves 200 small files and stays a few kilobytes, and grows to the chunk
/// size only for a file that needs it (mutant: the buffer sized to `CHUNK` at once).
#[test]
fn a_small_file_does_not_cost_a_chunk_of_buffer() {
    let (dir, fd) = scratch("hashing-buffer");
    let mut buffer = Vec::new();
    for n in 0..200 {
        std::fs::write(dir.join(format!("f{n}")), vec![n as u8; 100 + n]).expect("write");
        hash_file(&fd, &format!("f{n}"), u64::MAX, &mut buffer)
            .expect("hashed")
            .expect("fits");
    }
    assert!(buffer.len() <= 4096, "{} bytes of buffer", buffer.len());
    std::fs::write(dir.join("big"), vec![7u8; CHUNK + 5]).expect("write");
    let big = hash_file(&fd, "big", u64::MAX, &mut buffer)
        .expect("hashed")
        .expect("fits");
    assert_eq!(buffer.len(), CHUNK, "a large file reads a chunk at a time");
    assert_eq!(big.digest, whole(&vec![7u8; CHUNK + 5]).0);
}

/// Catches the hash running on one thread whatever the pool has: 64 files of 1 MiB
/// hashed on a pool of 4 are hashed on more than one thread, on a pool of 1 on
/// exactly one, and the outcomes are the same either way and in job order (mutant:
/// the worker count fixed at 1).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn files_are_hashed_on_as_many_threads_as_the_pool_has() {
    let (dir, upper) = scratch("hashing-threads");
    let dirs: Arc<[String]> = Arc::from(Vec::<String>::new());
    let jobs: Arc<[Job]> = (0..64)
        .map(|i| {
            let name = format!("f{i:02}");
            std::fs::write(dir.join(&name), vec![i as u8; 1 << 20]).expect("write");
            job(&dirs, &name, 1 << 20)
        })
        .collect();
    let mut digests = Vec::new();
    for threads in [1, 4] {
        let key = Arc::as_ptr(&upper) as usize;
        SEEN.lock().expect("seen").remove(&key);
        let pool = CpuPool::new(threads);
        let outcomes = hash_all(&pool, &upper, Arc::clone(&jobs), u64::MAX).await;
        let seen = SEEN.lock().expect("seen")[&key].len();
        if threads == 1 {
            assert_eq!(seen, 1);
        } else {
            assert!(seen > 1, "{seen} threads hashed with {threads} workers");
        }
        digests.push(
            outcomes
                .into_iter()
                .map(|o| o.expect("reached").expect("read").expect("fits").digest)
                .collect::<Vec<_>>(),
        );
    }
    assert_eq!(digests[0], digests[1]);
    for (i, digest) in digests[0].iter().enumerate() {
        assert_eq!(*digest, whole(&vec![i as u8; 1 << 20]).0);
    }
}

/// Catches a failure that is lost or hides the one before it: with one thread, a file
/// listed but gone fails with its own error, the files listed before it are hashed, the
/// ones after it are not reached (mutant: `first_bad` never set, the later files hashed),
/// and a file that does not fit the bytes left is `None`. A file in a directory that is
/// gone is that file's error too.
#[tokio::test]
async fn the_first_failure_stops_the_files_after_it() {
    let (dir, upper) = scratch("hashing-first-bad");
    let dirs: Arc<[String]> = Arc::from(Vec::<String>::new());
    for name in ["a", "c"] {
        std::fs::write(dir.join(name), b"12345").expect("write");
    }
    let jobs: Arc<[Job]> = vec![job(&dirs, "a", 5), job(&dirs, "b", 5), job(&dirs, "c", 5)].into();
    let pool = CpuPool::new(1);
    let outcomes = hash_all(&pool, &upper, Arc::clone(&jobs), u64::MAX).await;
    assert!(matches!(outcomes[0], Some(Ok(Some(_)))));
    assert!(matches!(outcomes[1], Some(Err(_))), "{:?}", outcomes[1]);
    assert!(outcomes[2].is_none(), "a file after the failure was hashed");
    // Bytes: 5 + 5 do not fit 9; with one thread the first fits and the second is None.
    let jobs: Arc<[Job]> = vec![job(&dirs, "a", 5), job(&dirs, "c", 5)].into();
    let outcomes = hash_all(&pool, &upper, jobs, 9).await;
    assert!(matches!(outcomes[0], Some(Ok(Some(_)))));
    assert!(matches!(outcomes[1], Some(Ok(None))));
    // A directory that is gone, then the same directory name for two files in a row.
    let elsewhere: Arc<[String]> = Arc::from(vec!["gone".to_owned()]);
    let jobs: Arc<[Job]> = vec![job(&elsewhere, "a", 5)].into();
    let outcomes = hash_all(&pool, &upper, jobs, u64::MAX).await;
    let why = outcomes[0]
        .as_ref()
        .expect("reached")
        .as_ref()
        .expect_err("gone");
    assert!(why.to_string().contains("removed or replaced"), "{why}");
}

/// Catches a file that holds more than its size says, read as if it did not: a file
/// whose `st_size` is 0 but holds bytes (procfs) is refused when it is hashed through,
/// because what was read is not the size it was stamped with, and is `None` when the
/// bytes left are fewer than it holds, because the read is bounded by the bytes, not by
/// `st_size` (a file can grow). Mutants: the bound taken from `st_size` only (the second
/// is hashed whole), the size compared to the stamp's not looked at (the first is
/// stored under the digest of what was read).
#[test]
fn a_file_that_holds_more_than_its_size_says_is_not_trusted() {
    let proc = rustix::fs::openat(
        rustix::fs::CWD,
        format!("/proc/{}", std::process::id()),
        crate::outputs::DIRECTORY,
        Mode::empty(),
    )
    .expect("open /proc/self");
    let mut buffer = Vec::new();
    let why = hash_file(&proc, "status", u64::MAX, &mut buffer).expect_err("size differs");
    assert!(why.to_string().contains("changed while"), "{why}");
    let within = hash_file(&proc, "status", 100, &mut buffer).expect("read");
    assert!(within.is_none(), "more than 100 bytes read");
}

/// Catches work that goes on after nobody waits for it: once the guard of a hash is
/// dropped (the future was dropped, as the upload's deadline does), a worker skips every
/// file but the first (mutant: the guard's drop does nothing, so all 500 are hashed).
#[test]
fn workers_skip_the_files_of_a_hash_that_was_dropped() {
    let (dir, upper) = scratch("hashing-cancel");
    let dirs: Arc<[String]> = Arc::from(Vec::<String>::new());
    let jobs: Arc<[Job]> = (0..500)
        .map(|i| {
            let name = format!("f{i:04}");
            std::fs::write(dir.join(&name), vec![1u8; 100]).expect("write");
            job(&dirs, &name, 100)
        })
        .collect();
    let shared = Arc::new(Shared {
        upper: Arc::clone(&upper),
        order: (0..jobs.len()).collect(),
        jobs,
        next: AtomicUsize::new(0),
        first_bad: AtomicUsize::new(usize::MAX),
        max: u64::MAX,
        used: AtomicU64::new(0),
    });
    drop(CancelOnDrop(Arc::clone(&shared)));
    let hashed = work(&shared).len();
    assert!(hashed <= 1, "{hashed} of 500 files hashed after the drop");
}
