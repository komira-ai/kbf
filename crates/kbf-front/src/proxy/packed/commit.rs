//! The commit of a build that wrote one pack object through a presigned write: the frontend
//! records it in the pack index after reading the pack back (the `untrusted` module has the
//! rules, [`Packs::commit_untrusted_pack`] the entry).
//!
//! This module holds what a commit is made of: the reads of the pack from the object store
//! (ranges of at most `READ_CHUNK_BYTES`, so that no blob is held whole), the errors a commit can
//! end in ([`CommitError`]), and the counts and log lines of every commit. Every refusal that is
//! the commit's own (not a failure of the store or the index) is counted in
//! `kbf_pack_commit_rejects_total` and logged; a recorded pack is counted in
//! `kbf_pack_commits_total`. Every commit is of an untrusted pack, so it is counted also in
//! `kbf_pack_untrusted_commits_total` and, for a refusal, in
//! `kbf_pack_untrusted_commit_rejects_total` by `reason`. The index is written on the blocking
//! pool.

use bytes::Bytes;
use kbf_objstore::{ByteRange, ObjectKey, ObjectStoreError};
use kbf_pack_index::{BlobDigest, PackBlob, PackIndexError, Recorded};
use kbf_segments::RecordHasher;

use super::store::{Packs, offload};

mod untrusted;

pub use self::untrusted::{
    Ingested, IssuedKeySet, IssuedPackKey, IssuedPutDir, MAX_UNTRUSTED_BLOB_BYTES,
    MAX_UNTRUSTED_LARGE_BYTES, MAX_UNTRUSTED_MANIFEST_BYTES, MAX_UNTRUSTED_PACK_BYTES,
    MAX_UNTRUSTED_READ_BYTES, MIN_SECRET_BYTES, MintError, PackKeyMint, RejectReason, StoreError,
    UntrustedLimits, ingest, ingest_put_dir, ingest_put_dir_with, ingest_set,
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
    /// The digest and the CRC-32C of the bytes of the pack `key` where `blob` says it is, read
    /// in ranges of at most `READ_CHUNK_BYTES` so that a large blob is never held whole.
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
