//! [`Spool`]: an action's outputs gathered in one local file before they are uploaded
//! as whole segments (`docs/design/write-path.md`, section 4.1), so a tree of thousands
//! of files is not held in memory and is sent in a few large calls, not one per file.
//!
//! A spool is a [`Cas`]: a driver's walk stores its outputs into it as it would into
//! the engine, and then hands it to [`Cas::put_spool`] of the lease's store. Each
//! distinct blob is written once, at an offset reserved for it, by positioned writes,
//! so concurrent puts never interleave. A blob is recorded only after all its bytes are
//! written and hashed to its digest; a put that fails leaves dead bytes no record
//! names. The file is unlinked as soon as it is created: nothing is left on disk once
//! the spool is dropped, whatever ends the lease.

use std::collections::BTreeSet;
use std::fs::File;
use std::io;
use std::os::unix::fs::{FileExt as _, OpenOptionsExt as _};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use futures::StreamExt as _;
use kbf_proto::reapi::Digest;
use kbf_segments::Crc32c;
use kbf_types::{DigestFunction, LeaseId};
use sha2::{Digest as _, Sha256};

use crate::cas::{Cas, CasError, Chunks, label};

/// One blob in the spool.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Spooled {
    /// The blob's digest.
    pub digest: kbf_types::Digest,
    /// Where its bytes start in the spool file.
    pub offset: u64,
    /// The CRC-32C of its bytes.
    pub crc32c: u32,
}

/// Blobs written once each into one unlinked local file.
#[derive(Debug)]
pub struct Spool {
    file: Arc<File>,
    state: Mutex<State>,
}

#[derive(Debug, Default)]
struct State {
    /// Bytes reserved so far: where the next blob goes.
    len: u64,
    /// Blobs written whole, in the order they were finished.
    records: Vec<Spooled>,
    /// Digests written or being written, so each is written once.
    taken: BTreeSet<kbf_types::Digest>,
}

/// Names spool files no other spool of this process uses.
static NEXT: AtomicU64 = AtomicU64::new(0);

impl Spool {
    /// An empty spool in a new file under `dir`, unlinked at once.
    ///
    /// # Errors
    /// The file could not be created or unlinked.
    pub fn create(dir: &Path) -> io::Result<Self> {
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let path = dir.join(format!(".kbf-spool-{}-{n}", std::process::id()));
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)?;
        std::fs::remove_file(&path)?;
        Ok(Self {
            file: Arc::new(file),
            state: Mutex::new(State::default()),
        })
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        // Every change is one assignment or one push.
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The blobs written whole so far, in the order they were finished.
    #[must_use]
    pub fn records(&self) -> Vec<Spooled> {
        self.lock().records.clone()
    }

    /// Reserves `size` bytes for `digest`, or `None` if that blob is spooled already
    /// (or is being written).
    fn reserve(&self, digest: kbf_types::Digest) -> Option<u64> {
        let mut state = self.lock();
        if !state.taken.insert(digest) {
            return None;
        }
        let at = state.len;
        state.len += digest.size_bytes;
        Some(at)
    }

    /// Writes `bytes` at `offset` of the file, on the blocking pool.
    async fn write_at(&self, bytes: Vec<u8>, offset: u64) -> io::Result<()> {
        let file = Arc::clone(&self.file);
        blocking(move || file.write_all_at(&bytes, offset)).await
    }

    /// `len` bytes of the file from `offset`, read on the blocking pool.
    ///
    /// # Errors
    /// The read failed or the file is shorter.
    pub async fn read(&self, offset: u64, len: u64) -> io::Result<Vec<u8>> {
        let file = Arc::clone(&self.file);
        let len = usize::try_from(len).map_err(io::Error::other)?;
        blocking(move || {
            let mut bytes = vec![0; len];
            file.read_exact_at(&mut bytes, offset).map(|()| bytes)
        })
        .await
    }

    /// The spooled blob `digest` names, if it is in the spool.
    fn find(&self, digest: &kbf_types::Digest) -> Option<Spooled> {
        self.lock()
            .records
            .iter()
            .find(|r| r.digest == *digest)
            .copied()
    }

    fn record(&self, spooled: Spooled) {
        self.lock().records.push(spooled);
    }

    /// Gives up `digest`'s reservation after a failed write, so a later put of the same
    /// blob writes it again; its reserved bytes stay dead.
    fn release(&self, digest: &kbf_types::Digest) {
        self.lock().taken.remove(digest);
    }

    /// Writes the chunks of the blob `parsed` names at `offset`, and returns their
    /// CRC-32C once they hash to it.
    async fn write_chunks(
        &self,
        parsed: kbf_types::Digest,
        offset: u64,
        blob: &str,
        mut chunks: Chunks,
    ) -> Result<u32, CasError> {
        let (mut hasher, mut crc, mut at) = (Sha256::new(), Crc32c::new(), 0u64);
        while let Some(chunk) = chunks.next().await {
            let chunk = chunk.map_err(|e| CasError::Read(blob.to_owned(), e.to_string()))?;
            let len = chunk.len() as u64;
            if at + len > parsed.size_bytes {
                let size = parsed.size_bytes;
                return Err(CasError::Corrupt(
                    blob.to_owned(),
                    format!("more than {size} bytes"),
                ));
            }
            hasher.update(&chunk);
            crc.update(&chunk);
            self.write_at(chunk, offset + at)
                .await
                .map_err(|e| CasError::Read(blob.to_owned(), format!("spool: {e}")))?;
            at += len;
        }
        let actual = kbf_types::Digest::new(DigestFunction::Sha256, hasher.finalize().into(), at);
        if actual != parsed {
            return Err(CasError::Corrupt(
                blob.to_owned(),
                label(&to_proto(&actual)),
            ));
        }
        Ok(crc.finish())
    }
}

impl Cas for Spool {
    type Lease = Arc<Self>;

    /// The spool itself: it has nothing per lease.
    fn lease(self: &Arc<Self>, _lease: &LeaseId) -> Arc<Self> {
        Arc::clone(self)
    }

    /// A blob put into this spool; nothing else is in it.
    async fn get(&self, digest: &Digest) -> Result<Vec<u8>, CasError> {
        let missing = || CasError::Missing(label(digest));
        let parsed = parse(digest).ok_or_else(missing)?;
        let found = self.find(&parsed).ok_or_else(missing)?;
        self.read(found.offset, parsed.size_bytes)
            .await
            .map_err(|e| CasError::Unavailable(label(digest), format!("spool: {e}")))
    }

    /// Writes `bytes` into the spool, once per digest. Returns once they are in the
    /// spool's file, not in the CAS: [`Cas::put_spool`] stores them there.
    async fn put(&self, bytes: Vec<u8>) -> Result<Digest, CasError> {
        let digest = kbf_segments::sha256(&bytes);
        let proto = to_proto(&digest);
        let Some(offset) = self.reserve(digest) else {
            return Ok(proto);
        };
        let crc32c = kbf_segments::crc32c(&bytes);
        if let Err(e) = self.write_at(bytes, offset).await {
            self.release(&digest);
            return Err(CasError::Read(label(&proto), format!("spool: {e}")));
        }
        self.record(Spooled {
            digest,
            offset,
            crc32c,
        });
        Ok(proto)
    }

    /// Writes the chunks into the spool, checking they hash to `digest`; once per
    /// digest (a blob already spooled is not read again).
    async fn put_chunks(&self, digest: Digest, chunks: Chunks) -> Result<Digest, CasError> {
        let blob = label(&digest);
        let parsed = parse(&digest)
            .ok_or_else(|| CasError::Corrupt(blob.clone(), "not a SHA-256 digest".to_owned()))?;
        let Some(offset) = self.reserve(parsed) else {
            return Ok(digest);
        };
        match self.write_chunks(parsed, offset, &blob, chunks).await {
            Ok(crc32c) => {
                self.record(Spooled {
                    digest: parsed,
                    offset,
                    crc32c,
                });
                Ok(digest)
            }
            Err(e) => {
                self.release(&parsed);
                Err(e)
            }
        }
    }
}

/// `digest` as kbf's digest, if it is a SHA-256 digest with a size.
pub(crate) fn parse(digest: &Digest) -> Option<kbf_types::Digest> {
    kbf_types::Digest::parse(DigestFunction::Sha256, &label(digest)).ok()
}

/// kbf's digest as a REAPI digest.
pub(crate) fn to_proto(digest: &kbf_types::Digest) -> Digest {
    Digest {
        hash: digest.hash_hex(),
        // Sizes come from lengths in memory or on disk, far below `i64::MAX`.
        size_bytes: i64::try_from(digest.size_bytes).unwrap_or(i64::MAX),
    }
}

/// Runs `f` on the blocking pool; a task that did not finish is an I/O error.
async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> io::Result<T> + Send + 'static,
) -> io::Result<T> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(io::Error::other)
        .and_then(std::convert::identity)
}
