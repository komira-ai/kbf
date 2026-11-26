//! The frontend's bounds on its writes to the pack store, and the wait for the store's
//! back-pressure: [`StoreWrites`], which [`Packs`](crate::Packs) puts in front of its store, so
//! every writer that goes through the packs shares it. Those writers are the packs of client
//! uploads (`BatchUpdateBlobs` and ByteStream `Write`), the large objects of uploads past the
//! packs' bound, the blobs, manifests, roots and lists of action trees, and the objects of base
//! publication and backfill.
//!
//! - **The bounds.** At most [`PackSettings::store_writes`](crate::PackSettings::store_writes)
//!   writes (`--pack-store-writes`, [`STORE_WRITES`] by default) are at the store at once, but
//!   for the writes of an action tree's manifests (`.kbf` among them), its root and its list,
//!   which take one of [`TREE_WRITES`] slots of their own instead. A write past its bound waits
//!   for a slot before it makes its request, so a burst of uploads queues at the frontend, where
//!   it costs the callers time, instead of at the store, which answers `SlowDown` to all of them.
//!   A write holds its slot until it ends, its waits for `SlowDown` included. Reads, lengths,
//!   deletes and listings take no slot.
//! - **A tree's writes do not queue behind the uploads.** A tree's manifests, root and list are
//!   small, and a run waits for them before it is submitted; with every one of the
//!   `--pack-store-writes` slots held by uploads, they still reach the store. A write is one of
//!   them when it is made under the tally of a tree's write ([`crate::action_tree::tallied`]),
//!   which only those writes are; the wait for its slot is told to that write
//!   ([`crate::action_tree::WriteWaits::store_slot`]). The blobs a tree writes whole, and its
//!   list's L2 sidecar, are made under no tally and share `--pack-store-writes` with the uploads.
//!   The frontend then has at most the two bounds added up at the store; the store's own bound
//!   on its puts, answered `SlowDown` past it, still holds them all.
//! - **Streamed writes hold a slot for their whole body.** A streamed write takes its slot before
//!   it reads its body and keeps it until the store has the last byte. For a client's blob over
//!   the packs' bound, that is as long as the client takes to send it: a client that stops
//!   sending is ended by `--pack-upload-idle-timeout`, which gives the slot back, but one that
//!   keeps sending slowly keeps it. As many such uploads as there are slots leave none for the
//!   other writers of those slots until one ends; a tree's manifests, root and list are not held
//!   back by them.
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
use tokio::sync::{AcquireError, Semaphore, SemaphorePermit};
use tokio::time::Instant;

use crate::action_tree::{in_tree_write, waited_for_slot};
use crate::store_wait::{Backoff, Waiting};

/// The default of `--pack-store-writes`: the most writes a frontend has at its pack store at once.
pub const STORE_WRITES: NonZeroUsize = NonZeroUsize::MIN.saturating_add(63);

/// The slots of the writes of action trees' manifests, roots and lists at the pack store, apart
/// from `--pack-store-writes`: as many as the tree writer's manifest slots by default
/// ([`MANIFEST_WRITES`](crate::action_tree::MANIFEST_WRITES)), so that a manifest that has one of
/// those waits for no other while no tree list is being written.
pub(crate) const TREE_WRITES: NonZeroUsize = NonZeroUsize::MIN.saturating_add(31);

/// An object store whose writes share a fixed number of slots, a tree's manifests, roots and
/// lists another, and whose calls wait out its 503s.
pub(crate) struct StoreWrites<S> {
    inner: Waiting<S>,
    slots: Semaphore,
    tree_slots: Semaphore,
}

impl<S: ObjectStore> StoreWrites<S> {
    /// `inner`, with at most `slots` writes at it at once and `tree_slots` writes of a tree's
    /// manifests, root and list besides, each call waiting out its 503s as `backoff` says.
    pub(crate) fn new(
        inner: Arc<S>,
        slots: NonZeroUsize,
        tree_slots: NonZeroUsize,
        backoff: Backoff,
    ) -> Self {
        // A semaphore holds at most `Semaphore::MAX_PERMITS`; `--pack-store-writes` is far below.
        let semaphore = |n: NonZeroUsize| Semaphore::new(n.get().min(Semaphore::MAX_PERMITS));
        Self {
            inner: Waiting::new(inner, backoff),
            slots: semaphore(slots),
            tree_slots: semaphore(tree_slots),
        }
    }
}

impl<S> StoreWrites<S> {
    /// One slot: a tree slot for the write of a tree's manifest, root or list
    /// ([`crate::action_tree::in_tree_write`]), the wait for it told to that write
    /// ([`crate::action_tree::waited_for_slot`]); one of `--pack-store-writes` for any other.
    async fn slot(&self) -> Result<SemaphorePermit<'_>, AcquireError> {
        let slots = match in_tree_write() {
            true => &self.tree_slots,
            false => &self.slots,
        };
        let asked = Instant::now();
        let slot = slots.acquire().await;
        waited_for_slot(asked.elapsed());
        slot
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
        let _slot = self.slot().await;
        self.inner.put_new(key, body, retain_until).await
    }

    async fn put_stream(
        &self,
        key: &ObjectKey,
        len: u64,
        body: BodyStream,
    ) -> Result<(), ObjectStoreError> {
        // Taken before the body is read, so a write that waits for a slot reads nothing of it.
        let _slot = self.slot().await;
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
