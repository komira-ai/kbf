//! The in-memory store's corners that the shared conformance run does not reach:
//! `may_hold`, overwriting without conditional writes (where only a retention lock
//! protects an object), and the trait's default `may_hold`.

use std::time::{Duration, SystemTime};

use bytes::Bytes;
use kbf_objstore::{
    ByteRange, Capabilities, KeyPrefix, ListPage, ListToken, MemoryStore, ObjectKey, ObjectStore,
    ObjectStoreError, PageSize,
};

fn key(name: &str) -> ObjectKey {
    ObjectKey::new(name).expect("a valid key")
}

fn whole(len: u64) -> ByteRange {
    ByteRange::new(0, len).expect("a valid range")
}

/// Catches: `may_hold` that answers `true` for any key (a cache hit would then be
/// served for an object the store lost), or `false` for a stored one.
#[tokio::test]
async fn may_hold_is_true_only_for_a_stored_key() {
    let store = MemoryStore::new(Capabilities::default());
    assert!(!store.may_hold(&key("a")).await.unwrap());
    store
        .put_new(&key("a"), Bytes::from("x"), None)
        .await
        .unwrap();
    assert!(store.may_hold(&key("a")).await.unwrap());
    assert!(!store.may_hold(&key("b")).await.unwrap());
    store.delete(&key("a")).await.unwrap();
    assert!(!store.may_hold(&key("a")).await.unwrap());
}

/// Catches: a store without conditional writes that refuses an overwrite of a plain
/// object (the contract says only conditional stores refuse), or that replaces a
/// retained object (a lock that a second put walks around).
#[tokio::test]
async fn without_conditional_writes_only_a_retained_object_is_protected() {
    let store = MemoryStore::new(Capabilities {
        conditional_put: false,
        object_lock: true,
    });
    store
        .put_new(&key("plain"), Bytes::from("one"), None)
        .await
        .unwrap();
    store
        .put_new(&key("plain"), Bytes::from("two"), None)
        .await
        .unwrap();
    let got = store.get_range(&key("plain"), whole(10)).await.unwrap();
    assert_eq!(got, Bytes::from("two"));

    let until = SystemTime::now() + Duration::from_secs(3600);
    store
        .put_new(&key("held"), Bytes::from("one"), Some(until))
        .await
        .unwrap();
    let err = store
        .put_new(&key("held"), Bytes::from("two"), None)
        .await
        .unwrap_err();
    assert!(
        matches!(&err, ObjectStoreError::Locked(k) if *k == key("held")),
        "{err}"
    );
    let got = store.get_range(&key("held"), whole(10)).await.unwrap();
    assert_eq!(got, Bytes::from("one"));
}

/// Catches: an expired retention still treated as a lock (the object could never be
/// replaced or deleted).
#[tokio::test]
async fn an_expired_retention_no_longer_protects_the_object() {
    let store = MemoryStore::new(Capabilities {
        conditional_put: false,
        object_lock: true,
    });
    let past = SystemTime::now() - Duration::from_secs(5);
    store
        .put_new(&key("old"), Bytes::from("one"), Some(past))
        .await
        .unwrap();
    store
        .put_new(&key("old"), Bytes::from("two"), None)
        .await
        .unwrap();
    store.delete(&key("old")).await.unwrap();
    assert!(!store.may_hold(&key("old")).await.unwrap());
}

/// A store that gives the trait's default `may_hold`: everything else is the fake's.
struct NoMayHold(MemoryStore);

impl ObjectStore for NoMayHold {
    fn capabilities(&self) -> Capabilities {
        self.0.capabilities()
    }
    async fn put_new(
        &self,
        key: &ObjectKey,
        body: Bytes,
        retain_until: Option<SystemTime>,
    ) -> Result<(), ObjectStoreError> {
        self.0.put_new(key, body, retain_until).await
    }
    async fn get_range(
        &self,
        key: &ObjectKey,
        range: ByteRange,
    ) -> Result<Bytes, ObjectStoreError> {
        self.0.get_range(key, range).await
    }
    async fn delete(&self, key: &ObjectKey) -> Result<(), ObjectStoreError> {
        self.0.delete(key).await
    }
    async fn list(
        &self,
        prefix: &KeyPrefix,
        after: Option<&ListToken>,
        max_keys: PageSize,
    ) -> Result<ListPage, ObjectStoreError> {
        self.0.list(prefix, after, max_keys).await
    }
}

/// Catches: a default `may_hold` that answers `false` (every cache hit refused on a
/// store that cannot tell without a request) or that asks the store and fails.
#[tokio::test]
async fn the_default_may_hold_says_yes_for_a_key_the_store_never_saw() {
    let store = NoMayHold(MemoryStore::new(Capabilities::default()));
    assert!(store.may_hold(&key("never-written")).await.unwrap());
}

/// Catches: a retention date accepted by a store that did not claim Object Lock (the
/// caller would believe the object protected), including when the key is new.
#[tokio::test]
async fn a_retention_date_without_object_lock_is_unsupported_and_writes_nothing() {
    let store = MemoryStore::new(Capabilities::default());
    let until = SystemTime::now() + Duration::from_secs(3600);
    let err = store
        .put_new(&key("a"), Bytes::from("x"), Some(until))
        .await
        .unwrap_err();
    assert!(
        matches!(err, ObjectStoreError::Unsupported("Object Lock retention")),
        "{err}"
    );
    assert!(!store.may_hold(&key("a")).await.unwrap());
}
