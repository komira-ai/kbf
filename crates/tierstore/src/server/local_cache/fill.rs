//! A fill: the object read from the upstream into the stage directory, checked, and linked
//! into `obj/`; and the single flight that lets the readers of one key share it.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;

use futures::FutureExt as _;
use futures::future::{BoxFuture, Shared};
use sha2::{Digest, Sha256};
use tokio::fs::OpenOptions;
use tokio::io::AsyncWriteExt as _;

use crate::{ByteRange, Error};

use super::layout::{Name, Sidecar, data_path, meta_path, object_dir, stage_dir};
use super::metrics::{FailedFill, bump};
use super::{Disk, Inner, locked, percent_of};

/// How a fill ended, for the readers that waited for it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Filled {
    /// The object is in the cache.
    Kept,
    /// It is not: the readers are answered from the upstream.
    Skipped,
}

/// A fill that is running, which every reader of the key waits on.
pub(super) type Pending = Shared<BoxFuture<'static, Result<Filled, Error>>>;

/// Why a fill ended without an object.
#[derive(Debug)]
enum Stop {
    /// The upstream failed, or has no such object.
    Upstream(Error),
    /// The upstream gave fewer bytes than it had said the object has.
    Short,
    /// The cache directory could not be written.
    Local,
    /// The bytes of a key named by its content hash to another name.
    Mismatch,
}

impl From<io::Error> for Stop {
    fn from(_: io::Error) -> Self {
        Stop::Local
    }
}

impl From<Error> for Stop {
    fn from(error: Error) -> Self {
        Stop::Upstream(error)
    }
}

/// A file of the stage directory, removed when it is dropped, and counted while it exists.
struct Staged {
    inner: Arc<Inner>,
    file: tokio::fs::File,
    path: PathBuf,
    /// Where the sidecar is written before it is renamed into place.
    sidecar: PathBuf,
}

impl Staged {
    /// A new file, read-only for everyone once it is made (mode 0444: the descriptor that made
    /// it can still write).
    async fn create(inner: &Arc<Inner>) -> io::Result<Self> {
        let number = inner.staged.fetch_add(1, Ordering::Relaxed);
        let path = stage_dir(&inner.config.path).join(format!("{number:016x}"));
        let sidecar = stage_dir(&inner.config.path).join(format!("{number:016x}.meta"));
        let mut options = OpenOptions::new();
        let file = options
            .write(true)
            .create_new(true)
            .mode(0o444)
            .open(&path)
            .await?;
        let _ = inner.counters.staging_files.fetch_add(1, Ordering::Relaxed);
        Ok(Self {
            inner: Arc::clone(inner),
            file,
            path,
            sidecar,
        })
    }

    /// Writes the end of the file to the disk, so that a name linked to it is whole after a
    /// crash.
    async fn finish(&mut self) -> io::Result<()> {
        self.file.flush().await?;
        self.file.sync_data().await
    }
}

impl Drop for Staged {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
        let _ = std::fs::remove_file(&self.sidecar);
        let counter = &self.inner.counters.staging_files;
        let _ = counter.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Puts the staged file into the cache: the sidecar first, by rename, then the data file by a
/// create-only link. Gives whether the data file is new (`false` if it was there already).
fn publish(
    disk: &dyn Disk,
    root: &Path,
    hex: &str,
    staged: (&Path, &Path),
    sidecar: &[u8],
) -> io::Result<bool> {
    let data = data_path(root, hex);
    let (from, temporary) = staged;
    std::fs::create_dir_all(object_dir(root, hex))?;
    std::fs::write(temporary, sidecar)?;
    std::fs::rename(temporary, meta_path(root, hex))?;
    match disk.link(from, &data) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Ok(false),
        Err(error) => {
            // Nothing owns the sidecar: leave no file that says an object is there.
            let _ = std::fs::remove_file(meta_path(root, hex));
            Err(error)
        }
    }
}

impl Inner {
    /// Whether an object of `size` bytes may be added: it fits in the budget and the cache would
    /// not pass its critical mark.
    fn has_room(&self, size: u64) -> bool {
        let held = locked(&self.index).bytes();
        let critical = percent_of(self.config.budget, self.config.critical);
        size <= self.config.budget && held.saturating_add(size) <= critical
    }

    /// The fill of `key`, started if none is running, and waited for.
    pub(super) async fn fill_once(
        self: &Arc<Self>,
        key: &str,
        name: &Name,
    ) -> Result<Filled, Error> {
        let pending = {
            let mut flights = locked(&self.flights);
            flights
                .entry(name.hex.clone())
                .or_insert_with(|| self.start(key, name))
                .clone()
        };
        pending.await
    }

    /// Runs the fill on a task of its own, so that it ends even if every reader goes away, and
    /// takes it out of the table of the fills that run when it ends.
    fn start(self: &Arc<Self>, key: &str, name: &Name) -> Pending {
        let (inner, key, name) = (Arc::clone(self), key.to_owned(), name.clone());
        let task = tokio::spawn(async move {
            let filled = inner.fill(&key, &name).await;
            drop(locked(&inner.flights).remove(&name.hex));
            filled
        });
        let waiting = async move { task.await.expect("a fill does not panic") };
        waiting.boxed().shared()
    }

    /// Reads the object into the stage directory and, if it is whole and is what its name says,
    /// puts it in the cache.
    async fn fill(self: &Arc<Self>, key: &str, name: &Name) -> Result<Filled, Error> {
        match self.fetch(key, name).await {
            Ok(filled) => Ok(filled),
            Err(Stop::Upstream(error)) => {
                if !matches!(error, Error::NotFound(_)) {
                    self.counters.failed(FailedFill::UpstreamError);
                }
                Err(error)
            }
            Err(Stop::Short) => self.skipped(FailedFill::ShortBody),
            Err(Stop::Local) => self.skipped(FailedFill::LocalIo),
            Err(Stop::Mismatch) => self.skipped(FailedFill::HashMismatch),
        }
    }

    /// Counts a fill that failed for `why`; its readers go to the upstream.
    fn skipped(&self, why: FailedFill) -> Result<Filled, Error> {
        self.counters.failed(why);
        Ok(Filled::Skipped)
    }

    async fn fetch(self: &Arc<Self>, key: &str, name: &Name) -> Result<Filled, Stop> {
        let meta = self.upstream.head(key).await?;
        if !self.has_room(meta.size) {
            bump(&self.counters.fills_skipped);
            return Ok(Filled::Skipped);
        }
        let mut staged = Staged::create(self).await?;
        let mut hasher = Sha256::new();
        let mut offset = 0;
        while offset < meta.size {
            let want = self.config.fill_chunk.max(1).min(meta.size - offset);
            let range = ByteRange::new(offset, want);
            let chunk = self.upstream.get(key, Some(range)).await?;
            if chunk.len() as u64 != want {
                return Err(Stop::Short);
            }
            staged.file.write_all(&chunk).await?;
            hasher.update(&chunk);
            offset += want;
        }
        staged.finish().await?;
        let digest = hex::encode(hasher.finalize());
        if name
            .content
            .as_deref()
            .is_some_and(|wanted| wanted != digest)
        {
            return Err(Stop::Mismatch);
        }
        let sidecar = Sidecar {
            key: key.to_owned(),
            size: meta.size,
            sha256: digest,
            tag: meta.tag.clone(),
            modified: meta.modified,
        };
        let fresh = self.commit(&staged, name, &sidecar).await?;
        locked(&self.index).insert(&name.hex, meta.size, meta.tag, meta.modified);
        if fresh {
            bump(&self.counters.fills);
        }
        self.relieve().await;
        Ok(Filled::Kept)
    }

    /// Links the staged file into the cache, on a thread that may block.
    async fn commit(
        self: &Arc<Self>,
        staged: &Staged,
        name: &Name,
        sidecar: &Sidecar,
    ) -> io::Result<bool> {
        let disk = Arc::clone(&self.disk);
        let root = self.config.path.clone();
        let hex = name.hex.clone();
        let (from, temporary) = (staged.path.clone(), staged.sidecar.clone());
        let text = serde_json::to_vec(sidecar).unwrap_or_default();
        let publishing = tokio::task::spawn_blocking(move || {
            publish(&*disk, &root, &hex, (&from, &temporary), &text)
        });
        publishing.await.expect("a publish does not panic")
    }

    /// Runs a pass of the eviction if the cache is above its high water mark.
    async fn relieve(self: &Arc<Self>) {
        let held = locked(&self.index).bytes();
        if held > percent_of(self.config.budget, self.config.high_water) {
            let inner = Arc::clone(self);
            let pass = tokio::task::spawn_blocking(move || inner.evict_pass());
            pass.await.expect("a pass does not panic");
        }
    }
}
