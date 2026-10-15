//! Hashing the files of an output tree, once each, on the [`CpuPool`].
//!
//! The listing of an output tree ([`crate::outputs`]) records every regular file it finds
//! as a [`Job`], in walk order, and keeps no descriptor of it. [`hash_all`] then hashes
//! them on up to `--cpu-threads` of the pool's threads and returns the outcomes in job
//! order, so the Directory messages built from them are the same whatever the number of
//! threads.
//!
//! This is the only time the daemon reads an output file to hash it. One pass over the
//! bytes computes the SHA-256 and the CRC-32C the segment footer needs
//! ([`RecordHasher`]); nothing is copied anywhere, and the upload reads the file again
//! only to send it, by the [`Stamp`] taken here.
//!
//! Each worker reuses one read buffer for all its files, grown to the largest file it has
//! met up to [`CHUNK`], not one zeroed megabyte per file, and keeps the descriptor of the
//! directory it read last: a file is opened below that directory with `O_NOFOLLOW`, the
//! directory reached from the upper directory one component at a time with `O_NOFOLLOW`
//! ([`descend`]). Workers take files in runs, so the files of one directory mostly share
//! a descriptor. A file of [`LARGE_FILE`] bytes or more is taken first, so the one long
//! hash does not start last.
//!
//! The first file that fails stops the workers from taking any file listed after it; the
//! caller reads the outcomes in job order and stops at the first problem, as hashing one
//! file at a time did. The bytes budget is shared: each file is read at most as far as
//! what the files hashed so far left of it, so a tree far past the budget is not read to
//! its end.

use std::os::fd::OwnedFd;
use std::os::unix::fs::FileExt as _;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use kbf_daemon::cas::{OPEN_FILE, RecordHasher, Stamp, descend};
use kbf_daemon::cpu_pool::CpuPool;
use kbf_types::Digest;
use rustix::fs::{FileType, Mode};

/// How much of a file a worker reads at a time: the most of one it holds in memory.
pub(crate) const CHUNK: usize = 1 << 20;

/// A file at least this large is hashed first.
pub(crate) const LARGE_FILE: u64 = 8 << 20;

/// How many files a worker takes at a time.
const RUN: usize = 16;

/// A file the listing found, to be hashed.
#[derive(Debug)]
pub(crate) struct Job {
    /// The directories from the upper directory down to the file's, by name. Files of
    /// one directory share the allocation.
    pub dirs: Arc<[String]>,
    pub name: String,
    /// Its size when it was listed; only decides the order of work.
    pub size: u64,
}

/// A regular file, hashed and closed.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Digested {
    pub digest: Digest,
    pub crc32c: u32,
    pub executable: bool,
    /// What the file looked like while it was hashed: the upload refuses a file that no
    /// longer matches.
    pub stamp: Stamp,
}

/// What hashing one file came to: its digest, `None` when it did not fit the byte
/// budget, or an error.
pub(crate) type Outcome = std::io::Result<Option<Digested>>;

/// Hashes the regular file `name` in `dir`, reading at most `max + 1` bytes; `None` when
/// it holds more than `max`. `buffer` is the worker's, grown as needed. The type is
/// checked on the open descriptor: an entry that stopped being a regular file after it
/// was listed is an error, never read. The file is stamped before and after the read:
/// one that changed while it was hashed is an error too.
pub(crate) fn hash_file(dir: &OwnedFd, name: &str, max: u64, buffer: &mut Vec<u8>) -> Outcome {
    let fd = rustix::fs::openat(dir, name, OPEN_FILE, Mode::empty())?;
    let stat = rustix::fs::fstat(&fd)?;
    #[allow(clippy::unnecessary_cast)] // `st_mode` is u32 on Linux, u16 on macOS
    let mode = stat.st_mode as u32;
    if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile {
        return Err(std::io::Error::other(
            "is no longer a regular file once opened",
        ));
    }
    let executable = mode & 0o111 != 0;
    let before = Stamp::of(&fd)?;
    if before.size > max {
        return Ok(None);
    }
    let file = std::fs::File::from(fd);
    // A file of `n` bytes needs no more than `n + 1` bytes of buffer to be read through
    // to its end; a large one is read a chunk at a time.
    let want = usize::try_from(before.size.saturating_add(1)).map_or(CHUNK, |n| n.min(CHUNK));
    if buffer.len() < want {
        buffer.resize(want, 0);
    }
    let mut hasher = RecordHasher::new();
    let mut size = 0u64;
    loop {
        // One byte past `max` is enough to tell the file is too large; the bound is on
        // the bytes read, not on `st_size` (a file can grow).
        let room = max.saturating_add(1).saturating_sub(size);
        let len = usize::try_from(room).map_or(buffer.len(), |n| n.min(buffer.len()));
        let read = file.read_at(&mut buffer[..len], size)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        size += read as u64;
        if size > max {
            return Ok(None);
        }
    }
    // Both parts are looked at, whichever is true: no short-circuit to cover.
    let changed = (Stamp::of(&file)? != before) | (size != before.size);
    if changed {
        return Err(std::io::Error::other("changed while it was being hashed"));
    }
    let (digest, crc32c) = hasher.finish();
    Ok(Some(Digested {
        digest,
        crc32c,
        executable,
        stamp: before,
    }))
}

/// The directory a worker read last.
struct Held {
    dirs: Arc<[String]>,
    fd: OwnedFd,
}

/// Hashes `job` below `upper`, reusing `held` when the file's directory is the one it
/// holds.
fn hash_job(
    upper: &OwnedFd,
    job: &Job,
    max: u64,
    held: &mut Option<Held>,
    buffer: &mut Vec<u8>,
) -> Outcome {
    let dir = match held.take() {
        Some(h) if Arc::ptr_eq(&h.dirs, &job.dirs) => h,
        _ => Held {
            dirs: Arc::clone(&job.dirs),
            fd: descend(upper, &job.dirs)?.ok_or_else(gone)?,
        },
    };
    let dir = held.insert(dir);
    hash_file(&dir.fd, &job.name, max, buffer)
}

fn gone() -> std::io::Error {
    std::io::Error::other("was removed or replaced since it was listed")
}

struct Shared {
    upper: Arc<OwnedFd>,
    jobs: Arc<[Job]>,
    /// Indexes into `jobs`, in the order they are taken.
    order: Vec<usize>,
    next: AtomicUsize,
    /// The lowest index of a job that failed or did not fit: later jobs are skipped.
    /// Dropping the hash before it is done sets it to 0, which skips all but the first.
    first_bad: AtomicUsize,
    max: u64,
    /// Bytes of the files hashed so far.
    used: AtomicU64,
}

/// The threads that hashed, for the test that counts them: by the address of the upper
/// directory's `Arc`, so one hash is told from the others that run beside it.
#[cfg(test)]
pub(crate) static SEEN: std::sync::LazyLock<
    std::sync::Mutex<
        std::collections::HashMap<usize, std::collections::HashSet<std::thread::ThreadId>>,
    >,
> = std::sync::LazyLock::new(Default::default);

fn work(shared: &Shared) -> Vec<(usize, Outcome)> {
    #[cfg(test)]
    SEEN.lock()
        .expect("seen")
        .entry(Arc::as_ptr(&shared.upper) as usize)
        .or_default()
        .insert(std::thread::current().id());
    let mut buffer = Vec::new();
    let mut held = None;
    let mut done = Vec::new();
    loop {
        let start = shared.next.fetch_add(RUN, Ordering::Relaxed);
        let Some(run) = shared.order.get(start..) else {
            break;
        };
        for &index in run.iter().take(RUN) {
            if index > shared.first_bad.load(Ordering::Relaxed) {
                continue;
            }
            let left = shared
                .max
                .saturating_sub(shared.used.load(Ordering::Relaxed));
            let outcome = hash_job(
                &shared.upper,
                &shared.jobs[index],
                left,
                &mut held,
                &mut buffer,
            );
            match &outcome {
                Ok(Some(d)) => {
                    shared
                        .used
                        .fetch_add(d.digest.size_bytes, Ordering::Relaxed);
                }
                _ => {
                    shared.first_bad.fetch_min(index, Ordering::Relaxed);
                }
            }
            done.push((index, outcome));
        }
    }
    done
}

/// Stops the workers when the hash is dropped before it is done.
struct CancelOnDrop(Arc<Shared>);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.first_bad.fetch_min(0, Ordering::Relaxed);
    }
}

/// Hashes `jobs` on up to `pool.threads()` of the pool's threads, the files together
/// holding at most `max` bytes. The outcome of `jobs[i]` is at `[i]`; `None` is a file
/// no worker hashed: one listed before it failed or did not fit, or the worker that had
/// it panicked.
pub(crate) async fn hash_all(
    pool: &CpuPool,
    upper: &Arc<OwnedFd>,
    jobs: Arc<[Job]>,
    max: u64,
) -> Vec<Option<Outcome>> {
    let mut order: Vec<usize> = (0..jobs.len()).collect();
    // Stable: large files first, each group in walk order.
    order.sort_by_key(|&i| jobs[i].size < LARGE_FILE);
    let workers = pool.threads().min(jobs.len()).max(1);
    let shared = Arc::new(Shared {
        upper: Arc::clone(upper),
        jobs: Arc::clone(&jobs),
        order,
        next: AtomicUsize::new(0),
        first_bad: AtomicUsize::new(usize::MAX),
        max,
        used: AtomicU64::new(0),
    });
    let _guard = CancelOnDrop(Arc::clone(&shared));
    let runs: Vec<_> = (0..workers)
        .map(|_| {
            let shared = Arc::clone(&shared);
            pool.run(move || work(&shared))
        })
        .collect();
    let mut outcomes: Vec<Option<Outcome>> = jobs.iter().map(|_| None).collect();
    // A worker that panicked leaves its files without an outcome.
    for done in futures::future::join_all(runs).await.into_iter().flatten() {
        for (index, outcome) in done {
            outcomes[index] = Some(outcome);
        }
    }
    outcomes
}

#[cfg(test)]
#[path = "hashing_tests.rs"]
mod tests;
