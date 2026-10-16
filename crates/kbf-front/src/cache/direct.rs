//! A blob written to the store as its bytes arrive ([`Cache::store_streamed`]): how a
//! ByteStream `Write` of a blob over [`MAX_UNSTAGED_BLOB_BYTES`](crate::MAX_UNSTAGED_BLOB_BYTES)
//! is stored, whatever its size, holding a piece of it at a time.

use std::sync::{Arc, Mutex, PoisonError};

use bytes::Bytes;
use futures::StreamExt;
use kbf_meta::{Location, StoreId};
use kbf_objstore::{BodyStream, ObjectStore, ObjectStoreError};
use kbf_segments::layout::{encode_footer, footer_len};
use kbf_segments::{IndexEntry, RecordHasher};
use kbf_types::Digest;

use super::write_group::{Group, read_back};
use super::{Cache, CacheError};
use crate::meta_log::MetaLog;

impl<M: MetaLog, O: ObjectStore> Cache<M, O> {
    /// Stores the blob `digest`, whose bytes come in `bytes`, as an object of its own:
    /// each piece hashed (on the hashing pool) and handed to the store as it comes
    /// ([`ObjectStore::put_stream`]: a multipart upload on S3), then the footer of its
    /// one record, only once the bytes hashed to `digest`. The footer is read back and
    /// the blob indexed with one `PutBlobs`: it is present only once this answers.
    ///
    /// Not staged on the write tier, and not under a `--write-puts-in-flight` slot: the
    /// write lasts as long as its sender takes. The caller's admission bounds how many
    /// run at once.
    ///
    /// # Errors
    /// [`CacheError::DigestMismatch`] if the bytes do not hash to `digest` (or are not
    /// as many): nothing is stored. The body's own error, or the store's or the log's.
    pub(crate) async fn store_streamed(
        &self,
        digest: Digest,
        bytes: BodyStream,
    ) -> Result<(), CacheError> {
        let object = self.writer.allocate();
        let key = self.object_key(object)?;
        let total = digest.size_bytes + footer_len(1);
        let mismatch = Arc::new(Mutex::new(None));
        let hashing = Hashing {
            group: Arc::clone(&self.group),
            bytes,
            hasher: Some(RecordHasher::new()),
            digest,
            mismatch: Arc::clone(&mismatch),
        };
        let body = futures::stream::unfold(hashing, Hashing::step).boxed();
        let put = self.writer.objects.put_stream(&key, total, body).await;
        let mismatch = mismatch
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some(actual) = mismatch {
            return Err(CacheError::DigestMismatch {
                claimed: digest,
                actual,
            });
        }
        put?;
        self.writer.recency.written(object, total);
        let footer = self
            .writer
            .read_footer(&key, total)
            .await
            .map_err(read_back)?;
        let entry = footer.find(&digest).filter(|e| e.offset == 0);
        if entry.is_none() {
            return Err(CacheError::Internal(format!(
                "object {key} came back without blob {digest}"
            )));
        }
        let at = Location {
            store: StoreId::CONFIGURED,
            object,
            offset: 0,
        };
        self.writer.commit_segment(vec![(digest, at)]).await
    }
}

/// A blob's bytes on their way to the store, hashed as they pass.
struct Hashing {
    group: Arc<Group>,
    bytes: BodyStream,
    /// `None` once the bytes ended.
    hasher: Option<RecordHasher>,
    digest: Digest,
    /// What the bytes hashed to, when not `digest`.
    mismatch: Arc<Mutex<Option<Digest>>>,
}

impl Hashing {
    /// The next piece of the object: a piece of the blob, then its footer (or, if the
    /// bytes were not the blob's, an error in its place).
    async fn step(mut self) -> Option<(Result<Bytes, ObjectStoreError>, Self)> {
        let mut hasher = self.hasher.take()?;
        let Some(piece) = self.bytes.next().await else {
            let (actual, crc32c) = hasher.finish();
            if actual != self.digest {
                *self.mismatch.lock().unwrap_or_else(PoisonError::into_inner) = Some(actual);
                let why = format!("{} hashes to {actual}", self.digest);
                return Some((Err(ObjectStoreError::Body(why)), self));
            }
            let entry = IndexEntry {
                digest: self.digest,
                offset: 0,
                crc32c,
            };
            let footer = encode_footer(self.digest.size_bytes, &[entry]);
            return Some((Ok(Bytes::from(footer)), self));
        };
        let piece = match piece {
            Ok(piece) => piece,
            Err(e) => return Some((Err(e), self)),
        };
        let hashed = piece.clone();
        let job = self.group.hashing.compute(move || {
            hasher.update(&hashed);
            hasher
        });
        match job.await {
            Ok(hasher) => self.hasher = Some(hasher),
            Err(e) => return Some((Err(ObjectStoreError::Body(e.to_string())), self)),
        }
        Some((Ok(piece), self))
    }
}
