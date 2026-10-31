//! The lock of the cache directory, and the mode it decides.
//!
//! Two processes may have one cache directory open at once: a new release is started beside the
//! one that runs, and the old one exits when it has drained. Only one of them may write to the
//! directory. The one that holds the exclusive lock on `<path>/.lock` is read-write; the other is
//! read-only until the kernel frees the lock (the holder exited or crashed) and it takes it
//! itself. Nothing tells it to: [`LocalCache::tick`] tries the lock, and [`LocalCache::hold`]
//! calls it on an interval until it is halted.
//!
//! A read-only cache serves the hits its scan found from `obj/`, sends every miss and every write
//! to the upstream, and starts no fill, no keep, no eviction pass and no write of file times. It
//! creates, changes and removes nothing in the directory. An object the holder publishes after
//! the scan is not in the index of the read-only cache: it is a miss, answered from the upstream.
//!
//! A write-through delete or put sent to a read-only cache goes to the upstream, and the cache
//! forgets the object in its index, so a read of it goes to the upstream too. The holder's files
//! for it stay: the cache notes the name (at most `CacheConfig::forget_limit` names, 65536 by
//! default; past that it notes only that there were too many). When it takes the lock it removes
//! those files after the scan, before it is read-write, or, if there were too many, removes
//! every object file and starts with an empty index. What remains: the holder itself still
//! serves its cached copy of such an object, on the connections it is draining, until it exits.
//! That window ends with its drain, and a read-only cache cannot shorten it.
//!
//! Taking the lock runs what a start of a lone cache runs: it empties `.stage/` (the fills of
//! the one that held the lock are not kept), scans `obj/` again with deletes (a sidecar whose
//! data file was never linked is the middle of a publish that did not finish), and replaces the
//! index; only then does the cache say it is read-write. The holder checks, at every tick, that
//! `.lock` still leads to the file it holds: a file that was replaced is a lock that nothing
//! else looks at, so the cache lets it go, is read-only again, and takes the new file when it
//! can.

use std::collections::HashSet;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::Notify;

use crate::server::lock::{self, LockFile};

use super::{Inner, LocalCache, layout, locked};
use super::{add, index_of};

/// The name of the lock file in the cache directory.
pub(super) const LOCK: &str = ".lock";

/// Whether a cache may write to its directory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// It holds the lock, or takes none: it fills, keeps, evicts and stamps files.
    ReadWrite,
    /// Another process holds the lock: it serves hits from the directory and writes nothing to
    /// it.
    ReadOnly,
}

impl Mode {
    /// The word `/readyz` says: `read-write` or `read-only`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::ReadWrite => "read-write",
            Mode::ReadOnly => "read-only",
        }
    }
}

/// What one [`LocalCache::tick`] found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tick {
    /// The cache holds the lock and the lock file is the one at the path, or it takes no lock.
    Held,
    /// The cache found the lock free and took it: it scanned the directory again and is now
    /// read-write.
    Took,
    /// Another process holds the lock; the cache is read-only.
    Waiting,
    /// The lock file at the path is not the file the cache holds: it let the lock go and is
    /// read-only until it takes the lock again.
    Lost,
}

/// How a cache stands to the lock of its directory.
pub(super) enum Standing {
    /// The cache takes no lock and is read-write.
    Lockless,
    /// Another process holds the lock: the cache is read-only and tries it at every tick.
    Waiting,
    /// The cache holds the lock and is read-write.
    Holding(LockFile),
}

/// The objects a read-only cache was told, by a write-through delete or put, are gone or
/// replaced at the upstream, by the name of their files. The files are the holder's and stay
/// where they are, so the cache must not trust them when it takes the lock.
#[derive(Default)]
struct Forgotten {
    names: HashSet<String>,
    /// More names than the limit were noted: the cache does not know which, so it trusts no
    /// file.
    overflowed: bool,
}

impl Forgotten {
    /// Notes `hex`, or, if `limit` names are noted already, that there are too many to note.
    fn note(&mut self, hex: &str, limit: usize) {
        if self.overflowed {
            return;
        }
        if self.names.len() >= limit {
            self.overflowed = true;
            self.names.clear();
        } else {
            let _ = self.names.insert(hex.to_owned());
        }
    }
}

/// The lock a cache holds, if any, and the mode that follows from it.
pub(super) struct Hold {
    standing: Mutex<Standing>,
    /// Read on every request, so it is not behind the mutex that a scan holds for its length.
    read_write: AtomicBool,
    /// Taken before `read_write` is read by whoever acts on it, and held by [`Inner::promote`]
    /// until `read_write` is set: a delete is noted before the promotion looks at the notes, or
    /// finds the cache read-write and removes the files itself.
    forgotten: Mutex<Forgotten>,
    /// Calls a test makes at fixed points of the take and of a delete, to order them.
    #[cfg(test)]
    pub(super) trace: Mutex<Trace>,
}

/// A call a test asks to be made at a point.
#[cfg(test)]
pub(super) type Hook = Arc<dyn Fn() + Send + Sync>;

/// The points of [`Trace`].
#[cfg(test)]
#[derive(Default)]
pub(super) struct Trace {
    /// In `promote`, after the notes were taken into account for the scan and before the index
    /// is replaced.
    pub(super) promote: Option<Hook>,
    /// In `drop_entry`, just before it asks whether the files may be removed.
    pub(super) before_delete: Option<Hook>,
}

impl Hold {
    pub(super) fn new(standing: Standing) -> Self {
        let read_write = !matches!(standing, Standing::Waiting);
        Self {
            standing: Mutex::new(standing),
            read_write: AtomicBool::new(read_write),
            forgotten: Mutex::new(Forgotten::default()),
            #[cfg(test)]
            trace: Mutex::new(Trace::default()),
        }
    }
}

/// Removes the file at `path`; one that is not there is removed already.
fn remove_if_there(path: &Path) -> io::Result<()> {
    match std::fs::remove_file(path) {
        Err(why) if why.kind() != io::ErrorKind::NotFound => Err(why),
        _ => Ok(()),
    }
}

impl Inner {
    /// Whether the cache may write to its directory.
    pub(super) fn read_write(&self) -> bool {
        self.hold.read_write.load(Ordering::SeqCst)
    }

    /// The lock file of the directory.
    fn lock_path(&self) -> PathBuf {
        self.config.path.join(LOCK)
    }

    /// One try of the lock; see [`LocalCache::tick`].
    pub(super) fn tick(&self) -> io::Result<Tick> {
        let mut standing = locked(&self.hold.standing);
        if let Standing::Holding(file) = &*standing {
            if file.is_current() {
                return Ok(Tick::Held);
            }
            *standing = Standing::Waiting;
            self.hold.read_write.store(false, Ordering::SeqCst);
            return Ok(Tick::Lost);
        }
        if matches!(*standing, Standing::Lockless) {
            return Ok(Tick::Held);
        }
        let Some(file) = lock::try_take(&self.lock_path())? else {
            return Ok(Tick::Waiting);
        };
        // The lock is held from here, and dropped with `file` if the directory cannot be made
        // ready: the next tick tries again.
        self.promote()?;
        *standing = Standing::Holding(file);
        Ok(Tick::Took)
    }

    /// Whether the files of `hex` may be removed now: the cache is read-write. A read-only cache
    /// notes the name instead, for [`promote`](Self::promote), and removes nothing.
    ///
    /// The holder of the directory may go on serving its copy on the connections it is
    /// draining, until it exits; that window ends with its drain, and nothing here can end it.
    pub(super) fn may_delete(&self, hex: &str) -> bool {
        let mut forgotten = locked(&self.hold.forgotten);
        if self.read_write() {
            return true;
        }
        forgotten.note(hex, self.config.forget_limit);
        false
    }

    /// What taking the lock does to the directory and the index, and then makes the cache
    /// read-write. After the scan it removes the files of the objects a delete or put through
    /// this cache, while it was read-only, said are gone or replaced at the upstream, and drops
    /// them from the index; if more were noted than the limit holds, it removes every object
    /// and the index starts empty. Nothing is trusted that the holder left for a name that was
    /// deleted.
    fn promote(&self) -> io::Result<()> {
        let removed = layout::clear_stage(&self.config.path)?;
        let scanned = layout::scan(&self.config.path)?;
        let mut forgotten = locked(&self.hold.forgotten);
        let distrust = |hex: &String| forgotten.overflowed || forgotten.names.contains(hex);
        let (gone, kept): (Vec<_>, Vec<_>) =
            scanned.found.into_iter().partition(|f| distrust(&f.hex));
        for found in &gone {
            remove_if_there(&layout::data_path(&self.config.path, &found.hex))?;
            remove_if_there(&layout::meta_path(&self.config.path, &found.hex))?;
        }
        #[cfg(test)]
        self.trace_promote();
        add(
            &self.counters.discarded_at_start,
            scanned.discarded + removed,
        );
        *locked(&self.index) = index_of(kept);
        *forgotten = Forgotten::default();
        self.hold.read_write.store(true, Ordering::SeqCst);
        Ok(())
    }

    /// Sets the calls a test makes at the points of [`Trace`].
    #[cfg(test)]
    pub(super) fn set_trace(&self, trace: Trace) {
        *locked(&self.hold.trace) = trace;
    }

    /// Makes the call a test set for the take, if there is one.
    #[cfg(test)]
    fn trace_promote(&self) {
        let hook = locked(&self.hold.trace).promote.clone();
        if let Some(hook) = hook {
            hook();
        }
    }

    /// Makes the call a test set for a delete, if there is one.
    #[cfg(test)]
    pub(super) fn trace_before_delete(&self) {
        let hook = locked(&self.hold.trace).before_delete.clone();
        if let Some(hook) = hook {
            hook();
        }
    }

    /// The line the process says on stderr of a tick, if the tick changed anything.
    pub(super) fn describe(&self, outcome: &io::Result<Tick>) -> Option<String> {
        let path = self.config.path.display();
        match outcome {
            Ok(Tick::Took) => Some(format!("local cache at {path}: took the lock, read-write")),
            Ok(Tick::Lost) => Some(format!(
                "local cache at {path}: the lock file was replaced; read-only until the lock \
                 is taken again"
            )),
            Ok(Tick::Held | Tick::Waiting) => None,
            Err(why) => Some(format!("local cache at {path}: the lock: {why}")),
        }
    }
}

impl LocalCache {
    /// Whether the cache may write to its directory now.
    #[must_use]
    pub fn mode(&self) -> Mode {
        if self.inner.read_write() {
            Mode::ReadWrite
        } else {
            Mode::ReadOnly
        }
    }

    /// One try of the lock, which blocks the thread for as long as the scan it may run takes:
    /// call it from a thread that may block. A cache that holds the lock checks that the lock
    /// file is still the one at the path ([`Tick::Held`], or [`Tick::Lost`] if it is not); a
    /// read-only one tries to take the lock without waiting ([`Tick::Waiting`], or [`Tick::Took`]
    /// after it emptied `.stage/`, scanned `obj/` again and replaced its index). A cache opened
    /// with [`open`](Self::open) takes no lock and is always [`Tick::Held`].
    ///
    /// # Errors
    /// What the system said about the lock file, or about emptying `.stage/` and scanning
    /// `obj/` after the lock was taken; the lock is let go and the next tick tries again.
    pub fn tick(&self) -> io::Result<Tick> {
        self.inner.tick()
    }

    /// Calls [`tick`](Self::tick) every `interval` until `halt` is notified. A tick runs on a
    /// thread that may block, so nothing waits on the lock inside a request or on the runtime.
    /// What a tick changed, and what went wrong, is said on stderr.
    pub async fn hold(&self, interval: Duration, halt: Arc<Notify>) {
        loop {
            tokio::select! {
                biased;
                () = halt.notified() => break,
                () = tokio::time::sleep(interval) => {}
            }
            let inner = Arc::clone(&self.inner);
            let ticking = tokio::task::spawn_blocking(move || {
                let outcome = inner.tick();
                inner.describe(&outcome)
            });
            if let Some(line) = ticking.await.expect("a tick does not panic") {
                eprintln!("tierstore: {line}");
            }
        }
    }
}
