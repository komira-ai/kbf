//! The read cache: blobs on a local disk, bounded in bytes, least recently used out first.
//!
//! One file per blob, named `<hash in hex>-<size>`, in one directory that one process owns.
//! A file is written under a temporary name and renamed into place, so a crash leaves either
//! no file or a whole one, plus a temporary file that the next [`ReadCache::open`] removes.
//! Nothing is synced: a torn or missing file is found by its length when the cache opens and
//! by its digest when it is read, and a blob that fails either is dropped and fetched again.
//! The cache holds nothing that is not also in the pack index and the object store.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::UNIX_EPOCH;

use bytes::Bytes;
use kbf_pack_index::BlobDigest;

use crate::digest::verifies;
use crate::error::CacheError;

/// The end of the name of a file that is being written.
const TEMP_SUFFIX: &str = ".tmp";

/// The cache's accounts: which blobs it holds and in which order they were used.
#[derive(Default)]
struct State {
    /// Each blob held, with the tick of its last use.
    entries: BTreeMap<BlobDigest, u64>,
    /// The same blobs by the tick of their last use, so the least recently used is first.
    order: BTreeMap<u64, BlobDigest>,
    /// The bytes of the blobs held.
    used: u64,
    /// The tick of the latest use.
    tick: u64,
}

impl State {
    /// Makes `digest` the most recently used blob, holding it if it was not held.
    fn use_entry(&mut self, digest: BlobDigest) {
        self.tick += 1;
        match self.entries.insert(digest, self.tick) {
            Some(old) => {
                self.order.remove(&old);
            }
            None => self.used += digest.size,
        }
        self.order.insert(self.tick, digest);
    }

    /// Makes `digest` the most recently used blob if it is held; a blob that is not held (it
    /// was evicted since the caller looked) changes nothing.
    fn touch(&mut self, digest: &BlobDigest) {
        let held = self.entries.contains_key(digest);
        // `held` is false for a blob not held: nothing is inserted and no bytes are charged.
        let _ = held.then(|| self.use_entry(*digest));
    }

    /// Stops holding `digest`; a blob that is not held changes nothing.
    fn remove(&mut self, digest: &BlobDigest) {
        let tick = self.entries.remove(digest);
        // `tick` is `None` for a blob not held: nothing leaves the order and no bytes are freed.
        let _ = tick.and_then(|tick| self.order.remove(&tick));
        self.used -= u64::from(tick.is_some()) * digest.size;
    }

    /// Stops holding the least recently used blobs until at most `budget` bytes are held, and
    /// returns them.
    fn evict_to(&mut self, budget: u64) -> Vec<BlobDigest> {
        let mut evicted = Vec::new();
        while self.used > budget {
            let (_, digest) = self
                .order
                .pop_first()
                .expect("bytes are held only while a blob is");
            self.remove(&digest);
            evicted.push(digest);
        }
        evicted
    }
}

/// What a file of the cache directory is, going by its name and its length.
enum Slot {
    /// A whole blob of the cache.
    Whole(BlobDigest),
    /// A blob of the wrong length, or a temporary file: the cache made it, and it is no use.
    Debris,
    /// A file the cache did not make: left alone.
    Foreign,
}

/// A bounded on-disk cache of blobs by digest.
pub struct ReadCache {
    dir: PathBuf,
    budget: u64,
    state: Mutex<State>,
    /// Numbers the temporary files.
    temp: AtomicU64,
}

impl ReadCache {
    /// Opens the cache in `dir`, which is made if it is absent, holding at most `budget`
    /// bytes of blobs. The blobs already there are kept: a file that is a whole blob of the
    /// cache (its name says the digest and the file has the length the digest says) is held,
    /// the one modified longest ago being the least recently used; a temporary file (named as
    /// [`ReadCache::put`] names one) and a file of the wrong length are removed; any other file,
    /// and a file whose metadata cannot be read, is left alone. If the blobs held are more
    /// than `budget`, the least recently used are removed. The digests are not checked here
    /// but on each read.
    ///
    /// A directory the call makes, and every file the cache writes, can be used by its owner
    /// alone (mode 0700 and 0600, less what the umask takes), so that other users of the
    /// machine cannot read the blobs. A directory that exists keeps its mode.
    ///
    /// One process may use a directory at a time.
    ///
    /// # Errors
    /// [`CacheError::Io`] when the directory cannot be made or listed.
    pub fn open(dir: impl Into<PathBuf>, budget: u64) -> Result<Self, CacheError> {
        let dir = dir.into();
        let listing = fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&dir)
            .and_then(|()| fs::read_dir(&dir))
            .map_err(|source| CacheError::Io {
                op: "open",
                path: dir.clone(),
                source,
            })?;
        // A file whose metadata cannot be read is skipped, not taken for debris.
        let found = listing
            .flatten()
            .filter_map(|item| item.metadata().ok().map(|meta| (item, meta)));
        let mut whole = Vec::new();
        for (item, meta) in found {
            let name = item.file_name().to_string_lossy().into_owned();
            match classify(&name, meta.len()) {
                Slot::Whole(digest) => {
                    let modified = meta.modified().unwrap_or(UNIX_EPOCH);
                    whole.push((modified, digest));
                }
                Slot::Debris => {
                    let _ = fs::remove_file(item.path());
                }
                Slot::Foreign => {}
            }
        }
        whole.sort();
        let mut state = State::default();
        for (_, digest) in whole {
            state.use_entry(digest);
        }
        for digest in state.evict_to(budget) {
            let _ = fs::remove_file(dir.join(file_name(&digest)));
        }
        Ok(Self {
            dir,
            budget,
            state: Mutex::new(state),
            temp: AtomicU64::new(0),
        })
    }

    /// The path of the file that holds (or would hold) the blob `digest`.
    #[must_use]
    pub fn path_of(&self, digest: &BlobDigest) -> PathBuf {
        self.dir.join(file_name(digest))
    }

    /// The bytes of the blobs the cache holds.
    #[must_use]
    pub fn used_bytes(&self) -> u64 {
        self.lock().used
    }

    /// How many blobs the cache holds.
    #[must_use]
    pub fn entries(&self) -> usize {
        self.lock().entries.len()
    }

    /// The blob `digest`, checked against the digest, or `None` when the cache does not hold
    /// it. A file that cannot be read, or whose bytes are not the blob, is removed from the
    /// cache and answers `None`: the caller fetches the blob again. The blob becomes the most
    /// recently used.
    #[must_use]
    pub fn get(&self, digest: &BlobDigest) -> Option<Bytes> {
        let held = self.lock().entries.contains_key(digest);
        if !held {
            return None;
        }
        let read = fs::read(self.path_of(digest))
            .ok()
            .filter(|bytes| verifies(digest, bytes));
        match read {
            Some(bytes) => {
                self.lock().touch(digest);
                Some(Bytes::from(bytes))
            }
            None => {
                self.lock().remove(digest);
                let _ = fs::remove_file(self.path_of(digest));
                None
            }
        }
    }

    /// Keeps the blob `digest`, whose bytes are `bytes`, and removes the least recently used
    /// blobs until the cache is within its budget. A blob larger than the budget is not kept,
    /// and one the cache holds already only becomes the most recently used.
    ///
    /// # Errors
    /// [`CacheError::Mismatch`] when `bytes` are not the blob `digest` names;
    /// [`CacheError::Io`] when the file cannot be written.
    pub fn put(&self, digest: &BlobDigest, bytes: &[u8]) -> Result<(), CacheError> {
        if !verifies(digest, bytes) {
            return Err(CacheError::Mismatch { digest: *digest });
        }
        if digest.size > self.budget {
            return Ok(());
        }
        let held = self.lock().entries.contains_key(digest);
        if held {
            self.lock().touch(digest);
            return Ok(());
        }
        let path = self.path_of(digest);
        let number = self.temp.fetch_add(1, Ordering::Relaxed);
        let temp = self
            .dir
            .join(format!("{}.{number}{TEMP_SUFFIX}", file_name(digest)));
        let written = write_private(&temp, bytes).and_then(|()| fs::rename(&temp, &path));
        if let Err(source) = written {
            let _ = fs::remove_file(&temp);
            return Err(CacheError::Io {
                op: "write",
                path,
                source,
            });
        }
        let evicted = {
            let mut state = self.lock();
            state.use_entry(*digest);
            state.evict_to(self.budget)
        };
        for gone in evicted {
            let _ = fs::remove_file(self.path_of(&gone));
        }
        Ok(())
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// The name of the file that holds `digest`.
fn file_name(digest: &BlobDigest) -> String {
    format!("{}-{}", hex::encode(digest.hash), digest.size)
}

/// The digest a file name says, or `None` when the name is not exactly what [`file_name`]
/// makes.
fn parse_name(name: &str) -> Option<BlobDigest> {
    let (hash_hex, size) = name.split_once('-')?;
    let size = size.parse().ok()?;
    let mut hash = [0_u8; 32];
    hex::decode_to_slice(hash_hex, &mut hash).ok()?;
    let digest = BlobDigest { hash, size };
    (file_name(&digest) == name).then_some(digest)
}

/// Whether `name` is what [`ReadCache::put`] calls a file it is writing: the name of a blob's
/// file, a dot, a number and the temporary suffix.
fn is_temp(name: &str) -> bool {
    let stem = name.strip_suffix(TEMP_SUFFIX);
    let split = stem.and_then(|stem| stem.rsplit_once('.'));
    let made = split.and_then(|(blob, number)| {
        let number: Option<u64> = number.parse().ok();
        number.and(parse_name(blob))
    });
    made.is_some()
}

/// What the file `name` of `len` bytes is.
fn classify(name: &str, len: u64) -> Slot {
    match parse_name(name) {
        Some(digest) if digest.size == len => Slot::Whole(digest),
        Some(_) => Slot::Debris,
        None if is_temp(name) => Slot::Debris,
        None => Slot::Foreign,
    }
}

/// Writes `bytes` to the new file `path`, which only the owner can read or change.
fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(bytes)
}

#[cfg(test)]
mod tests {
    use kbf_pack_index::BlobDigest;

    use super::State;

    fn digest(fill: u8, size: u64) -> BlobDigest {
        BlobDigest {
            hash: [fill; 32],
            size,
        }
    }

    /// Asserts that touching a blob that is not held changes nothing: no bytes are charged and
    /// no entry is made.
    #[test]
    fn touching_a_blob_that_is_not_held_changes_nothing() {
        let mut state = State::default();
        state.use_entry(digest(1, 7));

        state.touch(&digest(2, 9));

        assert_eq!(state.used, 7);
        assert_eq!(state.entries.len(), 1);
        assert_eq!(state.order.len(), 1);
    }

    /// Asserts that touching a held blob makes it the most recently used: the other blob is the
    /// one evicted first.
    #[test]
    fn touching_a_held_blob_makes_it_the_most_recent() {
        let (a, b) = (digest(1, 7), digest(2, 9));
        let mut state = State::default();
        state.use_entry(a);
        state.use_entry(b);

        state.touch(&a);

        assert_eq!(state.evict_to(7), [b]);
        assert_eq!(state.used, 7);
    }
}
