//! What the lookups and the whole writes make of an object of another length than the blob under
//! a blob's `cas/l/` key (a write the store kept cut short): the lookups leave the blob to be
//! written, a write deletes the object and stores the blob, an object of the blob's length is
//! taken for the blob, and a key a create-only write finds taken by an object of another length
//! is [`NoTree::WrongLength`]. One bucket is the blobs' source and the store.

use std::collections::BTreeSet;

use futures::StreamExt as _;
use futures::future::BoxFuture;
use kbf_storage::StorageError;
use kbf_types::DigestFunction;
use sha2::Digest as _;

use super::*;

/// The blobs a source reads, and the objects of a create-only store.
#[derive(Default)]
struct Bucket {
    blobs: BTreeMap<Digest, Bytes>,
    objects: Mutex<BTreeMap<String, Bytes>>,
    /// Every key deleted, in order.
    deleted: Mutex<Vec<String>>,
    /// Every key a write was made of, in order.
    written: Mutex<Vec<String>>,
    /// Keys a write finds taken without storing; with `Some`, the object it finds there.
    taken: BTreeMap<String, Option<Bytes>>,
}

fn locked<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Bucket {
    /// A blob of `bytes` the source holds.
    fn blob(&mut self, bytes: &'static [u8]) -> Digest {
        let hash: [u8; 32] = Sha256::digest(bytes).into();
        let digest = Digest::new(DigestFunction::Sha256, hash, bytes.len() as u64);
        self.blobs.insert(digest, Bytes::from_static(bytes));
        digest
    }

    /// Stores `bytes` under the `cas/l/` key of `digest`.
    fn hold(&self, digest: &Digest, bytes: &'static [u8]) {
        let key = large_key(digest).to_string();
        locked(&self.objects).insert(key, Bytes::from_static(bytes));
    }

    fn object(&self, digest: &Digest) -> Option<Bytes> {
        locked(&self.objects)
            .get(large_key(digest).as_str())
            .cloned()
    }
}

impl BlobSource for Bucket {
    fn read_blob<'a>(&'a self, digest: &'a Digest) -> BoxFuture<'a, Result<Bytes, StorageError>> {
        let read = self.blobs.get(digest).cloned();
        Box::pin(async move { read.ok_or(StorageError::NotFound(*digest)) })
    }

    fn read_stream<'a>(
        &'a self,
        digest: &'a Digest,
    ) -> BoxFuture<'a, Result<ReadStream, StorageError>> {
        let read = self.blobs.get(digest).cloned();
        Box::pin(async move {
            let bytes = read.ok_or(StorageError::NotFound(*digest))?;
            Ok(stream::iter([Ok(bytes)]).boxed())
        })
    }

    fn large_len<'a>(
        &'a self,
        digest: &'a Digest,
    ) -> BoxFuture<'a, Result<Option<u64>, StorageError>> {
        let len = self.object(digest).map(|bytes| bytes.len() as u64);
        Box::pin(async move { Ok(len) })
    }
}

impl LargeStore for Bucket {
    fn len<'a>(
        &'a self,
        key: &'a ObjectKey,
    ) -> BoxFuture<'a, Result<Option<u64>, ObjectStoreError>> {
        let len = locked(&self.objects)
            .get(key.as_str())
            .map(|bytes| bytes.len() as u64);
        Box::pin(async move { Ok(len) })
    }

    fn put<'a>(
        &'a self,
        key: &'a ObjectKey,
        len: u64,
        mut body: BodyStream,
    ) -> BoxFuture<'a, Result<(), ObjectStoreError>> {
        Box::pin(async move {
            locked(&self.written).push(key.to_string());
            let mut got = Vec::new();
            while let Some(piece) = body.next().await {
                got.extend_from_slice(&piece?);
            }
            assert_eq!(got.len() as u64, len, "a body of the length declared");
            if let Some(found) = self.taken.get(key.as_str()) {
                if let Some(found) = found {
                    locked(&self.objects).insert(key.to_string(), found.clone());
                }
                return Err(ObjectStoreError::AlreadyExists(key.clone()));
            }
            let mut objects = locked(&self.objects);
            if objects.contains_key(key.as_str()) {
                return Err(ObjectStoreError::AlreadyExists(key.clone()));
            }
            objects.insert(key.to_string(), Bytes::from(got));
            Ok(())
        })
    }

    fn delete<'a>(&'a self, key: &'a ObjectKey) -> BoxFuture<'a, Result<(), ObjectStoreError>> {
        locked(&self.deleted).push(key.to_string());
        locked(&self.objects).remove(key.as_str());
        Box::pin(async { Ok(()) })
    }
}

async fn write(bucket: &Bucket, blobs: Vec<Digest>) -> Result<(), NoTree> {
    let stores = (bucket as &dyn BlobSource, bucket as &dyn LargeStore);
    write_whole(stores, &PutRetry::default(), blobs, None).await
}

/// Catches: an object of another length at a blob's key taken for the blob, or failing the
/// lookups: a blob held at its length is whole, one held at 0 bytes and one held nowhere are
/// whole and left to be written.
#[tokio::test]
async fn the_lookups_leave_a_blob_held_at_another_length_to_be_written() {
    let mut bucket = Bucket::default();
    let held = bucket.blob(b"held at its length");
    let short = bucket.blob(b"held at 0 bytes");
    let nowhere = bucket.blob(b"held nowhere");
    bucket.hold(&held, b"held at its length");
    bucket.hold(&short, b"");
    let (places, unlocated) = locate(&bucket, &[held, short, nowhere])
        .await
        .expect("located");
    assert!(places.values().all(|place| *place == Place::Whole));
    assert_eq!(places.len(), 3);
    let unlocated: BTreeSet<Digest> = unlocated.into_iter().collect();
    assert_eq!(unlocated, BTreeSet::from([short, nowhere]));
}

/// Catches: a write that sends a blob already held at its length again, or writes over an
/// object of another length without deleting it (a create-only write would find the key taken):
/// the held blob is neither written nor deleted, the object of 0 bytes is deleted and the blob
/// stored in its place, and the blob held nowhere is stored.
#[tokio::test]
async fn a_write_keeps_a_held_blob_and_deletes_an_object_of_another_length() {
    let mut bucket = Bucket::default();
    let held = bucket.blob(b"held at its length");
    let short = bucket.blob(b"held at 0 bytes");
    let nowhere = bucket.blob(b"held nowhere");
    bucket.hold(&held, b"held at its length");
    bucket.hold(&short, b"");
    write(&bucket, vec![held, short, nowhere])
        .await
        .expect("written");
    let short_key = large_key(&short).to_string();
    assert_eq!(*locked(&bucket.deleted), [short_key.clone()]);
    let written: BTreeSet<String> = locked(&bucket.written).iter().cloned().collect();
    assert_eq!(
        written,
        BTreeSet::from([short_key, large_key(&nowhere).to_string()])
    );
    assert_eq!(locked(&bucket.written).len(), 2);
    for digest in [held, short, nowhere] {
        assert_eq!(bucket.object(&digest), bucket.blobs.get(&digest).cloned());
    }
}

/// Catches: a key found taken trusted whatever it holds, or a key found taken by a write the
/// store does not show yet failed: an object of 0 bytes there is `WrongLength`, and no object
/// shown is success.
#[tokio::test]
async fn a_key_found_taken_is_judged_by_its_length() {
    let mut bucket = Bucket::default();
    let short = bucket.blob(b"taken by an object of 0 bytes");
    let unseen = bucket.blob(b"taken by a write not shown yet");
    bucket
        .taken
        .insert(large_key(&short).to_string(), Some(Bytes::new()));
    bucket.taken.insert(large_key(&unseen).to_string(), None);
    let refused = write(&bucket, vec![short]).await.expect_err("refused");
    assert_eq!(
        refused,
        NoTree::WrongLength {
            digest: short,
            length: 0
        }
    );
    write(&bucket, vec![unseen])
        .await
        .expect("taken is written");
    assert_eq!(bucket.object(&unseen), None);
    assert!(locked(&bucket.deleted).is_empty());
}
