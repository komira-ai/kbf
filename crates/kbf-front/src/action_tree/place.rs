//! Where each blob a tree names is, and the writes that make every blob locatable.
//!
//! - **Packed**: the pack index has a span of the blob's length; the manifest names it as
//!   `source: {key, offset}`. The index is asked in chunks of [`LOCATE_CHUNK`] digests, at most
//!   [`ASKS_IN_FLIGHT`] chunks at once.
//! - **Whole**: tierstore holds `cas/l/<sha256>` of the blob's length (a HEAD each, at most
//!   [`HEADS_IN_FLIGHT`] at once); the manifest names no source.
//! - **Unlocated**: neither. The blob is read from the frontend's CAS and written to
//!   `cas/l/<sha256>` with a create-only PUT ([`write_whole`]); then it is whole. An object of
//!   another length under the key is not the blob: it is a write the store kept cut short (the
//!   SeaweedFS behind tierstore keeps a PUT whose body failed before its first byte as an object
//!   of 0 bytes), and the blob is unlocated too. Each attempt of a write asks the key's length
//!   first: an object of the blob's length is the blob, and nothing is sent; one of another
//!   length is deleted, and the create-only PUT that follows stores the blob. A PUT that finds
//!   the key taken asks its length again: an object of another length there ends the tree
//!   ([`NoTree::WrongLength`]), and the next tree's write deletes it.
//!
//! The lookups (the index asks and the HEADs) answer within [`LOOKUP_DEADLINE`] together, or the
//! tree is not made. A HEAD waits out a store that answers 503 for up to
//! [`STORE_WAIT`](crate::store_wait::STORE_WAIT) ([`crate::store_wait`]), so the deadline is
//! that wait and 30 s more: a store that fails over while a tree is looked up delays the tree,
//! and does not end it. A write streams the blob through a SHA-256 check and holds its last chunk
//! back until the whole has hashed to the digest, so a store never receives all of a blob whose
//! bytes are wrong: the body ends in an error instead. A store keeps nothing of it, or, like the
//! SeaweedFS above, an object shorter than the blob, which the next write deletes.

use std::collections::BTreeMap;
use std::convert::Infallible;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use bytes::Bytes;
use futures::stream::{self, StreamExt as _, TryStreamExt as _};
use kbf_objstore::{BodyStream, ObjectKey, ObjectStoreError};
use kbf_pack_index::cells::ObjectCells;
use kbf_storage::ReadStream;
use kbf_types::Digest;
use sha2::{Digest as _, Sha256};
use tierstore_list::CellHasher;

use super::NoTree;
use super::retry::{Failed, PutRetry, repeating, retrying};
use super::sidecar::Sidecars;
use crate::job_read::BlobSource;
use crate::large_backfill::{LargeStore, large_key};
use crate::store_wait::STORE_WAIT;

/// The most digests one ask of the pack index names.
pub const LOCATE_CHUNK: usize = 4096;

/// The most asks of the pack index open at once.
pub(super) const ASKS_IN_FLIGHT: usize = 4;

/// The most presence checks (HEADs) of `cas/l/` open at once.
pub const HEADS_IN_FLIGHT: usize = 16;

/// The most writes to `cas/l/` open at once, blobs or manifests.
pub(super) const WRITES_IN_FLIGHT: usize = 16;

/// How long the lookups of one tree may take together: the index asks and the presence checks.
/// It is longer than the most a presence check waits for a store that answers 503
/// ([`STORE_WAIT`]), by 30 s.
pub const LOOKUP_DEADLINE: Duration = STORE_WAIT.saturating_add(Duration::from_secs(30));

/// Where a blob's bytes are, as a manifest names them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Place {
    /// The span of the pack `key` at `offset`.
    Packed { key: String, offset: u64 },
    /// The object `cas/l/<sha256>`, which the blob is, whole.
    Whole,
}

/// The places of `blobs` (none of them empty), and those of them that are nowhere yet, which
/// are placed [`Place::Whole`] and must be written before a manifest names them.
///
/// # Errors
/// [`NoTree`]: the index or a presence check failed or did not answer within
/// [`LOOKUP_DEADLINE`].
pub(super) async fn locate(
    source: &dyn BlobSource,
    blobs: &[Digest],
) -> Result<(BTreeMap<Digest, Place>, Vec<Digest>), NoTree> {
    tokio::time::timeout(LOOKUP_DEADLINE, lookups(source, blobs))
        .await
        .map_err(|_| NoTree::Deadline)?
}

async fn lookups(
    source: &dyn BlobSource,
    blobs: &[Digest],
) -> Result<(BTreeMap<Digest, Place>, Vec<Digest>), NoTree> {
    let lookup = |error: kbf_storage::StorageError| NoTree::Lookup {
        why: error.to_string(),
    };
    // Collected, so the future holds the asks and not the closure that made them: a future
    // that holds a closure over a borrowed chunk cannot be proved `Send` for every lifetime.
    let asks: Vec<_> = blobs
        .chunks(LOCATE_CHUNK)
        .map(|chunk| async move { Ok((chunk, source.locate_packed(chunk).await?)) })
        .collect();
    let answers: Vec<_> = stream::iter(asks)
        .buffered(ASKS_IN_FLIGHT)
        .try_collect()
        .await
        .map_err(lookup)?;
    let mut places = BTreeMap::new();
    let mut rest = Vec::new();
    for (chunk, spans) in answers {
        // A blob the index gave no answer for is in no pack.
        let spans = spans.into_iter().chain(std::iter::repeat(None));
        for (digest, span) in chunk.iter().zip(spans) {
            match span {
                Some(span) if span.length == digest.size_bytes => {
                    let (key, offset) = (span.key, span.offset);
                    places.insert(*digest, Place::Packed { key, offset });
                }
                _ => rest.push(*digest),
            }
        }
    }
    let heads = rest
        .into_iter()
        .map(|digest| async move { Ok((digest, source.large_len(&digest).await?)) });
    let lengths: Vec<(Digest, Option<u64>)> = stream::iter(heads)
        .buffered(HEADS_IN_FLIGHT)
        .try_collect()
        .await
        .map_err(lookup)?;
    let mut unlocated = Vec::new();
    for (digest, length) in lengths {
        match length {
            Some(length) if length == digest.size_bytes => {}
            Some(length) => {
                tracing::warn!(blob = %digest, length, "cas/l/ holds an object of another length than the blob: it is written again");
                unlocated.push(digest);
            }
            None => unlocated.push(digest),
        }
        places.insert(digest, Place::Whole);
    }
    Ok((places, unlocated))
}

/// Writes each of `blobs` to `cas/l/<sha256>`, read from `source` and checked on the way, at
/// most [`WRITES_IN_FLIGHT`] at once. An object of the blob's length under the key is success:
/// the key is content-bound. One of another length is deleted and the blob written over it.
///
/// # Errors
/// [`NoTree`]: a blob could not be read, its bytes are not its digest's, the store refused
/// the write, the length check or the delete, or a write found the key taken by an object of
/// another length ([`NoTree::WrongLength`]).
pub(super) async fn write_whole(
    (source, store): (&dyn BlobSource, &dyn LargeStore),
    retry: &PutRetry,
    blobs: Vec<Digest>,
    sidecars: Option<&Sidecars>,
) -> Result<(), NoTree> {
    stream::iter(blobs)
        .map(|digest| write_blob(source, store, retry, digest, sidecars))
        .buffered(WRITES_IN_FLIGHT)
        .try_collect()
        .await
}

/// Writes the blob `digest` to `cas/l/<sha256>`; with `sidecars`, the cells of its bytes,
/// hashed as they pass to the store, are recorded once the write succeeds (an existing key holds
/// the same bytes: the key is content-bound).
async fn write_blob(
    source: &dyn BlobSource,
    store: &dyn LargeStore,
    retry: &PutRetry,
    digest: Digest,
    sidecars: Option<&Sidecars>,
) -> Result<(), NoTree> {
    let key = large_key(&digest);
    let hashed = Arc::new(Mutex::new(None));
    written_blob(source, store, retry, digest, &hashed).await?;
    let cells = hashed.lock().unwrap_or_else(PoisonError::into_inner).take();
    if let (Some(sidecars), Some(cells)) = (sidecars, cells) {
        sidecars.record(&key, cells).await;
    }
    Ok(())
}

async fn written_blob(
    source: &dyn BlobSource,
    store: &dyn LargeStore,
    retry: &PutRetry,
    digest: Digest,
    hashed: &Arc<Mutex<Option<ObjectCells>>>,
) -> Result<(), NoTree> {
    let key = large_key(&digest);
    // An attempt reads the blob again: a body is gone once a write has taken it. It asks what
    // the key holds first, so that an attempt after one whose body stalled (which a store may
    // have kept as an object of 0 bytes) deletes what that one left.
    retrying(retry, &key, || async {
        if made_way(store, &key, digest.size_bytes)
            .await
            .map_err(Failed::<NoTree>::Store)?
        {
            return Ok(());
        }
        let read = source.read_stream(&digest).await.map_err(|error| {
            Failed::Other(NoTree::Read {
                digest,
                why: error.to_string(),
            })
        })?;
        let failure = Arc::new(Mutex::new(None));
        let body = Checked {
            inner: read,
            hasher: Sha256::new(),
            cells: CellHasher::new(),
            held: None,
            digest,
            ended: false,
            failure: Arc::clone(&failure),
            hashed: Arc::clone(hashed),
        }
        .into_body();
        match store.put(&key, digest.size_bytes, body).await {
            Ok(()) => Ok(()),
            Err(ObjectStoreError::AlreadyExists(_)) => taken(store, &key, digest).await,
            Err(error) => {
                let mut failure = failure.lock().unwrap_or_else(PoisonError::into_inner);
                Err(failure.take().map_or(Failed::Store(error), Failed::Other))
            }
        }
    })
    .await
}

/// Whether `key` holds an object of `len` bytes already: the blob, since the key is
/// content-bound. An object of another length is deleted, so that a create-only write can store
/// the blob: it is not the blob, but a write the store kept cut short.
///
/// # Errors
/// The store's answer to the length check or the delete.
async fn made_way(
    store: &dyn LargeStore,
    key: &ObjectKey,
    len: u64,
) -> Result<bool, ObjectStoreError> {
    match store.len(key).await? {
        Some(found) if found == len => Ok(true),
        Some(found) => {
            tracing::warn!(%key, found, len, "deleting an object of another length than the blob its key names, to write the blob");
            store.delete(key).await?;
            Ok(false)
        }
        None => Ok(false),
    }
}

/// A create-only write of `digest` found `key` taken. An object of the blob's length is the
/// blob, and one the store does not show yet is a write still being made (a store shows an
/// object only once it is whole): both are success. One of another length is not the blob.
///
/// # Errors
/// [`NoTree::WrongLength`]: the key holds an object of another length; the store's answer to
/// the length check.
async fn taken(store: &dyn LargeStore, key: &ObjectKey, digest: Digest) -> Result<(), Failed> {
    match store.len(key).await.map_err(Failed::<NoTree>::Store)? {
        Some(length) if length != digest.size_bytes => {
            Err(Failed::Other(NoTree::WrongLength { digest, length }))
        }
        _ => Ok(()),
    }
}

/// Writes `bytes` to `key`, create-only: an existing key is success, since every key this
/// writer writes is named by its content.
///
/// # Errors
/// [`NoTree::Put`]: the store refused it.
pub(super) async fn write_object(
    store: &dyn LargeStore,
    retry: &PutRetry,
    key: &ObjectKey,
    bytes: Bytes,
) -> Result<(), NoTree> {
    store_object(store, retry, key, bytes)
        .await
        .map_err(|error| NoTree::Put {
            key: key.to_string(),
            why: error.to_string(),
        })
}

/// [`write_object`], the failure the store's last answer.
///
/// # Errors
/// The store's last answer.
pub(super) async fn store_object(
    store: &dyn LargeStore,
    retry: &PutRetry,
    key: &ObjectKey,
    bytes: Bytes,
) -> Result<(), ObjectStoreError> {
    let length = bytes.len() as u64;
    let stored = repeating::<Infallible, _, _>(retry, || async {
        let body = stream::once(std::future::ready(Ok(bytes.clone()))).boxed();
        put(store, key, length, body).await.map_err(Failed::Store)
    })
    .await;
    stored.map_err(|failed| {
        let Failed::Store(error) = failed;
        error
    })
}

/// One create-only write; an existing key is success.
async fn put(
    store: &dyn LargeStore,
    key: &ObjectKey,
    length: u64,
    body: BodyStream,
) -> Result<(), ObjectStoreError> {
    match store.put(key, length, body).await {
        Err(ObjectStoreError::AlreadyExists(_)) => Ok(()),
        other => other,
    }
}

/// A blob's bytes as a put's body, hashed as they pass, the last chunk held back until the whole
/// has hashed to the digest.
struct Checked {
    inner: ReadStream,
    hasher: Sha256,
    held: Option<Bytes>,
    /// The cells of the bytes, hashed in the same pass (the L2 sidecar's).
    cells: CellHasher,
    digest: Digest,
    ended: bool,
    /// Why the body ended in an error, for the write to report.
    failure: Arc<Mutex<Option<NoTree>>>,
    /// The cells of the bytes once they have hashed to the digest.
    hashed: Arc<Mutex<Option<ObjectCells>>>,
}

/// What the body says when it ends in an error: the store must keep nothing.
const REFUSED: &str =
    "the blob was not written: its bytes were not its digest's or its read failed";

impl Checked {
    fn into_body(self) -> BodyStream {
        stream::unfold(self, Self::next).boxed()
    }

    async fn next(mut self) -> Option<(Result<Bytes, ObjectStoreError>, Self)> {
        if self.ended {
            return None;
        }
        loop {
            let failure = match self.inner.next().await {
                Some(Ok(chunk)) => {
                    self.hasher.update(&chunk);
                    self.cells.update(&chunk);
                    if let Some(ready) = self.held.replace(chunk) {
                        return Some((Ok(ready), self));
                    }
                    continue;
                }
                Some(Err(error)) => NoTree::Read {
                    digest: self.digest,
                    why: error.to_string(),
                },
                None => {
                    self.ended = true;
                    let hash: [u8; 32] = self.hasher.finalize_reset().into();
                    if hash == self.digest.hash {
                        let cells = std::mem::take(&mut self.cells).finish();
                        let row = ObjectCells {
                            size: cells.size,
                            digests: cells.digests,
                        };
                        *self.hashed.lock().unwrap_or_else(PoisonError::into_inner) = Some(row);
                        return self.held.take().map(|last| (Ok(last), self));
                    }
                    NoTree::Mismatch {
                        digest: self.digest,
                    }
                }
            };
            self.ended = true;
            *self.failure.lock().unwrap_or_else(PoisonError::into_inner) = Some(failure);
            return Some((Err(ObjectStoreError::Body(REFUSED.to_owned())), self));
        }
    }
}

#[cfg(test)]
#[path = "place_tests.rs"]
mod tests;
