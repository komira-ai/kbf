//! The frontend's one bound on its writes to the pack store, and the wait for the store's
//! back-pressure: [`StoreWrites`], which [`Packs`](crate::Packs) puts in front of its store, so
//! every writer that goes through the packs shares it. Those writers are the packs of client
//! uploads (`BatchUpdateBlobs` and ByteStream `Write`), the large objects of uploads past the
//! packs' bound, the blobs and manifests of action trees, and the objects of base publication
//! and backfill.
//!
//! - **The bound.** At most [`PackSettings::store_writes`](crate::PackSettings::store_writes)
//!   writes (`--pack-store-writes`, [`STORE_WRITES`] by default) are at the store at once. A
//!   write past it waits for a slot before it makes its request, so a burst of uploads queues at
//!   the frontend, where it costs the callers time, instead of at the store, which answers
//!   `SlowDown` to all of them. A write holds its slot until it ends, its waits for `SlowDown`
//!   included. Reads, lengths, deletes and listings take no slot.
//! - **Streamed writes hold a slot for their whole body.** A streamed write takes its slot before
//!   it reads its body and keeps it until the store has the last byte. For a client's blob over
//!   the packs' bound, that is as long as the client takes to send it: a client that stops
//!   sending is ended by `--pack-upload-idle-timeout`, which gives the slot back, but one that
//!   keeps sending slowly keeps it. As many such uploads as there are slots leave none for the
//!   other writers until one ends. Streamed writes are not given a share of their own, because
//!   the action trees' blobs and manifests are streamed writes too, and would share it.
//! - **`SlowDown`.** tierstore answers a 503 `SlowDown` when it is taking as many uploads as it
//!   can: back-pressure, not a fault. A write whose body is in hand ([`ObjectStore::put_new`]) is
//!   made again after a wait that starts at [`FIRST_WAIT`] and doubles up to [`LONGEST_WAIT`], for
//!   as long as the store answers it. What ends it is the caller: a client upload whose
//!   deadline passes drops the write, and with it the wait and the slot; a writer of the
//!   frontend's own (an action tree's preparation, base publication) ends it by its own limit,
//!   or waits for the store. A streamed write
//!   ([`ObjectStore::put_stream`]) reads a body that cannot be read twice, so its `SlowDown` is
//!   its caller's answer, as every other answer of the store is.

use std::num::NonZeroUsize;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use bytes::Bytes;
use kbf_objstore::{
    BodyStream, ByteRange, Capabilities, KeyPrefix, ListPage, ListToken, ObjectKey, ObjectStore,
    ObjectStoreError, PageSize,
};
use tokio::sync::Semaphore;

use crate::action_tree::{FIRST_WAIT, LONGEST_WAIT, Sleeper, is_slow_down, tokio_sleeper};

/// The default of `--pack-store-writes`: the most writes a frontend has at its pack store at once.
pub const STORE_WRITES: NonZeroUsize = NonZeroUsize::MIN.saturating_add(63);

/// An object store whose writes share a fixed number of slots and wait out `SlowDown`.
pub(crate) struct StoreWrites<S> {
    inner: Arc<S>,
    slots: Semaphore,
    sleeper: Arc<dyn Sleeper>,
}

impl<S: ObjectStore> StoreWrites<S> {
    /// `inner`, with at most `slots` writes at it at once, waiting on the tokio timer.
    pub(crate) fn new(inner: Arc<S>, slots: NonZeroUsize) -> Self {
        Self::with_sleeper(inner, slots, tokio_sleeper())
    }

    /// [`StoreWrites::new`] waiting with `sleeper` (a test records the waits instead).
    pub(crate) fn with_sleeper(
        inner: Arc<S>,
        slots: NonZeroUsize,
        sleeper: Arc<dyn Sleeper>,
    ) -> Self {
        // A semaphore holds at most `Semaphore::MAX_PERMITS`; `--pack-store-writes` is far below.
        let slots = Semaphore::new(slots.get().min(Semaphore::MAX_PERMITS));
        Self {
            inner,
            slots,
            sleeper,
        }
    }
}

impl<S: ObjectStore> ObjectStore for StoreWrites<S> {
    fn capabilities(&self) -> Capabilities {
        self.inner.capabilities()
    }

    async fn put_new(
        &self,
        key: &ObjectKey,
        body: Bytes,
        retain_until: Option<SystemTime>,
    ) -> Result<(), ObjectStoreError> {
        // Held until the write ends, its waits included. The semaphore is never closed, so this
        // is always a permit.
        let _slot = self.slots.acquire().await;
        let mut wait = FIRST_WAIT;
        loop {
            match self.inner.put_new(key, body.clone(), retain_until).await {
                Err(error) if is_slow_down(&error) => {
                    tracing::debug!(%key, ?wait, "the store answered SlowDown; waiting");
                    self.sleeper.sleep(wait).await;
                    wait = next_wait(wait);
                }
                answer => return answer,
            }
        }
    }

    async fn put_stream(
        &self,
        key: &ObjectKey,
        len: u64,
        body: BodyStream,
    ) -> Result<(), ObjectStoreError> {
        // Taken before the body is read, so a write that waits for a slot reads nothing of it.
        let _slot = self.slots.acquire().await;
        self.inner.put_stream(key, len, body).await
    }

    async fn get_range(
        &self,
        key: &ObjectKey,
        range: ByteRange,
    ) -> Result<Bytes, ObjectStoreError> {
        self.inner.get_range(key, range).await
    }

    async fn may_hold(&self, key: &ObjectKey) -> Result<bool, ObjectStoreError> {
        self.inner.may_hold(key).await
    }

    async fn len(&self, key: &ObjectKey) -> Result<Option<u64>, ObjectStoreError> {
        self.inner.len(key).await
    }

    async fn delete(&self, key: &ObjectKey) -> Result<(), ObjectStoreError> {
        self.inner.delete(key).await
    }

    async fn list(
        &self,
        prefix: &KeyPrefix,
        after: Option<&ListToken>,
        max_keys: PageSize,
    ) -> Result<ListPage, ObjectStoreError> {
        self.inner.list(prefix, after, max_keys).await
    }
}

/// The wait after `wait`: twice as long, at most [`LONGEST_WAIT`].
fn next_wait(wait: Duration) -> Duration {
    wait.saturating_mul(2).min(LONGEST_WAIT)
}

#[cfg(test)]
#[path = "store_writes_tests.rs"]
mod tests;
