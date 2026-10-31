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
//! Taking the lock runs what a start of a lone cache runs: it empties `.stage/` (the fills of
//! the one that held the lock are not kept), scans `obj/` again with deletes (a sidecar whose
//! data file was never linked is the middle of a publish that did not finish), and replaces the
//! index; only then does the cache say it is read-write. The holder checks, at every tick, that
//! `.lock` still leads to the file it holds: a file that was replaced is a lock that nothing
//! else looks at, so the cache lets it go, is read-only again, and takes the new file when it
//! can.

use std::io;
use std::path::PathBuf;
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

/// The lock a cache holds, if any, and the mode that follows from it.
pub(super) struct Hold {
    standing: Mutex<Standing>,
    /// Read on every request, so it is not behind the mutex that a scan holds for its length.
    read_write: AtomicBool,
}

impl Hold {
    pub(super) fn new(standing: Standing) -> Self {
        let read_write = !matches!(standing, Standing::Waiting);
        Self {
            standing: Mutex::new(standing),
            read_write: AtomicBool::new(read_write),
        }
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
        self.hold.read_write.store(true, Ordering::SeqCst);
        Ok(Tick::Took)
    }

    /// What taking the lock does to the directory and the index, before the cache is read-write.
    fn promote(&self) -> io::Result<()> {
        let removed = layout::clear_stage(&self.config.path)?;
        let scanned = layout::scan(&self.config.path)?;
        add(
            &self.counters.discarded_at_start,
            scanned.discarded + removed,
        );
        *locked(&self.index) = index_of(scanned.found);
        Ok(())
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
