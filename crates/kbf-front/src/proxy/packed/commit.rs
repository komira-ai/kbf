//! `CommitPack`: a pack that a daemon stored in the object store is recorded in the pack index.
//!
//! A daemon that writes its outputs into packs stores a pack with a create-only write and then
//! asks the frontend to record it ([`Packs::commit_pack`]). The frontend does not take the
//! daemon's word for what the pack holds: before the index points any blob at the pack it reads
//! the pack's last byte and the one after it (so a pack that is not stored, or is shorter or
//! longer than the commit says, is refused) and re-hashes a sample of the blobs the commit lists,
//! by ranged reads of at most `READ_CHUNK_BYTES` each, so that no blob is held whole. The blobs
//! the commit lists add up to at most the pack's size, which bounds the bytes one commit can make
//! the frontend read. The pack's footer is not read. A blob whose bytes are not its digest
//! refuses the whole commit and nothing is recorded. The blobs that were not sampled are checked
//! by every read of them (a read hashes what it serves), so a daemon that lies about one is
//! caught at its first read, which fails and runs the action again.
//!
//! The sample is chosen from the commit alone ([`MAX_COMMIT_BLOBS`] bounds its list): `sample`
//! blobs spread over the list from a start that a hash of the pack's name picks, so the same
//! commit is always checked at the same blobs. A `sample` of 0 checks no blob (only that the pack
//! is stored); one at least as large as the list checks them all.
//!
//! A daemon may commit only its own packs: the commit names the writer, which must be the node
//! the caller's certificate names, and the pack must be named `cas/d/<writer>/<boot>/<sequence>`.
//! The record is idempotent on the pack's name: a commit sent again with the same contents is
//! answered `created: false`, and one with other contents is [`CommitError::Conflict`]. The
//! index is written on the blocking pool.
//!
//! A pack written by a build that is not trusted (a presigned write) is committed by
//! [`Packs::commit_untrusted_pack`] instead, which reads the footer and hashes every byte (the
//! `untrusted` module has the rules). A daemon's commit cannot be made for such a pack: its name
//! must be under `cas/d/`.
//!
//! Every refusal that is the commit's own (not a failure of the store or the index) is counted
//! in `kbf_pack_commit_rejects_total` and logged. A commit of an untrusted pack is counted there
//! too, and also in `kbf_pack_untrusted_commits_total` and, for a refusal, in
//! `kbf_pack_untrusted_commit_rejects_total` by `reason`.

use std::sync::Arc;

use bytes::Bytes;
use kbf_objstore::{ByteRange, ObjectKey, ObjectStoreError};
use kbf_pack::blob_digest;
use kbf_pack_index::{BlobDigest, NewPack, PackBlob, PackIndexError, Recorded};
use kbf_segments::RecordHasher;

use super::store::{Packs, from_index, offload};

mod untrusted;

pub use self::untrusted::{
    Ingested, IssuedKeySet, IssuedPackKey, MAX_UNTRUSTED_BLOB_BYTES, MAX_UNTRUSTED_LARGE_BYTES,
    MAX_UNTRUSTED_MANIFEST_BYTES, MAX_UNTRUSTED_PACK_BYTES, MAX_UNTRUSTED_READ_BYTES,
    MIN_SECRET_BYTES, MintError, PackKeyMint, RejectReason, StoreError, UntrustedLimits, ingest,
    ingest_set,
};

/// The most blobs one commit lists: the most the packer puts in a pack.
pub const MAX_COMMIT_BLOBS: usize = 16_384;

/// The most bytes of a pack read from the store by one request of a check.
const READ_CHUNK_BYTES: u64 = 8 * 1024 * 1024;

/// Why a pack was not committed. Nothing was recorded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CommitError {
    /// The commit names a writer other than the caller.
    Forbidden(String),
    /// The commit cannot be one: the pack's name, its size or its list of blobs (or the index
    /// refuses the list).
    Invalid(String),
    /// The pack is not in the object store, or is not as long as the commit says.
    Missing(String),
    /// A blob that was checked is not the bytes its digest names.
    Mismatch(String),
    /// The pack was recorded before with other contents.
    Conflict(String),
    /// The object store or the index could not answer. The commit may be sent again.
    Unavailable(String),
}

impl CommitError {
    /// What went wrong, as text.
    #[must_use]
    pub fn message(&self) -> &str {
        match self {
            Self::Forbidden(text)
            | Self::Invalid(text)
            | Self::Missing(text)
            | Self::Mismatch(text)
            | Self::Conflict(text)
            | Self::Unavailable(text) => text,
        }
    }
}

impl Packs {
    /// Records `pack`, which the daemon `caller` says it stored, after checking `sample` of its
    /// blobs against the bytes in the object store (the module docs say how). `pack.writer` is
    /// the daemon's node id and must be `caller`; the index records the writer as `d/<node>`.
    ///
    /// # Errors
    /// [`CommitError`]: the writer is not the caller, the name, the size or the blobs cannot be
    /// a pack's, the pack is not in the store, a sampled blob is not its digest, the name was
    /// recorded with other contents, or the store or the index failed.
    pub async fn commit_pack(
        &self,
        caller: &str,
        pack: NewPack,
        sample: usize,
    ) -> Result<Recorded, CommitError> {
        let key = pack.key.clone();
        let done = self.committed(caller, pack, sample).await;
        self.counted(&key, done)
    }

    /// The commit, without its counts.
    async fn committed(
        &self,
        caller: &str,
        pack: NewPack,
        sample: usize,
    ) -> Result<Recorded, CommitError> {
        let key = admissible(caller, &pack)?;
        let checked = sampled(pack.blobs.len(), sample, &pack.key);
        self.sound(&key, &pack, &checked).await?;
        let recorded = NewPack {
            writer: format!("d/{}", pack.writer),
            ..pack
        };
        let index = Arc::clone(&self.index);
        let done = offload(move || index.record_pack(&recorded)).await;
        done.map_err(recording_failed)
    }

    /// Checks that the pack `key` is stored and that the blobs of `pack` at `checked` are
    /// their digests.
    async fn sound(
        &self,
        key: &ObjectKey,
        pack: &NewPack,
        checked: &[usize],
    ) -> Result<(), CommitError> {
        let last = ByteRange::new(pack.size - 1, 2).expect("the pack is at least one byte");
        let tail = self.fetch(key, last).await?;
        if tail.len() != 1 {
            let message = format!("{key} is longer than the {} bytes said", pack.size);
            return Err(CommitError::Missing(message));
        }
        for &at in checked {
            let blob = &pack.blobs[at];
            if self.hashed(key, blob).await? != blob.digest {
                let digest = from_index(&blob.digest);
                let offset = blob.offset;
                let message = format!("the blob {digest} at {offset} of {key} holds other bytes");
                return Err(CommitError::Mismatch(message));
            }
        }
        Ok(())
    }

    /// The digest of the bytes of the pack `key` where `blob` says it is, read in ranges of at
    /// most `READ_CHUNK_BYTES` so that a large blob is never held whole.
    async fn hashed(&self, key: &ObjectKey, blob: &PackBlob) -> Result<BlobDigest, CommitError> {
        Ok(self.hashed_crc(key, blob).await?.0)
    }

    /// [`Self::hashed`], and the CRC-32C of the bytes as well.
    async fn hashed_crc(
        &self,
        key: &ObjectKey,
        blob: &PackBlob,
    ) -> Result<(BlobDigest, u32), CommitError> {
        let mut hasher = RecordHasher::new();
        let end = blob.offset + blob.digest.size;
        let mut at = blob.offset;
        while at < end {
            let take = (end - at).min(READ_CHUNK_BYTES);
            let range = ByteRange::new(at, take).expect("a chunk has bytes and an end");
            hasher.update(&self.fetch(key, range).await?);
            at += take;
        }
        let (digest, crc) = hasher.finish();
        let digest = BlobDigest {
            hash: digest.hash,
            size: digest.size_bytes,
        };
        Ok((digest, crc))
    }

    /// The bytes of `range` of the pack `key`, or why the store has none.
    async fn fetch(&self, key: &ObjectKey, range: ByteRange) -> Result<Bytes, CommitError> {
        let stored = self.store.read_range(key, range).await;
        match stored {
            Ok(bytes) => Ok(bytes),
            Err(error) if absent(&error) => {
                let message = format!("{key} is not in the store, or is shorter than said");
                Err(CommitError::Missing(message))
            }
            Err(error) => {
                let message = format!("reading {key}: {error}");
                Err(CommitError::Unavailable(message))
            }
        }
    }

    /// `done`, counted and logged.
    fn counted(
        &self,
        key: &str,
        done: Result<Recorded, CommitError>,
    ) -> Result<Recorded, CommitError> {
        match &done {
            Ok(recorded) if recorded.created => self.metrics.commits.add(1),
            Ok(_) => {}
            Err(CommitError::Unavailable(reason)) => {
                tracing::warn!(
                    key,
                    reason,
                    "a pack could not be committed: the store or the index failed"
                );
            }
            Err(refused) => {
                self.metrics.commit_rejects.add(1);
                let reason = refused.message();
                tracing::warn!(key, reason, "a pack commit was refused");
            }
        }
        done
    }
}

/// The pack's key, once the commit is one that `caller` may make: the caller is the writer, the
/// name is `cas/d/<writer>/<boot>/<sequence>`, and the size and the list of blobs can be a
/// pack's.
fn admissible(caller: &str, pack: &NewPack) -> Result<ObjectKey, CommitError> {
    if pack.writer != caller {
        return Err(CommitError::Forbidden(format!(
            "the node {caller:?} may not commit a pack of {:?}",
            pack.writer
        )));
    }
    let key = named(pack)?;
    let given = pack.blobs.len();
    if given > MAX_COMMIT_BLOBS {
        let message = format!("{given} blobs in one pack, at most {MAX_COMMIT_BLOBS}");
        return Err(CommitError::Invalid(message));
    }
    if pack.size == 0 {
        return Err(CommitError::Invalid("the pack is empty".to_owned()));
    }
    let past = |blob: &PackBlob| {
        let end = blob.offset.checked_add(blob.digest.size);
        end.is_none_or(|end| end > pack.size)
    };
    if pack.blobs.iter().any(past) {
        let message = "a blob reaches past the end of the pack".to_owned();
        return Err(CommitError::Invalid(message));
    }
    let mut sizes = pack.blobs.iter().map(|blob| blob.digest.size);
    let held = sizes.try_fold(0_u64, u64::checked_add);
    if held.is_none_or(|held| held > pack.size) {
        let message = "the blobs add up to more bytes than the pack holds".to_owned();
        return Err(CommitError::Invalid(message));
    }
    Ok(key)
}

/// The key of `pack`, if its name is `cas/d/<writer>/<boot>/<sequence>`: under its writer's
/// prefix, and nothing but a boot and a sequence number below it.
fn named(pack: &NewPack) -> Result<ObjectKey, CommitError> {
    let key = &pack.key;
    let under = format!("cas/d/{}/", pack.writer);
    if !key.starts_with(&under) {
        return Err(misnamed(key, &under));
    }
    if key.matches('/').count() != 4 {
        return Err(misnamed(key, &under));
    }
    let made = ObjectKey::new(key.clone());
    made.map_err(|error| CommitError::Invalid(error.to_string()))
}

/// The refusal for a pack name that is not `under` and a boot and a sequence number.
fn misnamed(key: &str, under: &str) -> CommitError {
    let message = format!("the pack {key:?} is not {under}<boot>/<sequence>");
    CommitError::Invalid(message)
}

/// Which of `blobs` blobs are checked: `sample` of them, or all if there are fewer, spread over
/// the list from a start that a hash of the pack's name `key` picks. The same arguments always
/// give the same blobs, and no blob twice.
fn sampled(blobs: usize, sample: usize, key: &str) -> Vec<usize> {
    let wanted = sample.min(blobs);
    let [a, b, c, d, e, f, g, h, ..] = blob_digest(key.as_bytes()).hash;
    let seed = u64::from_be_bytes([a, b, c, d, e, f, g, h]);
    let start = (seed % blobs.max(1) as u64) as usize;
    (0..wanted)
        .map(|i| (i * blobs / wanted + start) % blobs)
        .collect()
}

/// Whether the store answered that it does not have the bytes asked for.
fn absent(error: &ObjectStoreError) -> bool {
    matches!(
        error,
        ObjectStoreError::NotFound(_) | ObjectStoreError::InvalidRange { .. }
    )
}

/// The refusal for an index that did not record the pack.
fn recording_failed(error: PackIndexError) -> CommitError {
    match error {
        PackIndexError::Invalid(why) => {
            let message = format!("the pack index refuses the pack: {why}");
            CommitError::Invalid(message)
        }
        PackIndexError::Conflict { key } => {
            let message = format!("the pack {key:?} is recorded with other contents");
            CommitError::Conflict(message)
        }
        other => {
            let message = format!("recording the pack: {other}");
            CommitError::Unavailable(message)
        }
    }
}
