//! The multipart uploads in progress. The parts of an upload are held in memory until the
//! upload is completed, when they are written to the proxy as one object, or aborted.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use bytes::Bytes;
use tokio::time::Instant;

/// One part that was uploaded.
#[derive(Clone)]
pub(crate) struct Part {
    pub(crate) body: Bytes,
    /// The hex SHA-256 of the body, which the part's ETag quotes.
    pub(crate) tag: String,
}

struct Upload {
    key: String,
    parts: BTreeMap<u32, Part>,
    /// When the upload was made, or a part was last added to it.
    touched: Instant,
}

/// Why a part was not taken.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Rejected {
    /// There is no such upload for this key.
    NoSuchUpload,
    /// The parts held would pass the limit, but the parts of this upload alone would not: it
    /// can be sent again when other uploads are done.
    Full,
    /// The parts of this upload alone would pass the limit: no wait can make room for them.
    TooLarge,
}

/// The uploads in progress, by upload id.
pub(crate) struct Uploads {
    open: Mutex<BTreeMap<String, Upload>>,
    /// Starts every id, so that an id of an earlier run of the server is not one of this run.
    epoch: u64,
    next: AtomicU64,
}

fn locked<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    // No method panics while holding the lock, so a poisoned map is still whole.
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Uploads {
    pub(crate) fn new(epoch: u64) -> Self {
        Self {
            open: Mutex::new(BTreeMap::new()),
            epoch,
            next: AtomicU64::new(1),
        }
    }

    /// Starts an upload of `key` and gives its id. An upload that nothing was added to for
    /// `ttl` is dropped first.
    pub(crate) fn create(&self, key: &str, ttl: Duration) -> String {
        let number = self.next.fetch_add(1, Ordering::SeqCst);
        let id = format!("{:x}-{number:x}", self.epoch);
        let now = Instant::now();
        let mut open = locked(&self.open);
        open.retain(|_, upload| now.duration_since(upload.touched) < ttl);
        let upload = Upload {
            key: key.to_owned(),
            parts: BTreeMap::new(),
            touched: now,
        };
        open.insert(id.clone(), upload);
        id
    }

    /// Adds a part to the upload `id` of `key`, in place of the part of that number if there
    /// is one, unless the parts of this upload would then hold more than `limit` bytes
    /// ([`Rejected::TooLarge`]) or the parts of all the uploads would ([`Rejected::Full`]).
    /// An upload that nothing was added to for `ttl` is dropped first.
    pub(crate) fn add_part(
        &self,
        id: &str,
        key: &str,
        number: u32,
        part: Part,
        limit: u64,
        ttl: Duration,
    ) -> Result<(), Rejected> {
        let now = Instant::now();
        let mut open = locked(&self.open);
        open.retain(|_, upload| now.duration_since(upload.touched) < ttl);
        let held: usize = open
            .values()
            .flat_map(|upload| upload.parts.values())
            .map(|stored| stored.body.len())
            .sum();
        let upload = open.get_mut(id).filter(|upload| upload.key == key);
        let upload = upload.ok_or(Rejected::NoSuchUpload)?;
        let replaced = upload.parts.get(&number).map_or(0, |old| old.body.len());
        let kept = &upload.parts;
        let own: usize = kept.values().map(|stored| stored.body.len()).sum();
        let alone = (own - replaced + part.body.len()) as u64;
        if alone > limit {
            return Err(Rejected::TooLarge);
        }
        let after = (held - replaced + part.body.len()) as u64;
        if after > limit {
            return Err(Rejected::Full);
        }
        upload.parts.insert(number, part);
        upload.touched = now;
        Ok(())
    }

    /// The parts of the upload `id` of `key`, if there is such an upload.
    pub(crate) fn parts(&self, id: &str, key: &str) -> Option<BTreeMap<u32, Part>> {
        let open = locked(&self.open);
        open.get(id)
            .filter(|upload| upload.key == key)
            .map(|upload| upload.parts.clone())
    }

    /// Forgets the upload `id`, which was completed.
    pub(crate) fn finish(&self, id: &str) {
        let _ = locked(&self.open).remove(id);
    }

    /// Forgets the upload `id` of `key`, if there is such an upload. Says whether there was.
    pub(crate) fn abort(&self, id: &str, key: &str) -> bool {
        let mut open = locked(&self.open);
        let known = open.get(id).is_some_and(|upload| upload.key == key);
        if known {
            let _ = open.remove(id);
        }
        known
    }

    /// The bytes held in the parts of all the uploads.
    #[cfg(test)]
    pub(crate) fn held(&self) -> usize {
        let open = locked(&self.open);
        open.values()
            .flat_map(|upload| upload.parts.values())
            .map(|part| part.body.len())
            .sum()
    }
}
