//! An in-memory [`Backend`]: the executable spec of the contract, for tests.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use async_trait::async_trait;
use bytes::Bytes;
use sha2::{Digest, Sha256};

use crate::{Backend, ByteRange, Clock, Error, Listed, Meta};

/// A clock a test sets by hand, for the times a [`MemoryBackend`] gives its objects. It starts
/// at zero and moves only when told to, so two tiers that share one clock agree on the time.
#[derive(Default)]
pub struct TestClock {
    now: AtomicU64,
}

impl TestClock {
    /// A clock at zero.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The time, in milliseconds.
    #[must_use]
    pub fn now(&self) -> u64 {
        self.now.load(Ordering::SeqCst)
    }

    /// Sets the time, in milliseconds.
    pub fn set(&self, now: u64) {
        self.now.store(now, Ordering::SeqCst);
    }

    /// Moves the time on by `millis` milliseconds.
    pub fn advance(&self, millis: u64) {
        self.now.fetch_add(millis, Ordering::SeqCst);
    }
}

impl Clock for TestClock {
    fn now(&self) -> u64 {
        TestClock::now(self)
    }
}

/// An object held in memory, with the time it was written.
struct Stored {
    body: Bytes,
    modified: u64,
}

/// The content tag of a body: the hex of its SHA-256.
fn tag_of(body: &Bytes) -> String {
    hex::encode(Sha256::digest(body))
}

/// A bucket held in memory. Its content tag is the hex of the SHA-256 of the body, and an
/// object's time of last write is what its [`TestClock`] said when it was put.
#[derive(Default)]
pub struct MemoryBackend {
    objects: Mutex<BTreeMap<String, Stored>>,
    clock: Arc<TestClock>,
}

impl MemoryBackend {
    /// An empty bucket with a clock of its own, which stays at zero unless it is read with
    /// [`MemoryBackend::clock`] and moved.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// An empty bucket that stamps its objects with the time of `clock`.
    #[must_use]
    pub fn with_clock(clock: Arc<TestClock>) -> Self {
        Self {
            objects: Mutex::new(BTreeMap::new()),
            clock,
        }
    }

    /// The clock that stamps the objects.
    #[must_use]
    pub fn clock(&self) -> Arc<TestClock> {
        Arc::clone(&self.clock)
    }

    fn objects(&self) -> MutexGuard<'_, BTreeMap<String, Stored>> {
        // No method panics while holding the lock, so a poisoned map is still whole.
        self.objects.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn object(&self, key: &str) -> Result<(Bytes, u64), Error> {
        let found = self
            .objects()
            .get(key)
            .map(|stored| (stored.body.clone(), stored.modified));
        found.ok_or_else(|| Error::NotFound(key.to_owned()))
    }
}

#[async_trait]
impl Backend for MemoryBackend {
    async fn put(&self, key: &str, body: Bytes) -> Result<(), Error> {
        let modified = self.clock.now();
        let mut objects = self.objects();
        if objects.contains_key(key) {
            return Err(Error::AlreadyExists(key.to_owned()));
        }
        objects.insert(key.to_owned(), Stored { body, modified });
        Ok(())
    }

    async fn put_if_match(&self, key: &str, body: Bytes, etag: &str) -> Result<String, Error> {
        let modified = self.clock.now();
        let mut objects = self.objects();
        let current = objects
            .get(key)
            .ok_or_else(|| Error::NotFound(key.to_owned()))?;
        if tag_of(&current.body) != etag {
            return Err(Error::PreconditionFailed(key.to_owned()));
        }
        let tag = tag_of(&body);
        objects.insert(key.to_owned(), Stored { body, modified });
        Ok(tag)
    }

    async fn get(&self, key: &str, range: Option<ByteRange>) -> Result<Bytes, Error> {
        let (body, _) = self.object(key)?;
        match range {
            Some(range) => range.slice(&body),
            None => Ok(body),
        }
    }

    async fn head(&self, key: &str) -> Result<Meta, Error> {
        let (body, modified) = self.object(key)?;
        Ok(Meta {
            size: body.len() as u64,
            tag: tag_of(&body),
            modified,
        })
    }

    async fn delete(&self, key: &str) -> Result<(), Error> {
        self.objects().remove(key);
        Ok(())
    }

    async fn list(&self, prefix: &str) -> Result<Vec<Listed>, Error> {
        let objects = self.objects();
        let listed: Vec<Listed> = objects
            .iter()
            .filter(|(key, _)| key.starts_with(prefix))
            .map(|(key, stored)| Listed {
                key: key.clone(),
                size: stored.body.len() as u64,
                modified: stored.modified,
            })
            .collect();
        Ok(listed)
    }
}
