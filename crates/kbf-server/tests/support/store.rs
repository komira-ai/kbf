//! The bucket a [`super::Cell`] and its restarts share.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::SystemTime;

use bytes::Bytes;
use super::Gate;
use kbf_objstore::{
    ByteRange, Capabilities, KeyPrefix, ListPage, ListToken, MemoryStore, ObjectKey, ObjectStore,
    ObjectStoreError, PageSize,
};

/// The cell's bucket: one [`MemoryStore`] that outlives each server process over it,
/// as a real bucket outlives `kbf-server` (see [`Cell::cold_restart`]), with a gate a
/// test can close on the next write, to stand for a slow object store.
#[derive(Clone)]
pub struct SharedStore {
    memory: Arc<MemoryStore>,
    /// Holds the next `put_new` until the test opens it.
    pub put: Arc<Gate>,
}

impl ObjectStore for SharedStore {
    fn capabilities(&self) -> Capabilities {
        self.memory.capabilities()
    }

    async fn put_new(
        &self,
        key: &ObjectKey,
        body: Bytes,
        retain_until: Option<SystemTime>,
    ) -> Result<(), ObjectStoreError> {
        self.put.pass().await;
        self.memory.put_new(key, body, retain_until).await
    }

    async fn get_range(
        &self,
        key: &ObjectKey,
        range: ByteRange,
    ) -> Result<Bytes, ObjectStoreError> {
        self.memory.get_range(key, range).await
    }

    async fn delete(&self, key: &ObjectKey) -> Result<(), ObjectStoreError> {
        self.memory.delete(key).await
    }

    async fn list(
        &self,
        prefix: &KeyPrefix,
        after: Option<&ListToken>,
        max_keys: PageSize,
    ) -> Result<ListPage, ObjectStoreError> {
        self.memory.list(prefix, after, max_keys).await
    }
}

impl SharedStore {
    /// An empty bucket whose gate is open.
    pub fn new() -> Self {
        Self {
            memory: Arc::new(MemoryStore::new(Capabilities::default())),
            put: Arc::default(),
        }
    }

    /// Every object in the bucket, key and bytes, in key order.
    pub async fn objects(&self) -> BTreeMap<String, Bytes> {
        let mut objects = BTreeMap::new();
        let mut after = None;
        loop {
            let page = self
                .list(&KeyPrefix::default(), after.as_ref(), PageSize::MAX)
                .await
                .expect("list the bucket");
            for info in page.objects {
                let range = ByteRange::new(0, info.size).expect("a range");
                let bytes = if info.size == 0 {
                    Bytes::new()
                } else {
                    self.get_range(&info.key, range)
                        .await
                        .expect("read an object")
                };
                objects.insert(info.key.as_str().to_owned(), bytes);
            }
            match page.next {
                Some(token) => after = Some(token),
                None => return objects,
            }
        }
    }
}
