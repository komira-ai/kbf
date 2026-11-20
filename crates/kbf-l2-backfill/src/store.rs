//! The store as the engine needs it: a `LargeStore` over an object store.

use std::sync::Arc;

use bytes::Bytes;
use futures::future::BoxFuture;
use kbf_front::large_backfill::LargeStore;
use kbf_objstore::{BodyStream, ByteRange, ObjectKey, ObjectStore, ObjectStoreError};

/// A [`LargeStore`] over the object store `S`: the bucket the frontends write their packs and
/// tree lists to.
pub struct Bucket<S: ObjectStore> {
    store: Arc<S>,
}

impl<S: ObjectStore> Bucket<S> {
    /// The calls of the engine over `store`.
    #[must_use]
    pub fn new(store: Arc<S>) -> Self {
        Self { store }
    }
}

impl<S: ObjectStore> LargeStore for Bucket<S> {
    fn len<'a>(
        &'a self,
        key: &'a ObjectKey,
    ) -> BoxFuture<'a, Result<Option<u64>, ObjectStoreError>> {
        Box::pin(self.store.len(key))
    }

    fn put<'a>(
        &'a self,
        key: &'a ObjectKey,
        len: u64,
        body: BodyStream,
    ) -> BoxFuture<'a, Result<(), ObjectStoreError>> {
        Box::pin(self.store.put_stream(key, len, body))
    }

    fn read_range<'a>(
        &'a self,
        key: &'a ObjectKey,
        offset: u64,
        len: u64,
    ) -> BoxFuture<'a, Result<Bytes, ObjectStoreError>> {
        match ByteRange::new(offset, len) {
            Some(range) => Box::pin(self.store.get_range(key, range)),
            None => Box::pin(std::future::ready(Err(ObjectStoreError::Unsupported(
                "a read of no bytes",
            )))),
        }
    }
}
