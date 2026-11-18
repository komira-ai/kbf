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
//! - **The store's 503s.** Under the slots, every call waits out a store that answers 503
//!   ([`Waiting`], as [`crate::store_wait`] says): a whole write waits holding its slot, its
//!   `SlowDown` for as long as the store answers it and any other 503 for at most
//!   [`STORE_WAIT`](crate::store_wait::STORE_WAIT); a read, a length, a delete or a listing waits
//!   holding none. A streamed write ([`ObjectStore::put_stream`]) reads a body that cannot be
//!   read twice, so it is made once, and the store's answer is its caller's.

use std::num::NonZeroUsize;
use std::sync::Arc;
use std::time::SystemTime;

use bytes::Bytes;
use kbf_objstore::{
    BodyStream, ByteRange, Capabilities, KeyPrefix, ListPage, ListToken, ObjectKey, ObjectStore,
    ObjectStoreError, PageSize,
};
use tokio::sync::Semaphore;

use crate::store_wait::{Backoff, Waiting};

/// The default of `--pack-store-writes`: the most writes a frontend has at its pack store at once.
pub const STORE_WRITES: NonZeroUsize = NonZeroUsize::MIN.saturating_add(63);

/// An object store whose writes share a fixed number of slots and whose calls wait out its 503s.
pub(crate) struct StoreWrites<S> {
    inner: Waiting<S>,
    slots: Semaphore,
}

impl<S: ObjectStore> StoreWrites<S> {
    /// `inner`, with at most `slots` writes at it at once, each call waiting out its 503s as
    /// `backoff` says.
    pub(crate) fn new(inner: Arc<S>, slots: NonZeroUsize, backoff: Backoff) -> Self {
        // A semaphore holds at most `Semaphore::MAX_PERMITS`; `--pack-store-writes` is far below.
        let slots = Semaphore::new(slots.get().min(Semaphore::MAX_PERMITS));
        Self {
            inner: Waiting::new(inner, backoff),
            slots,
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
        self.inner.put_new(key, body, retain_until).await
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

#[cfg(test)]
#[path = "store_writes_tests.rs"]
mod tests;
