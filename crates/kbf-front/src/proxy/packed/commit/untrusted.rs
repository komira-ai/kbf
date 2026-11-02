//! `CommitPack` for a writer that is not trusted: a build that wrote one pack object through a
//! presigned write.
//!
//! [`Packs::commit_pack`] takes the word of a certified daemon: it checks a sample of the blobs
//! and does not read the footer. This is the other entry, [`Packs::commit_untrusted_pack`], and
//! nothing a daemon sends can reach it or be mistaken for it: it takes an [`IssuedPackKey`], which
//! only the frontend that issued the write can make, and it records the pack as written by
//! `b/<build>`. It trusts nothing the object says about itself:
//!
//! - **Authorization.** The pack's key must be exactly the key issued for the build's lease
//!   (`cas/b/<build>/<nonce>`) and the manifest must name that build. The manifest (key, size and
//!   the blobs with their offsets) comes from the lease's authenticated channel, never from the
//!   object.
//! - **Shape, before any read.** At most [`MAX_UNTRUSTED_PACK_BYTES`] bytes, at most
//!   [`MAX_COMMIT_BLOBS`] blobs, none over [`MAX_UNTRUSTED_BLOB_BYTES`]; blobs listed in strictly
//!   ascending (hash, size) order; records that start at offset 0, follow each other with no gap and
//!   no overlap, and end exactly where the footer starts (the pack's size less the footer's length).
//!   An untrusted pack may not hold a record of no bytes: offset 0 would be ambiguous and any
//!   number of digests could share one offset. The empty blob needs no record, its digest is known.
//!   The bytes the commit will read (the trailer, the footer and every record) must fit
//!   [`UntrustedLimits::read_budget`]; a commit over it is refused without a read.
//! - **Footer.** The trailer's magic, version and CRC, its blob count and where the records end
//!   must be as the manifest says, and the footer's entries must equal the manifest exactly: the
//!   same (hash, size, offset), nothing more and nothing less.
//! - **Every byte.** Each record is read, in ranges of at most `READ_CHUNK_BYTES`, and hashed; its
//!   SHA-256 must be the manifest's and its CRC-32C the footer's.
//!
//! The pack is then recorded where it lies, with no copy, as for a daemon's. The record is
//! idempotent on the key: the same contents are answered `created: false` and other contents are
//! [`CommitError::Conflict`].
//!
//! What this does not do: it cannot stop the writer from replacing the object after the commit
//! has read it. That is for the store (a create-only write, or the end of the write's validity).

use std::sync::Arc;

use kbf_objstore::{ByteRange, ObjectKey};
use kbf_pack_index::{NewPack, PackBlob, Recorded};
use kbf_segments::layout::{TRAILER_LEN, footer_len};
use kbf_segments::{Footer, MAX_SEGMENT_BYTES, SegmentError};

use super::{CommitError, MAX_COMMIT_BLOBS, Packs, offload, recording_failed};
use crate::proxy::packed::metrics::Reason;

mod large;
mod metadata;
mod mint;
mod set;
mod task;

pub use self::mint::{MIN_SECRET_BYTES, MintError, PackKeyMint};
pub use self::set::IssuedKeySet;
pub use self::task::{Ingested, RejectReason, StoreError, ingest, ingest_set};

/// The most bytes a pack of an untrusted writer may have: the most a segment may have.
pub const MAX_UNTRUSTED_PACK_BYTES: u64 = MAX_SEGMENT_BYTES;

/// The most bytes a blob of an untrusted writer's pack may have: the most a daemon puts in a pack
/// (a blob over it is not read from packs).
pub const MAX_UNTRUSTED_BLOB_BYTES: u64 = 64 << 20;

/// The most bytes the record at offset 0 of an untrusted pack may have when the caller reads that
/// record as the pack's manifest (the caller applies it; this module does not know which record
/// is one).
pub const MAX_UNTRUSTED_MANIFEST_BYTES: u64 = 4 << 20;

/// The most bytes one commit of an untrusted pack may read from the store: every byte of a pack
/// of [`MAX_UNTRUSTED_PACK_BYTES`] once, and the trailer twice (the read of the trailer, which
/// also shows that the pack is not longer than said by one byte, and the read of the footer).
pub const MAX_UNTRUSTED_READ_BYTES: u64 = MAX_UNTRUSTED_PACK_BYTES + TRAILER_LEN as u64 + 1;

/// The most bytes one large object of a task may have by default (`--task-large-max-bytes`): 512
/// MiB, which the object store's own cap for the large-object key must equal.
pub const MAX_UNTRUSTED_LARGE_BYTES: u64 = 512 << 20;

/// The key the frontend issued for one build's write of one pack: `cas/b/<build>/<nonce>`.
///
/// A commit of an untrusted pack is authorized by this key and no other.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IssuedPackKey {
    build: String,
    key: String,
    object: ObjectKey,
}

impl IssuedPackKey {
    /// The key `cas/b/<build>/<nonce>`.
    ///
    /// # Errors
    /// [`CommitError::Invalid`]: `build` or `nonce` is empty or holds a `/`, or the key is not an
    /// object key.
    pub fn new(build: &str, nonce: &str) -> Result<Self, CommitError> {
        let slashed = |part: &str| part.is_empty() || part.contains('/');
        if slashed(build) || slashed(nonce) {
            let message = format!("the pack key of {build:?} and {nonce:?} has a bad part");
            return Err(CommitError::Invalid(message));
        }
        let key = format!("cas/b/{build}/{nonce}");
        let object = ObjectKey::new(key.clone());
        let object = object.map_err(|error| CommitError::Invalid(error.to_string()))?;
        Ok(Self {
            build: build.to_owned(),
            key,
            object,
        })
    }

    /// The object key.
    #[must_use]
    pub fn key(&self) -> &str {
        &self.key
    }

    /// The build the key was issued to.
    #[must_use]
    pub fn build(&self) -> &str {
        &self.build
    }
}

/// The bounds on the commit of an untrusted pack. The defaults are the limits of the format.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UntrustedLimits {
    /// The most bytes of a pack.
    pub max_pack_bytes: u64,
    /// The most blobs of a pack.
    pub max_blobs: usize,
    /// The most bytes of a blob.
    pub max_blob_bytes: u64,
    /// The most bytes one commit may read from the store, checked before the first read.
    pub read_budget: u64,
    /// The most bytes of one large object of a run ([`MAX_UNTRUSTED_LARGE_BYTES`]): a manifest
    /// that names a larger one is refused as a bad manifest. 0 refuses every large object.
    pub max_large_bytes: u64,
}

impl Default for UntrustedLimits {
    fn default() -> Self {
        Self {
            max_pack_bytes: MAX_UNTRUSTED_PACK_BYTES,
            max_blobs: MAX_COMMIT_BLOBS,
            max_blob_bytes: MAX_UNTRUSTED_BLOB_BYTES,
            read_budget: MAX_UNTRUSTED_READ_BYTES,
            max_large_bytes: MAX_UNTRUSTED_LARGE_BYTES,
        }
    }
}

/// What the manifest alone shows about a pack's layout.
struct Layout {
    /// Where the records end and the footer starts.
    records_len: u64,
    /// The indexes of the manifest's blobs, in the order of their offsets.
    by_offset: Vec<usize>,
}

impl Packs {
    /// Records `pack`, which a build says it wrote under the key `issued` for it, after reading
    /// the pack's footer and hashing every record (the module docs say how). `pack` is the
    /// manifest the build reported over the lease's channel; the index records its writer as
    /// `b/<build>`.
    ///
    /// # Errors
    /// [`CommitError`]: the key or the writer is not the one issued, the manifest cannot be a
    /// pack's or is over `limits`, the pack is not as long as said, its footer is not the
    /// manifest, a record is not its digest, the key was recorded with other contents, or the
    /// store or the index failed.
    pub async fn commit_untrusted_pack(
        &self,
        issued: &IssuedPackKey,
        pack: NewPack,
        limits: &UntrustedLimits,
    ) -> Result<Recorded, CommitError> {
        let key = pack.key.clone();
        let done = self.committed_untrusted(issued, pack, limits).await;
        match &done {
            Ok(recorded) if recorded.created => self.metrics.untrusted_commits.add(1),
            Err((stage, error)) => {
                if let Some(reason) = reason_of(*stage, error) {
                    self.metrics.untrusted_rejects[reason as usize].add(1);
                }
            }
            Ok(_) => {}
        }
        self.counted(&key, done.map_err(|(_, error)| error))
    }

    /// The commit, without its counts; a refusal says which step made it.
    async fn committed_untrusted(
        &self,
        issued: &IssuedPackKey,
        pack: NewPack,
        limits: &UntrustedLimits,
    ) -> Result<Recorded, (Stage, CommitError)> {
        let layout = laid_out(issued, &pack, limits).map_err(|error| (Stage::Layout, error))?;
        let proof = self.proved(&issued.object, &pack, &layout).await;
        proof.map_err(|error| (Stage::Proof, error))?;
        let recorded = NewPack {
            writer: format!("b/{}", issued.build),
            ..pack
        };
        let index = Arc::clone(&self.index);
        let done = offload(move || index.record_pack(&recorded)).await;
        done.map_err(|error| (Stage::Index, recording_failed(error)))
    }

    /// Reads the trailer and the footer of the pack `key`, requires the footer to be the manifest
    /// of `pack`, and hashes every record.
    async fn proved(
        &self,
        key: &ObjectKey,
        pack: &NewPack,
        layout: &Layout,
    ) -> Result<(), CommitError> {
        let trailer = TRAILER_LEN as u64;
        // One byte more than the trailer: the pack is not longer than said.
        let last = ByteRange::new(pack.size - trailer, trailer + 1).expect("a trailer has bytes");
        let tail = self.fetch(key, last).await?;
        if tail.len() as u64 != trailer {
            let message = format!("{key} is not the {} bytes said", pack.size);
            return Err(CommitError::Missing(message));
        }
        let wanted = Footer::len_from_trailer(&tail).map_err(damaged)?;
        let listed = footer_len(pack.blobs.len() as u64);
        if wanted != listed {
            let message = format!(
                "the footer of {key} is {wanted} bytes, the manifest's {} blobs need {listed}",
                pack.blobs.len()
            );
            return Err(CommitError::Invalid(message));
        }
        let range = ByteRange::new(pack.size - wanted, wanted).expect("a footer has bytes");
        let bytes = self.fetch(key, range).await?;
        let footer = Footer::parse(&bytes, pack.size).map_err(damaged)?;
        let entries = footer.entries();
        let same = footer.records_len() == layout.records_len
            && entries.iter().zip(&pack.blobs).all(|(entry, blob)| {
                entry.digest.hash == blob.digest.hash
                    && entry.digest.size_bytes == blob.digest.size
                    && entry.offset == blob.offset
            });
        if !same {
            let message = format!("the footer of {key} is not the manifest");
            return Err(CommitError::Invalid(message));
        }
        for &at in &layout.by_offset {
            let blob = &pack.blobs[at];
            let (digest, crc) = self.hashed_crc(key, blob).await?;
            if digest != blob.digest || crc != entries[at].crc32c {
                let offset = blob.offset;
                let message = format!("the record at {offset} of {key} holds other bytes");
                return Err(CommitError::Mismatch(message));
            }
        }
        Ok(())
    }
}

/// The step of a commit that refused it.
#[derive(Clone, Copy)]
enum Stage {
    /// The manifest alone: the key, the shape and the layout.
    Layout,
    /// The reads: the trailer, the footer and every record.
    Proof,
    /// The pack index.
    Index,
}

/// Under which reason a refusal is counted; none for a store or an index that failed.
fn reason_of(stage: Stage, error: &CommitError) -> Option<Reason> {
    match (error, stage) {
        (CommitError::Forbidden(_), _) => Some(Reason::Forbidden),
        (CommitError::Invalid(_), Stage::Proof) => Some(Reason::Footer),
        (CommitError::Invalid(_), _) => Some(Reason::Shape),
        (CommitError::Mismatch(_), _) => Some(Reason::Mismatch),
        (CommitError::Missing(_), _) => Some(Reason::Missing),
        (CommitError::Conflict(_), _) => Some(Reason::Conflict),
        (CommitError::Unavailable(_), _) => None,
    }
}

/// The refusal for a footer that cannot be one.
fn damaged(error: SegmentError) -> CommitError {
    CommitError::Invalid(format!("the pack's footer: {error}"))
}

/// The layout of `pack`, if `issued` may commit it and its manifest can be a pack's within
/// `limits`. Nothing is read.
fn laid_out(
    issued: &IssuedPackKey,
    pack: &NewPack,
    limits: &UntrustedLimits,
) -> Result<Layout, CommitError> {
    if pack.key != issued.key || pack.writer != issued.build {
        let message = format!(
            "the build {:?} may not commit {:?}: the key issued to {:?} is {:?}",
            pack.writer, pack.key, issued.build, issued.key
        );
        return Err(CommitError::Forbidden(message));
    }
    let invalid = |message: String| Err(CommitError::Invalid(message));
    let given = pack.blobs.len();
    if given == 0 || given > limits.max_blobs {
        return invalid(format!(
            "{given} blobs in one pack, 1 to {}",
            limits.max_blobs
        ));
    }
    let footer = footer_len(given as u64);
    if pack.size > limits.max_pack_bytes || pack.size < footer {
        let max = limits.max_pack_bytes;
        return invalid(format!("a pack of {} bytes, {footer} to {max}", pack.size));
    }
    let key = |blob: &PackBlob| (blob.digest.hash, blob.digest.size);
    let ascending = pack
        .blobs
        .windows(2)
        .all(|pair| key(&pair[0]) < key(&pair[1]));
    if !ascending {
        return invalid("the blobs are not in strictly ascending (hash, size) order".to_owned());
    }
    if pack
        .blobs
        .iter()
        .any(|blob| blob.digest.size > limits.max_blob_bytes)
    {
        return invalid(format!("a blob of over {} bytes", limits.max_blob_bytes));
    }
    if pack.blobs.iter().any(|blob| blob.digest.size == 0) {
        return invalid("a record of no bytes".to_owned());
    }
    let mut by_offset: Vec<usize> = (0..given).collect();
    by_offset.sort_by_key(|&at| pack.blobs[at].offset);
    let mut end = 0_u64;
    for &at in &by_offset {
        let blob = &pack.blobs[at];
        if blob.offset < end {
            return invalid(format!(
                "the record at {} overlaps the one before it",
                blob.offset
            ));
        }
        if blob.offset > end {
            return invalid(format!(
                "a gap of records between {end} and {}",
                blob.offset
            ));
        }
        end = end.saturating_add(blob.digest.size);
    }
    let records_len = pack.size - footer;
    if end != records_len {
        return invalid(format!(
            "the records end at {end}, the footer starts at {records_len}"
        ));
    }
    // The trailer and the one byte past it, the footer (which holds the trailer again) and the
    // records.
    let reads = records_len + footer + TRAILER_LEN as u64 + 1;
    if reads > limits.read_budget {
        let budget = limits.read_budget;
        return invalid(format!("the commit reads {reads} bytes, at most {budget}"));
    }
    Ok(Layout {
        records_len,
        by_offset,
    })
}
