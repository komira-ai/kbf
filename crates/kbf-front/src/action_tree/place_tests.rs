//! What the lookups and the whole writes make of each place a blob may be: a pack span of its
//! length, or of another length; a `cas/l/` object of its length, or of another length (a write
//! the store kept cut short); nowhere. The lookups leave a blob held at another length to be
//! written; a write deletes such an object and stores the blob, records the cells of what it
//! wrote, takes an object of the blob's length for the blob, refuses bytes that are not the
//! blob's, and judges a key a create-only write finds taken by the length of what is there. One
//! bucket is the blobs' source and the store; it reads a blob in chunks of [`CHUNK`] bytes.

use std::collections::BTreeSet;

use futures::StreamExt as _;
use futures::future::BoxFuture;
use kbf_pack_index::PackIndex;
use kbf_pack_index::memory::MemoryPackIndex;
use kbf_state::ManualClock;
use kbf_storage::StorageError;
use kbf_types::DigestFunction;
use sha2::Digest as _;

use super::*;
use crate::job_read::Span;

/// The bytes of one chunk of a blob's read: a blob is several chunks.
const CHUNK: usize = 4;

/// The blobs a source reads, the pack index's spans, and the objects of a create-only store.
#[derive(Default)]
struct Bucket {
    blobs: BTreeMap<Digest, Bytes>,
    /// Bytes read for a blob instead of its own.
    wrong: BTreeMap<Digest, Bytes>,
    spans: BTreeMap<Digest, Span>,
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

    /// Records `digest` in the pack index at a span of `length` bytes.
    fn span(&mut self, digest: Digest, length: u64) {
        let span = Span {
            key: "cas/f/w/p0".to_owned(),
            offset: 8,
            length,
        };
        self.spans.insert(digest, span);
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
        let read = (self.wrong.get(digest))
            .or_else(|| self.blobs.get(digest))
            .cloned();
        Box::pin(async move {
            let bytes = read.ok_or(StorageError::NotFound(*digest))?;
            let chunks: Vec<Result<Bytes, StorageError>> = (0..bytes.len())
                .step_by(CHUNK)
                .map(|at| Ok(bytes.slice(at..(at + CHUNK).min(bytes.len()))))
                .collect();
            Ok(stream::iter(chunks).boxed())
        })
    }

    fn locate_packed<'a>(
        &'a self,
        digests: &'a [Digest],
    ) -> BoxFuture<'a, Result<Vec<Option<Span>>, StorageError>> {
        let spans = digests.iter().map(|d| self.spans.get(d).cloned()).collect();
        Box::pin(async move { Ok(spans) })
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

/// Catches: a span of another length taken for a blob's place, or an object of another length
/// at a blob's key taken for the blob or failing the lookups: a blob the index spans at its
/// length is packed there; one spanned at another length and held nowhere, one held at 0 bytes
/// and one held nowhere are left to be written; one held at its length is whole.
#[tokio::test]
async fn the_lookups_leave_a_blob_held_at_another_length_to_be_written() {
    let mut bucket = Bucket::default();
    let packed = bucket.blob(b"spanned at its length");
    let odd = bucket.blob(b"spanned at another length");
    let held = bucket.blob(b"held at its length");
    let short = bucket.blob(b"held at 0 bytes");
    let nowhere = bucket.blob(b"held nowhere");
    bucket.span(packed, packed.size_bytes);
    bucket.span(odd, odd.size_bytes - 1);
    bucket.hold(&held, b"held at its length");
    bucket.hold(&short, b"");
    let (places, unlocated) = locate(&bucket, &[packed, odd, held, short, nowhere])
        .await
        .expect("located");
    let at = Place::Packed {
        key: "cas/f/w/p0".to_owned(),
        offset: 8,
    };
    assert_eq!(places.get(&packed), Some(&at));
    for digest in [odd, held, short, nowhere] {
        assert_eq!(places.get(&digest), Some(&Place::Whole));
    }
    let unlocated: BTreeSet<Digest> = unlocated.into_iter().collect();
    assert_eq!(unlocated, BTreeSet::from([odd, short, nowhere]));
}

/// Catches: a write that sends a blob already held at its length again, writes over an object
/// of another length without deleting it (a create-only write would find the key taken), or
/// records no cells of what it wrote: the held blob is neither written nor deleted, the object
/// of 0 bytes is deleted and the blob stored in its place, the blob held nowhere is stored, each
/// read in several chunks, and the cells of both written are recorded in the index.
#[tokio::test]
async fn a_write_keeps_a_held_blob_and_deletes_an_object_of_another_length() {
    let mut bucket = Bucket::default();
    let held = bucket.blob(b"held at its length");
    let short = bucket.blob(b"held at 0 bytes");
    let nowhere = bucket.blob(b"held nowhere");
    bucket.hold(&held, b"held at its length");
    bucket.hold(&short, b"");
    let bucket = Arc::new(bucket);
    let index = Arc::new(MemoryPackIndex::new(Arc::new(ManualClock::default())));
    let sidecars = Sidecars::new(index.clone(), bucket.clone());
    let stores = (&*bucket as &dyn BlobSource, &*bucket as &dyn LargeStore);
    let blobs = vec![held, short, nowhere];
    write_whole(stores, &PutRetry::default(), blobs, Some(&sidecars))
        .await
        .expect("written");
    let short_key = large_key(&short).to_string();
    let nowhere_key = large_key(&nowhere).to_string();
    let written: BTreeSet<String> = locked(&bucket.written).iter().cloned().collect();
    assert_eq!(
        written,
        BTreeSet::from([short_key.clone(), nowhere_key.clone()])
    );
    assert_eq!(locked(&bucket.written).len(), 2);
    assert_eq!(*locked(&bucket.deleted), std::slice::from_ref(&short_key));
    for digest in [held, short, nowhere] {
        assert_eq!(bucket.object(&digest), bucket.blobs.get(&digest).cloned());
    }
    let keys = [short_key, nowhere_key, large_key(&held).to_string()];
    let cells = index.object_cells(&keys).expect("cells");
    let sizes: Vec<Option<u64>> = cells
        .iter()
        .map(|row| row.as_ref().map(|c| c.size))
        .collect();
    assert_eq!(
        sizes,
        [Some(short.size_bytes), Some(nowhere.size_bytes), None]
    );
}

/// Catches: bytes that are not the blob's stored under its key: the write is refused as a
/// mismatch, and nothing is stored.
#[tokio::test]
async fn bytes_that_are_not_the_blob_s_are_not_stored() {
    let mut bucket = Bucket::default();
    let blob = bucket.blob(b"the blob's own bytes");
    bucket
        .wrong
        .insert(blob, Bytes::from_static(b"other bytes, as long"));
    let refused = write(&bucket, vec![blob]).await.expect_err("refused");
    assert_eq!(refused, NoTree::Mismatch { digest: blob });
    assert_eq!(bucket.object(&blob), None);
}

/// Catches: a key found taken trusted whatever it holds, or a key found taken by the blob or by
/// a write the store does not show yet failed: an object of 0 bytes there is `WrongLength`, and
/// the blob's own bytes or no object shown are success.
#[tokio::test]
async fn a_key_found_taken_is_judged_by_its_length() {
    let mut bucket = Bucket::default();
    let short = bucket.blob(b"taken by an object of 0 bytes");
    let unseen = bucket.blob(b"taken by a write not shown yet");
    let same = bucket.blob(b"taken by the blob itself");
    bucket
        .taken
        .insert(large_key(&short).to_string(), Some(Bytes::new()));
    bucket.taken.insert(large_key(&unseen).to_string(), None);
    let own = Bytes::from_static(b"taken by the blob itself");
    bucket.taken.insert(large_key(&same).to_string(), Some(own));
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
    write(&bucket, vec![same]).await.expect("taken is written");
    assert!(locked(&bucket.deleted).is_empty());
}
