//! Large blobs in the packs' object store, for a frontend that has no storage node.
//!
//! A blob over [`Packs::max_blob_bytes`] is never packed. Where there are storage nodes it goes
//! to them; a frontend without nodes (`crate::Backend::stored`) writes it itself as the object
//! `cas/l/<sha256 lowercase hex>`, the key tierstore's job reads and the large-object backfill
//! use, and records it in the pack index as a pack of one blob at offset 0 whose length is the
//! object's. The index is then the one place that says what is held, whatever the size, and a
//! read is the ranged read of the object that a read of any pack is.
//!
//! **Write** ([`Packs::put_large`]). The index is asked first: a blob it has is held and nothing
//! is read from the client. Otherwise one HEAD of the key: an object of the right length is
//! present (the key is content-bound and create-only, so whoever wrote it wrote these bytes) and
//! only the row is recorded; an object of another length is refused and never replaced; no
//! object is a streamed put of exactly the blob's size ([`LargeStore::put`]). The bytes are
//! hashed as they pass and the put is given the blob one piece behind, so the store has not
//! received its last byte until the digest has checked: bytes that are not the digest end the
//! body in an error and the store leaves nothing under the key. The row is recorded only after
//! the object is stored, so a row never points at an object that is not there; a crash between
//! the two leaves an object no row names, which the next write of the blob records.
//!
//! **Read** ([`Packs::read_large`]). The range is read from the object in windows of
//! [`READ_WINDOW_BYTES`] and sent as it is read; when the range is the whole blob it is hashed on
//! the way and its last window is sent only once the digest checked. Bytes that are not the
//! digest end the stream UNAVAILABLE in place of the last window and the location is dropped
//! from the index. A range of part of a blob is not checked: the object is content-bound and
//! only ever written whole after a check.

use std::sync::{Arc, Mutex, PoisonError};

use bytes::Bytes;
use futures::StreamExt as _;
use futures::stream::{self, BoxStream};
use kbf_objstore::{BodyStream, ByteRange, ObjectKey, ObjectStoreError};
use kbf_pack_index::{Location, NewPack, PackBlob};
use kbf_types::Digest;
use sha2::{Digest as _, Sha256};
use tonic::Status;

use super::Packs;
use super::store::{from_index, index_failed, offload, to_index};
use crate::READ_WINDOW_BYTES;

/// The pieces of a large blob as a client sends them, or the client's own error.
pub type Pieces = BoxStream<'static, Result<Bytes, Status>>;

/// The windows of a range of a large blob, or the failure that ended it.
pub type LargeRead = BoxStream<'static, Result<Bytes, Status>>;

/// The key a large blob is stored under.
#[must_use]
pub fn large_key(digest: &Digest) -> String {
    let mut key = String::from("cas/l/");
    for byte in digest.hash {
        key.push_str(&format!("{byte:02x}"));
    }
    key
}

impl Packs {
    /// Where the blob `digest` lies, of any size: the row of the index, `None` when it has none.
    /// ([`Packs::locate`] answers `None` for a blob over [`Packs::max_blob_bytes`], which it
    /// never reads.)
    ///
    /// # Errors
    /// UNAVAILABLE when the index fails.
    pub async fn locate_stored(&self, digest: &Digest) -> Result<Option<Location>, Status> {
        let wanted = vec![to_index(digest)];
        let index = Arc::clone(&self.index);
        let located = offload(move || index.locate(&wanted)).await;
        let mut found = located.map_err(index_failed)?;
        Ok(found.pop().flatten())
    }

    /// The bytes `[offset, end)` of the stored blob `digest`, as a stream of windows; `None`
    /// when the index has no row for it. `end` is at most the blob's size.
    ///
    /// # Errors
    /// UNAVAILABLE when the index failed; later failures are items of the stream (the object
    /// could not be read or is shorter than the row says, or the whole blob is not its digest).
    pub async fn read_large(
        &self,
        digest: &Digest,
        offset: u64,
        end: u64,
    ) -> Result<Option<LargeRead>, Status> {
        let Some(location) = self.locate_stored(digest).await? else {
            return Ok(None);
        };
        let key = ObjectKey::new(&location.pack_key)
            .map_err(|e| Status::unavailable(format!("the index names the key {e}")))?;
        let reading = Reading {
            packs: self.clone(),
            digest: *digest,
            key,
            base: location.offset,
            pos: offset,
            end,
            hasher: (offset == 0 && end == digest.size_bytes).then(Sha256::new),
            pack_key: location.pack_key,
        };
        Ok(Some(
            stream::unfold(Some(reading), |state| async move {
                let state = state?;
                match state.next().await {
                    Ok(Some((window, rest))) => Some((Ok(window), rest)),
                    Ok(None) => None,
                    Err(status) => Some((Err(status), None)),
                }
            })
            .boxed(),
        ))
    }

    /// Stores the blob `digest` (over [`Packs::max_blob_bytes`], or any other) as the object
    /// [`large_key`] from `pieces`, as it arrives, and records it in the index. Nothing is held
    /// whole. A blob the index already has is held: `open` is not called, so nothing is read.
    ///
    /// # Errors
    /// INVALID_ARGUMENT when the pieces `open` gives are not exactly the digest's bytes (nothing is stored);
    /// the client's own error when a piece is one; UNAVAILABLE when the object store or the
    /// index fails, or an object of another length is under the key.
    pub async fn put_large(
        &self,
        digest: &Digest,
        open: impl FnOnce() -> Pieces + Send,
    ) -> Result<(), Status> {
        if self.locate_stored(digest).await?.is_some() {
            return Ok(());
        }
        let name = large_key(digest);
        let key = ObjectKey::new(&name)
            .map_err(|e| Status::unavailable(format!("the key of {digest}: {e}")))?;
        let failed = |e: &dyn std::fmt::Display| Status::unavailable(format!("object store: {e}"));
        match self.store.len(&key).await.map_err(|e| failed(&e))? {
            Some(len) if len == digest.size_bytes => {}
            Some(len) => {
                return Err(Status::unavailable(format!(
                    "object {name} is {len} bytes, not the {} bytes of {digest}: not replaced",
                    digest.size_bytes
                )));
            }
            None => {
                let refused = Arc::new(Mutex::new(None));
                let body = verified(open(), *digest, Arc::clone(&refused));
                let put = self.store.put_stream(&key, digest.size_bytes, body).await;
                let refusal = refused
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .take();
                if let Some(status) = refusal {
                    return Err(status);
                }
                match put {
                    // The key is content-bound: whoever wrote it wrote these bytes.
                    Ok(()) | Err(ObjectStoreError::AlreadyExists(_)) => {}
                    Err(e) => {
                        self.metrics.write_failures.add(1);
                        tracing::warn!(error = %e, blob = %digest, "a large blob was not stored");
                        return Err(failed(&e));
                    }
                }
            }
        }
        let pack = NewPack {
            key: name,
            writer: self.writer.clone(),
            size: digest.size_bytes,
            blobs: vec![PackBlob {
                digest: to_index(digest),
                offset: 0,
            }],
        };
        let index = Arc::clone(&self.index);
        let recorded = offload(move || index.record_pack(&pack)).await;
        recorded.map_err(index_failed)?;
        self.metrics.writes.add(1);
        self.metrics.written_bytes.add(digest.size_bytes);
        self.metrics.written_blobs.add(1);
        Ok(())
    }
}

/// One read of a range of a large blob, a window per step.
struct Reading {
    packs: Packs,
    digest: Digest,
    key: ObjectKey,
    pack_key: String,
    /// Where the blob starts in the object.
    base: u64,
    pos: u64,
    end: u64,
    /// The hash of the bytes read so far, when the range is the whole blob.
    hasher: Option<Sha256>,
}

impl Reading {
    /// The next window and the state after it, `None` at the end of the range.
    async fn next(mut self) -> Result<Option<(Bytes, Option<Self>)>, Status> {
        let Some(range) = ByteRange::new(
            self.base + self.pos,
            READ_WINDOW_BYTES.min(self.end.saturating_sub(self.pos)),
        ) else {
            return Ok(None);
        };
        let wanted = range.size();
        let window = self
            .packs
            .store
            .read_range(&self.key, range)
            .await
            .map_err(|e| {
                self.packs.metrics.read_errors.add(1);
                Status::unavailable(format!(
                    "reading {} from the object store: {e}",
                    self.digest
                ))
            })?;
        if window.len() as u64 != wanted {
            self.packs.metrics.lost.add(1);
            return Err(Status::unavailable(format!(
                "the object of {} is shorter than the index says",
                self.digest
            )));
        }
        self.pos += wanted;
        self.packs.metrics.read_bytes.add(wanted);
        if let Some(hasher) = &mut self.hasher {
            hasher.update(&window);
        }
        if self.pos < self.end {
            return Ok(Some((window, Some(self))));
        }
        // The last window is sent only once the whole blob has checked.
        if let Some(hasher) = self.hasher.take() {
            let hash: [u8; 32] = hasher.finalize().into();
            if hash != self.digest.hash {
                return Err(self.damaged().await);
            }
        }
        self.packs.metrics.reads.add(1);
        Ok(Some((window, None)))
    }

    /// The failure of a blob whose object is other bytes than its digest: the row is dropped,
    /// so the blob is missing and a client uploads it again.
    async fn damaged(&self) -> Status {
        let digest = to_index(&self.digest);
        let (index, pack_key) = (Arc::clone(&self.packs.index), self.pack_key.clone());
        let dropped = offload(move || index.drop_location(&digest, &pack_key)).await;
        self.packs.metrics.digest_mismatches.add(1);
        self.packs
            .metrics
            .locations_dropped
            .add(u64::from(dropped.unwrap_or(false)));
        tracing::error!(blob = %from_index(&digest), "an object holds other bytes than its digest: not served");
        Status::unavailable(format!(
            "blob {} is stored as other bytes than its digest",
            self.digest
        ))
    }
}

/// `pieces` as the body of a put of the blob `digest`: each piece handed on one piece late and
/// the last only once the bytes have hashed to the digest, so the put cannot have received the
/// whole blob before it is checked. A refusal (the client's error, more bytes than the digest
/// names, fewer, or other bytes) is left in `refused` and ends the body with an error.
fn verified(pieces: Pieces, digest: Digest, refused: Arc<Mutex<Option<Status>>>) -> BodyStream {
    let start = (pieces, Sha256::new(), 0_u64, None::<Bytes>);
    let refuse = move |status: Status| {
        let why = status.message().to_owned();
        refused
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get_or_insert(status);
        Some((Err(ObjectStoreError::Body(why)), None))
    };
    stream::unfold(Some(start), move |state| {
        let refuse = refuse.clone();
        async move {
            let (mut pieces, mut hasher, mut received, mut behind) = state?;
            loop {
                match pieces.next().await {
                    Some(Ok(piece)) if piece.is_empty() => {}
                    Some(Ok(piece)) => {
                        received += piece.len() as u64;
                        if received > digest.size_bytes {
                            let more =
                                format!("more than the {} bytes {digest} names", digest.size_bytes);
                            return refuse(Status::invalid_argument(more));
                        }
                        hasher.update(&piece);
                        if let Some(held) = behind.replace(piece) {
                            let rest = (pieces, hasher, received, behind);
                            return Some((Ok(held), Some(rest)));
                        }
                    }
                    Some(Err(status)) => return refuse(status),
                    None => {
                        if received != digest.size_bytes {
                            let why = format!(
                                "{received} bytes were sent for {digest}, which names {}",
                                digest.size_bytes
                            );
                            return refuse(Status::invalid_argument(why));
                        }
                        let hash: [u8; 32] = hasher.finalize().into();
                        if hash != digest.hash {
                            let why = format!("blob {digest}: the data is of another digest");
                            return refuse(Status::invalid_argument(why));
                        }
                        // A blob of a size of at least one byte has a last piece.
                        return behind.map(|last| (Ok(last), None));
                    }
                }
            }
        }
    })
    .boxed()
}
