//! The packer: blobs into packs, and the write rule.
//!
//! A pack is one object in the segment layout of `kbf-segments` (the blobs back to back, then a
//! footer that indexes them). The packer writes a pack to the object store with a create-only
//! write, and only when the store has acknowledged it does the packer record the pack in the
//! pack index. A row of the index therefore never points at a pack the store does not hold.
//! Failures leave only what is harmless:
//!
//! - a write that fails records nothing; the packer deletes what the failed attempt may have
//!   left and tries again under the next sequence number, up to [`PackerConfig::put_attempts`]
//!   times;
//! - a key that is taken (a create-only write refused) belongs to someone else and is left
//!   alone; the packer tries the next sequence number;
//! - a record that fails after the write is tried again, with the same pack and key, up to
//!   [`PackerConfig::put_attempts`] times, as the index takes a repeat of a record as a no-op;
//!   an error for which a repeat gets the same answer ([`PackIndexError::is_final`]: a refusal)
//!   ends the attempts at once;
//!   when every attempt fails the pack is one no row points at, which the index reports as
//!   unreferenced and garbage collection deletes after its grace; the blobs are still missing
//!   from the index, so packing them again writes a new pack.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use bytes::Bytes;
use kbf_objstore::{ObjectKey, ObjectStore, ObjectStoreError};
use kbf_pack_index::{
    BlobDigest, MAX_BATCH, NewPack, PackBlob, PackIndex, PackIndexError, Recorded,
};
use kbf_segments::layout::footer_len;
use kbf_segments::{Footer, MAX_SEGMENT_BYTES, SegmentWriter};

use crate::blocking::offload;
use crate::digest::from_segment;
use crate::error::PackError;
use crate::keys::PackKeys;

/// The longest a pack is by default: the segment limit of the farm, 128 MiB. A frontend that
/// packs client uploads sets [`FRONTEND_PACK_BYTES`].
pub const DEFAULT_PACK_BYTES: u64 = MAX_SEGMENT_BYTES;

/// The length at which a frontend closes the pack of client uploads, 32 MiB.
pub const FRONTEND_PACK_BYTES: u64 = 32 << 20;

/// The most blobs a pack holds by default. The footer costs 52 bytes a blob, so this keeps
/// it under 1 MiB.
pub const DEFAULT_MAX_BLOBS: usize = 16_384;

/// How many times a pack is written before the packer gives up, each under a new key.
pub const DEFAULT_PUT_ATTEMPTS: u32 = 3;

/// How the packer cuts packs and retries writes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PackerConfig {
    /// The most bytes a pack has, footer included. A blob that does not fit even an empty pack
    /// of this length (the blob plus a one-blob footer) is written as a pack of its own.
    pub pack_bytes: u64,
    /// The most blobs in a pack. At least one is used.
    pub max_blobs: usize,
    /// How many times a pack is written, each under a new key, before the call fails; and how
    /// many times the record of a written pack is tried, under its one key. At least one is
    /// used.
    pub put_attempts: u32,
}

impl Default for PackerConfig {
    fn default() -> Self {
        Self {
            pack_bytes: DEFAULT_PACK_BYTES,
            max_blobs: DEFAULT_MAX_BLOBS,
            put_attempts: DEFAULT_PUT_ATTEMPTS,
        }
    }
}

/// A blob to pack: its bytes and the digest the caller says they have.
#[derive(Clone, Debug)]
pub struct Blob {
    /// The digest the caller names. The packer checks the bytes against it.
    pub digest: BlobDigest,
    /// The blob's bytes.
    pub bytes: Bytes,
}

/// A pack the packer stored and recorded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredPack {
    /// The pack's object key.
    pub key: String,
    /// The pack's length in bytes, footer included.
    pub size: u64,
    /// How many blobs the pack holds.
    pub blobs: usize,
    /// How many of them the index points at this pack.
    pub indexed: u32,
}

/// What a call of [`Packer::pack`] did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Packed {
    /// The packs written, in order.
    pub packs: Vec<StoredPack>,
    /// How many of the blobs given were not packed: the index had them already, or the call
    /// named them twice.
    pub skipped: usize,
}

/// The write rule over one object store and one pack index.
pub struct Packer<S: ObjectStore> {
    store: Arc<S>,
    index: Arc<dyn PackIndex>,
    keys: PackKeys,
    config: PackerConfig,
    /// The sequence number of the next key.
    next_seq: AtomicU64,
}

impl<S: ObjectStore> Packer<S> {
    /// A packer that writes to `store`, records in `index` and names its packs by `keys`.
    ///
    /// # Errors
    /// [`PackError::NotCreateOnly`] when the store does not claim create-only writes. Run
    /// [`check_create_only`] at startup to check the claim.
    pub fn new(
        store: Arc<S>,
        index: Arc<dyn PackIndex>,
        keys: PackKeys,
        config: PackerConfig,
    ) -> Result<Self, PackError> {
        if !store.capabilities().conditional_put {
            return Err(PackError::NotCreateOnly);
        }
        let config = PackerConfig {
            max_blobs: config.max_blobs.max(1),
            ..config
        };
        Ok(Self {
            store,
            index,
            keys,
            config,
            next_seq: AtomicU64::new(0),
        })
    }

    /// Packs the blobs the index does not have yet and records the packs.
    ///
    /// The index is asked which blobs it lacks (in batches), and only those are packed, in
    /// the order given, each pack cut at [`PackerConfig::pack_bytes`] or
    /// [`PackerConfig::max_blobs`]. Each pack is written and then recorded before the next
    /// one is started, so the packs finished when a call fails stay recorded and a repeated
    /// call packs only the rest.
    ///
    /// # Errors
    /// [`PackError::Index`] when the index fails a lookup; [`PackError::DigestMismatch`] when
    /// the bytes of a blob are not the digest named for them, or are not as long as the digest
    /// says (this is checked before anything is written); [`PackError::Put`] when every
    /// attempt to write a pack failed; [`PackError::Record`] when a pack was written and every
    /// attempt of the index to record it failed.
    pub async fn pack(&self, blobs: Vec<Blob>) -> Result<Packed, PackError> {
        let total = blobs.len();
        for blob in &blobs {
            if blob.bytes.len() as u64 != blob.digest.size {
                return Err(PackError::DigestMismatch {
                    claimed: blob.digest,
                });
            }
        }
        let mut missing = self.missing(&blobs).await?;
        let limit = self.config.pack_bytes;
        let mut packs = Vec::new();
        let mut open = SegmentWriter::new(limit);
        let mut in_open = 0_usize;
        for blob in blobs {
            if !missing.remove(&blob.digest) {
                continue;
            }
            let alone = blob.digest.size.saturating_add(footer_len(1));
            if alone > limit {
                let mut writer = SegmentWriter::new(alone);
                add(&mut writer, &blob)?;
                packs.push(self.commit(writer).await?);
                continue;
            }
            // One condition per `if`: the open pack is full by count, or the blob does not fit.
            let added = if in_open < self.config.max_blobs {
                add(&mut open, &blob)?
            } else {
                false
            };
            if !added {
                let full = std::mem::replace(&mut open, SegmentWriter::new(limit));
                packs.push(self.commit(full).await?);
                add(&mut open, &blob)?;
                in_open = 0;
            }
            in_open += 1;
        }
        if in_open > 0 {
            packs.push(self.commit(open).await?);
        }
        let packed: usize = packs.iter().map(|pack| pack.blobs).sum();
        Ok(Packed {
            packs,
            skipped: total - packed,
        })
    }

    /// The digests among `blobs` that the index has no row for.
    async fn missing(&self, blobs: &[Blob]) -> Result<BTreeSet<BlobDigest>, PackIndexError> {
        let digests: Vec<BlobDigest> = blobs.iter().map(|blob| blob.digest).collect();
        let mut missing = BTreeSet::new();
        for chunk in digests.chunks(MAX_BATCH) {
            let index = Arc::clone(&self.index);
            let batch = chunk.to_vec();
            missing.extend(offload(move || index.find_missing(&batch)).await?);
        }
        Ok(missing)
    }

    /// Writes the pack `writer` holds, then records it.
    async fn commit(&self, writer: SegmentWriter) -> Result<StoredPack, PackError> {
        let segment = Bytes::from(writer.finish());
        let footer = Footer::parse(&segment, segment.len() as u64)
            .expect("a segment just finished has a footer that parses");
        let blobs: Vec<PackBlob> = footer
            .entries()
            .iter()
            .map(|entry| PackBlob {
                digest: from_segment(&entry.digest),
                offset: entry.offset,
            })
            .collect();
        let key = self.write_pack(&segment).await?.to_string();
        // The store holds the pack: only now may a row point at it.
        let pack = NewPack {
            key: key.clone(),
            writer: self.keys.writer(),
            size: segment.len() as u64,
            blobs,
        };
        let (size, blobs) = (pack.size, pack.blobs.len());
        let recorded = self.record(pack).await?;
        Ok(StoredPack {
            key,
            size,
            blobs,
            indexed: recorded.indexed,
        })
    }

    /// Records `pack`, trying again as the config says, but not after a refusal. The index
    /// takes a repeat of a key it recorded with the same contents as a no-op, so a record
    /// whose answer was lost is safe to send again.
    async fn record(&self, pack: NewPack) -> Result<Recorded, PackError> {
        let pack = Arc::new(pack);
        let mut attempt = 1;
        loop {
            let index = Arc::clone(&self.index);
            let sent = Arc::clone(&pack);
            let source = match offload(move || index.record_pack(&sent)).await {
                Ok(recorded) => return Ok(recorded),
                Err(source) => source,
            };
            // A refusal is the same on every repeat: the caller's fallback is the way on.
            let tried_out = attempt >= self.config.put_attempts;
            let refused = source.is_final();
            let stop = if refused { refused } else { tried_out };
            if stop {
                return Err(PackError::Record {
                    key: pack.key.clone(),
                    source,
                });
            }
            attempt += 1;
        }
    }

    /// Writes `segment` under a key no object has, trying new keys as the config says.
    async fn write_pack(&self, segment: &Bytes) -> Result<ObjectKey, PackError> {
        let mut attempt = 1;
        loop {
            let seq = self.next_seq.fetch_add(1, Ordering::Relaxed);
            let key = self.keys.key(seq);
            let error = match self.store.put_new(&key, segment.clone(), None).await {
                Ok(()) => return Ok(key),
                Err(error) => error,
            };
            // A key that is taken holds someone else's object: it is not ours to delete.
            if !matches!(error, ObjectStoreError::AlreadyExists(_)) {
                self.discard(&key).await;
            }
            if attempt >= self.config.put_attempts {
                return Err(PackError::Put {
                    key: key.to_string(),
                    attempts: attempt,
                    source: error,
                });
            }
            attempt += 1;
        }
    }

    /// Deletes what a write that reported a failure may have left under `key`. If the delete
    /// fails too the object is an unreferenced pack, which garbage collection deletes.
    async fn discard(&self, key: &ObjectKey) {
        if let Err(error) = self.store.delete(key).await {
            tracing::warn!(%key, %error, "could not delete a pack whose write failed");
        }
    }
}

/// Pushes `blob` into `writer`: `true` when it fit, `false` when the pack is full.
fn add(writer: &mut SegmentWriter, blob: &Blob) -> Result<bool, PackError> {
    let Ok(digest) = writer.push(&blob.bytes) else {
        return Ok(false);
    };
    if from_segment(&digest) != blob.digest {
        return Err(PackError::DigestMismatch {
            claimed: blob.digest,
        });
    }
    Ok(true)
}

/// Checks that `store` refuses to replace an object, as the write rule needs: writes a key
/// of the writer's prefix twice, and the second write must be refused. Run it once when the
/// process starts and do not pack if it fails.
///
/// The key is the same for every call with the same [`PackKeys`], and the delete at the end
/// may be refused (a writer that may not delete): the object is then left in the writer's
/// prefix, where garbage collection deletes it, and a second call with the same keys fails
/// with [`PackError::Store`] at the first write, as the key is taken. A boot id that is new on
/// every start makes the keys of one start differ from those of the next.
///
/// # Errors
/// [`PackError::Store`] when the first write fails; [`PackError::CreateOnlySelfTest`] when
/// the second is not refused as an object that exists.
pub async fn check_create_only<S: ObjectStore>(
    store: &S,
    keys: &PackKeys,
) -> Result<(), PackError> {
    let key = keys.self_test_key();
    store
        .put_new(&key, Bytes::from_static(b"first"), None)
        .await?;
    let second = store
        .put_new(&key, Bytes::from_static(b"second"), None)
        .await;
    // Best effort: an object left behind is unreferenced, and garbage collection deletes it.
    let _ = store.delete(&key).await;
    match second {
        Err(ObjectStoreError::AlreadyExists(_)) => Ok(()),
        _ => Err(PackError::CreateOnlySelfTest),
    }
}
