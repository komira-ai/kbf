//! A fill: the object read from the upstream into the stage directory, checked, and linked
//! into `obj/`; and the single flight that lets the readers of one key share it.

use std::collections::hash_map::Entry;
use std::io;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;

use futures::FutureExt as _;
use futures::future::{BoxFuture, OptionFuture, Shared};
use sha2::{Digest, Sha256};
use tokio::fs::OpenOptions;
use tokio::io::AsyncWriteExt as _;
use tokio::sync::{OwnedSemaphorePermit, oneshot};
use tokio::task::JoinHandle;

use crate::{ByteRange, Error};

use super::layout::{Name, Sidecar, data_path, make_dirs, meta_path, object_dir, stage_dir};
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

/// The task of a fill.
type Task = JoinHandle<Result<Filled, Error>>;

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
    /// The object changed while it was being read: the bytes may be of two versions.
    Changed,
    /// The cache lost the lock of its directory while the fill ran: nothing more is written.
    Lost,
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
pub(super) struct Staged {
    inner: Arc<Inner>,
    pub(super) file: tokio::fs::File,
    path: PathBuf,
    /// Where the sidecar is written before it is renamed into place.
    sidecar: PathBuf,
}

impl Staged {
    /// A new file, read-only for everyone once it is made (mode 0444: the descriptor that made
    /// it can still write).
    pub(super) async fn create(inner: &Arc<Inner>) -> io::Result<Self> {
        let number = inner.staged.fetch_add(1, Ordering::Relaxed);
        let name = format!("{}-{number:016x}", inner.stage_prefix);
        let path = stage_dir(&inner.config.path).join(&name);
        let sidecar = stage_dir(&inner.config.path).join(format!("{name}.meta"));
        let mut options = OpenOptions::new();
        let file = options
            .write(true)
            .create_new(true)
            .mode(0o444)
            .open(&path)
            .await?;
        // The mode given to `open` is cut by the umask: set it, so that it is 0444 for any.
        file.set_permissions(std::fs::Permissions::from_mode(0o444))
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
    pub(super) async fn finish(&mut self) -> io::Result<()> {
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

/// Writes the sidecar at `path`, readable by everyone and writable by the owner whatever the
/// umask.
fn write_sidecar(path: &Path, text: &[u8]) -> io::Result<()> {
    std::fs::write(path, text)
        .and_then(|()| std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o644)))
}

/// What [`publish`] did.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Published {
    /// The data file is new.
    New,
    /// The data file was there already.
    Existed,
    /// The cache does not own the directory any more, and nothing of the object is in it.
    Lost,
}

/// Puts the staged file into the cache: the sidecar first, by rename, then the data file by a
/// create-only link. `owns` is asked before the first change to the directory and again before
/// the rename, the first step that puts the object in it: a cache that lost the lock of the
/// directory since the fill started publishes nothing ([`Published::Lost`]).
fn publish(
    disk: &dyn Disk,
    root: &Path,
    hex: &str,
    staged: (&Path, &Path),
    sidecar: &[u8],
    owns: &dyn Fn() -> bool,
) -> io::Result<Published> {
    let data = data_path(root, hex);
    let (from, temporary) = staged;
    let meta = meta_path(root, hex);
    if !owns() {
        return Ok(Published::Lost);
    }
    make_dirs(root, &object_dir(root, hex)).and_then(|()| write_sidecar(temporary, sidecar))?;
    if !owns() {
        return Ok(Published::Lost);
    }
    std::fs::rename(temporary, &meta)?;
    match disk.link(from, &data) {
        Ok(()) => Ok(Published::New),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Ok(Published::Existed),
        Err(error) => {
            // Nothing owns the sidecar: leave no file that says an object is there.
            let _ = std::fs::remove_file(meta_path(root, hex));
            Err(error)
        }
    }
}

/// The bytes of a fill that is running, counted against the critical mark from the moment the
/// fill starts until its object is in the index (or it ends without one).
pub(super) struct Reserved {
    inner: Arc<Inner>,
    size: u64,
}

impl Drop for Reserved {
    fn drop(&mut self) {
        let _ = self.inner.reserved.fetch_sub(self.size, Ordering::SeqCst);
    }
}

/// A fill's place in the table of the fills that run: taken out when it is dropped, whether the
/// fill ended, panicked or was cancelled, so that no key stays marked as being filled.
pub(super) struct Flight {
    pub(super) inner: Arc<Inner>,
    pub(super) hex: String,
}

impl Drop for Flight {
    fn drop(&mut self) {
        let gone = {
            let mut flights = locked(&self.inner.flights);
            // The mark of a delete is of this fill only: it goes with it (lock order: flights,
            // then invalidated).
            locked(&self.inner.invalidated).remove(&self.hex);
            flights.remove(&self.hex)
        };
        drop(gone);
    }
}

impl Inner {
    /// Reserves room for an object of `size` bytes if it fits in the budget and the cache, with
    /// the fills that are running, would not pass its critical mark.
    pub(super) fn reserve(self: &Arc<Self>, size: u64) -> Option<Reserved> {
        self.reserve_between(size, || {})
    }

    /// [`reserve`](Self::reserve), running `between` after the bytes are reserved and before the
    /// index is read. The order matters: a fill that commits puts its object in the index before
    /// it drops its reservation, so an object is in the reserved bytes or in the index (possibly
    /// both, which only makes the count high), never in neither. A refused reservation
    /// is counted while it is held, so a reservation made at the same time can be refused for it:
    /// that costs a skipped fill, never room.
    pub(super) fn reserve_between(
        self: &Arc<Self>,
        size: u64,
        between: impl FnOnce(),
    ) -> Option<Reserved> {
        let before = self.reserved.fetch_add(size, Ordering::SeqCst);
        // A refused reservation is dropped, which gives the bytes back.
        let reservation = Reserved {
            inner: Arc::clone(self),
            size,
        };
        between();
        let held = locked(&self.index).bytes();
        let critical = percent_of(self.config.budget, self.config.critical);
        let total = held.saturating_add(before).saturating_add(size);
        (size <= self.config.budget && total <= critical).then_some(reservation)
    }

    /// The fill of `key`, started if none is running, and waited for. A key with no fill running
    /// when all the fills the cache may run are running is not filled: the answer is
    /// [`Filled::Skipped`] at once, and the reader goes to the upstream.
    pub(super) async fn fill_once(
        self: &Arc<Self>,
        key: &str,
        name: &Name,
    ) -> Result<Filled, Error> {
        match self.join_or_start(key, name) {
            Ok(pending) => pending.await,
            Err(filled) => Ok(filled),
        }
    }

    /// The fill of `key` to wait for (`Ok`), started if none is running; or (`Err`) the answer
    /// that needs no wait. It does not wait, and it holds no lock when it returns.
    ///
    /// The fill is started after the table of the fills that run is unlocked. The task is
    /// spawned with its [`Flight`], and a spawn that drops the task at once (the runtime is
    /// shutting down) drops the flight on this thread, which locks the table: with the table
    /// locked by this thread that is a deadlock. The entry is therefore put in the table first,
    /// with a fill that waits for the task, and the task is handed to it after the unlock.
    pub(super) fn join_or_start(
        self: &Arc<Self>,
        key: &str,
        name: &Name,
    ) -> Result<Pending, Filled> {
        let (pending, launch) = {
            let mut flights = locked(&self.flights);
            // A fill that ended after the reader missed, and before it got here, left the object
            // in the index (it is put there before the flight is removed): that is not a miss
            // to fill again.
            if locked(&self.index).get(&name.hex).is_some() {
                return Err(Filled::Kept);
            }
            match flights.entry(name.hex.clone()) {
                Entry::Occupied(running) => (running.get().clone(), None),
                Entry::Vacant(slot) => {
                    let Ok(permit) = Arc::clone(&self.permits).try_acquire_owned() else {
                        bump(&self.counters.fills_skipped);
                        bump(&self.counters.fills_skipped_busy);
                        return Err(Filled::Skipped);
                    };
                    let (sender, receiver) = oneshot::channel();
                    let pending = self.waiting(receiver);
                    slot.insert(pending.clone());
                    (pending, Some((permit, sender)))
                }
            }
        };
        if let Some((permit, sender)) = launch {
            self.start(key, name, permit, sender);
        }
        Ok(pending)
    }

    /// The fill that waits for the task that `receiver` is to be given, and for its end. A task
    /// that did not end by itself is an error of the readers that waited for it, and is counted.
    fn waiting(self: &Arc<Self>, receiver: oneshot::Receiver<Task>) -> Pending {
        let inner = Arc::clone(self);
        let waiting = async move {
            let task = OptionFuture::from(receiver.await.ok()).await;
            match task.and_then(Result::ok) {
                Some(filled) => filled,
                None => {
                    inner.counters.failed(FailedFill::Aborted);
                    let text = "the fill of the object did not finish";
                    Err(Error::Unavailable(text.to_owned()))
                }
            }
        };
        waiting.boxed().shared()
    }

    /// Runs the fill on a task of its own, so that it ends even if every reader goes away, and
    /// takes it out of the table of the fills that run when it ends, panics or is cancelled. The
    /// permit is held until then. The task is sent to the fill that waits for it. The table must
    /// not be locked by the caller (see [`join_or_start`](Self::join_or_start)).
    fn start(
        self: &Arc<Self>,
        key: &str,
        name: &Name,
        permit: OwnedSemaphorePermit,
        sender: oneshot::Sender<Task>,
    ) {
        let (inner, key, name) = (Arc::clone(self), key.to_owned(), name.clone());
        // Built before the spawn, so a task dropped before its first poll still takes its key out.
        let flight = Flight {
            inner: Arc::clone(&inner),
            hex: name.hex.clone(),
        };
        let task = tokio::spawn(async move {
            let _permit = permit;
            let _flight = flight;
            inner.fill(&key, &name).await
        });
        // The receiver is in the fill that is in the table, and in the caller: it is there.
        let _ = sender.send(task);
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
            Err(Stop::Changed) => self.skipped(FailedFill::Changed),
            Err(Stop::Lost) => {
                // Not a failure of the fill: the directory is another cache's now.
                bump(&self.counters.fills_skipped);
                Ok(Filled::Skipped)
            }
            Err(Stop::Mismatch) => {
                // The bytes are known to be wrong: they go to nobody.
                self.counters.failed(FailedFill::HashMismatch);
                let text = "the upstream's bytes do not hash to the name of the object";
                Err(Error::Unavailable(text.to_owned()))
            }
        }
    }

    /// Counts a fill that failed for `why`; its readers go to the upstream.
    fn skipped(&self, why: FailedFill) -> Result<Filled, Error> {
        self.counters.failed(why);
        Ok(Filled::Skipped)
    }

    /// Reads the object into the stage directory and puts it in the cache. The fill remembers
    /// the generation of the lock it started under and asks [`owns`](Inner::owns) before it
    /// makes the staging file, before it writes each chunk, inside the publish and before it
    /// puts the object in the index: a cache that lost the lock of its directory meanwhile
    /// writes nothing more ([`Stop::Lost`]). The lock is noticed lost by a tick at most once a
    /// second, so the step that is running when the lock file is replaced can still finish.
    async fn fetch(self: &Arc<Self>, key: &str, name: &Name) -> Result<Filled, Stop> {
        let generation = self.generation();
        let meta = self.upstream.head(key).await?;
        let Some(reservation) = self.reserve(meta.size) else {
            bump(&self.counters.fills_skipped);
            return Ok(Filled::Skipped);
        };
        if !self.owns(generation) {
            return Err(Stop::Lost);
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
            if !self.owns(generation) {
                return Err(Stop::Lost);
            }
            staged.file.write_all(&chunk).await?;
            hasher.update(&chunk);
            offset += want;
        }
        staged.finish().await?;
        // A key named by its content is checked by the hash below. Any other key could have been
        // written over while it was read, and the file would hold bytes of two versions.
        if name.content.is_none() && !self.upstream.head(key).await?.same_content(&meta) {
            return Err(Stop::Changed);
        }
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
        let fresh = match self.commit(&staged, name, &sidecar, generation).await? {
            Published::New => true,
            Published::Existed => false,
            Published::Lost => return Err(Stop::Lost),
        };
        #[cfg(test)]
        self.trace_after_publish();
        if !self.owns(generation) {
            if fresh {
                // The one delete a cache makes in a directory it does not own. It is safe: this
                // fill linked these names itself just now (`fresh`), and no other writer makes
                // them, the staging name being unique; an object left here would be one the new
                // owner does not index, so never evicts, and a later scan would index it, stale
                // if the key was deleted at the upstream in between. It covers the whole span
                // from the check before the rename, through the rename, the link and the return
                // of the publish. What is left is a whole object that is on the disk for the
                // moments until this removes it.
                let root = &self.config.path;
                let _ = std::fs::remove_file(data_path(root, &name.hex));
                let _ = std::fs::remove_file(meta_path(root, &name.hex));
            }
            return Err(Stop::Lost);
        }
        locked(&self.index).insert(&name.hex, meta.size, meta.tag, meta.modified);
        // A delete that came while the object was being read marked this fill: what it read is
        // gone from the upstream, so it is not kept (the mark is looked for after the entry is
        // in the index; see `forget`).
        if self.is_invalidated(&name.hex) {
            self.drop_entry(&name.hex);
            return Err(Stop::Changed);
        }
        drop(reservation);
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
        generation: u64,
    ) -> io::Result<Published> {
        let publishing = self.begin_commit(staged, name, sidecar, generation);
        publishing.await.expect("a publish does not panic")
    }

    /// Starts the link of the staged file into the cache on a thread that may block, and gives
    /// the handle of the job, which goes on when the handle is dropped. The job publishes only
    /// while the cache still owns the directory as of `generation` (see `Inner::owns`): fills
    /// and keeps both run it, and both give up when it says [`Published::Lost`].
    pub(super) fn begin_commit(
        self: &Arc<Self>,
        staged: &Staged,
        name: &Name,
        sidecar: &Sidecar,
        generation: u64,
    ) -> JoinHandle<io::Result<Published>> {
        let disk = Arc::clone(&self.disk);
        let root = self.config.path.clone();
        let hex = name.hex.clone();
        let (from, temporary) = (staged.path.clone(), staged.sidecar.clone());
        let text = serde_json::to_vec(sidecar).unwrap_or_default();
        let inner = Arc::clone(self);
        tokio::task::spawn_blocking(move || {
            let owns = || inner.owns(generation);
            publish(&*disk, &root, &hex, (&from, &temporary), &text, &owns)
        })
    }

    /// Runs a pass of the eviction if the cache is above its high water mark.
    pub(super) async fn relieve(self: &Arc<Self>) {
        let held = locked(&self.index).bytes();
        if held > percent_of(self.config.budget, self.config.high_water) {
            let inner = Arc::clone(self);
            let pass = tokio::task::spawn_blocking(move || inner.evict_pass());
            pass.await.expect("a pass does not panic");
        }
    }
}
