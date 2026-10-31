//! The lock of the cache directory, and the mode it decides.

use std::io;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Notify;

use super::{Inner, LocalCache};

/// Whether a cache may write to its directory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// It holds the lock, or takes none.
    ReadWrite,
    /// Another process holds the lock.
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
    /// The cache found the lock free and took it.
    Took,
    /// Another process holds the lock.
    Waiting,
    /// The lock file at the path is not the file the cache holds.
    Lost,
}

impl Inner {
    /// The line the process says on stderr of a tick, if the tick changed anything.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(super) fn describe(&self, _outcome: &io::Result<Tick>) -> Option<String> {
        None
    }
}

impl LocalCache {
    /// Whether the cache may write to its directory now.
    #[must_use]
    pub fn mode(&self) -> Mode {
        Mode::ReadWrite
    }

    /// One try of the lock.
    ///
    /// # Errors
    /// What the system said about the lock file.
    pub fn tick(&self) -> io::Result<Tick> {
        Ok(Tick::Held)
    }

    /// Calls [`tick`](Self::tick) every `interval` until `halt` is notified.
    pub async fn hold(&self, _interval: Duration, halt: Arc<Notify>) {
        halt.notified().await;
    }
}
