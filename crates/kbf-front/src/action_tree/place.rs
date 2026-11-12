//! Where each blob a tree names is, and the writes that make every blob locatable.
//!
//! - **Packed**: the pack index has a span of the blob's length; the manifest names it as
//!   `source: {key, offset}`. The index is asked in chunks of [`LOCATE_CHUNK`] digests, at most
//!   [`ASKS_IN_FLIGHT`] chunks at once.
//! - **Whole**: tierstore holds `cas/l/<sha256>` of the blob's length (a HEAD each, at most
//!   [`HEADS_IN_FLIGHT`] at once); the manifest names no source. An object of another length
//!   there is a store fault: it is not the blob, and a create-only write cannot replace it.
//! - **Unlocated**: neither. The blob is read from the frontend's CAS and written to
//!   `cas/l/<sha256>` with a create-only PUT ([`write_whole`]); then it is whole.
//!
//! The lookups (the index asks and the HEADs) answer within [`LOOKUP_DEADLINE`] together, or the
//! tree is not made. A write streams the blob through a SHA-256 check and holds its last chunk
//! back until the whole has hashed to the digest, so a store never receives all of a blob whose
//! bytes are wrong: the body ends in an error instead, and the store keeps nothing.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use bytes::Bytes;
use futures::stream::{self, StreamExt as _, TryStreamExt as _};
use kbf_objstore::{BodyStream, ObjectKey, ObjectStoreError};
use kbf_storage::ReadStream;
use kbf_types::Digest;
use sha2::{Digest as _, Sha256};

use super::NoTree;
use crate::job_read::BlobSource;
use crate::large_backfill::{LargeStore, large_key};

/// The most digests one ask of the pack index names.
pub const LOCATE_CHUNK: usize = 4096;

/// The most asks of the pack index open at once.
pub(super) const ASKS_IN_FLIGHT: usize = 4;

/// The most presence checks (HEADs) of `cas/l/` open at once.
pub const HEADS_IN_FLIGHT: usize = 16;

/// The most writes to `cas/l/` open at once, blobs or manifests.
pub(super) const WRITES_IN_FLIGHT: usize = 16;

/// How long the lookups of one tree may take together: the index asks and the presence checks.
pub const LOOKUP_DEADLINE: Duration = Duration::from_secs(30);

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
/// [`LOOKUP_DEADLINE`], or `cas/l/` holds an object of another length for a blob.
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
            Some(length) if length != digest.size_bytes => {
                return Err(NoTree::WrongLength { digest, length });
            }
            Some(_) => {}
            None => unlocated.push(digest),
        }
        places.insert(digest, Place::Whole);
    }
    Ok((places, unlocated))
}

/// Writes each of `blobs` to `cas/l/<sha256>`, read from `source` and checked on the way, at
/// most [`WRITES_IN_FLIGHT`] at once. An existing key is success: the key is content-bound.
///
/// # Errors
/// [`NoTree`]: a blob could not be read, its bytes are not its digest's, or the store refused
/// the write.
pub(super) async fn write_whole(
    source: &dyn BlobSource,
    store: &dyn LargeStore,
    blobs: Vec<Digest>,
) -> Result<(), NoTree> {
    stream::iter(blobs)
        .map(|digest| write_blob(source, store, digest))
        .buffered(WRITES_IN_FLIGHT)
        .try_collect()
        .await
}

async fn write_blob(
    source: &dyn BlobSource,
    store: &dyn LargeStore,
    digest: Digest,
) -> Result<(), NoTree> {
    let read = source
        .read_stream(&digest)
        .await
        .map_err(|error| NoTree::Read {
            digest,
            why: error.to_string(),
        })?;
    let failure = Arc::new(Mutex::new(None));
    let body = Checked {
        inner: read,
        hasher: Sha256::new(),
        held: None,
        digest,
        ended: false,
        failure: Arc::clone(&failure),
    }
    .into_body();
    let key = large_key(&digest);
    put(store, &key, digest.size_bytes, body)
        .await
        .map_err(|error| {
            let mut failure = failure.lock().unwrap_or_else(PoisonError::into_inner);
            failure.take().unwrap_or(error)
        })
}

/// Writes `bytes` to `cas/l/<sha256 of bytes>` (`manifest`), create-only.
///
/// # Errors
/// [`NoTree::Put`]: the store refused it.
pub(super) async fn write_manifest(
    store: &dyn LargeStore,
    manifest: &Digest,
    bytes: Bytes,
) -> Result<(), NoTree> {
    let body = stream::once(async move { Ok(bytes) }).boxed();
    put(store, &large_key(manifest), manifest.size_bytes, body).await
}

/// One create-only write; an existing key is success.
async fn put(
    store: &dyn LargeStore,
    key: &ObjectKey,
    length: u64,
    body: BodyStream,
) -> Result<(), NoTree> {
    match store.put(key, length, body).await {
        Ok(()) | Err(ObjectStoreError::AlreadyExists(_)) => Ok(()),
        Err(error) => Err(NoTree::Put {
            key: key.to_string(),
            why: error.to_string(),
        }),
    }
}

/// A blob's bytes as a put's body, hashed as they pass, the last chunk held back until the whole
/// has hashed to the digest.
struct Checked {
    inner: ReadStream,
    hasher: Sha256,
    held: Option<Bytes>,
    digest: Digest,
    ended: bool,
    /// Why the body ended in an error, for the write to report.
    failure: Arc<Mutex<Option<NoTree>>>,
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
