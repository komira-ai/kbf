//! Directory rows: the bytes of each REAPI `Directory` a walk of an input tree has read, by
//! the Directory's own digest.
//!
//! A Directory is a blob like any other and keeps its row in the pack index; this table holds
//! its bytes as well, so a walk reads a whole level of a tree with one query
//! ([`PackIndex::directories`]) instead of one ranged read of the object store per Directory.
//! `PostgresPackIndex` keeps the rows in the `directory` table of migration 4
//! ([`crate::schema`]); `MemoryPackIndex` in a map. The rules, which `directory_conformance`
//! states for both:
//!
//! - **A row is content-addressed and never changes.** The body's SHA-256 is the digest's hash
//!   and its length the digest's size; the index checks both and refuses the whole call
//!   ([`PackIndexError::Invalid`]) when one body is not its digest, so no row is ever a wrong
//!   answer. A digest written again is the same bytes, so the second write changes nothing and
//!   is not counted.
//! - **Bodies and batches are bounded.** A body longer than [`MAX_DIRECTORY_BYTES`] is
//!   refused (the caller keeps reading such a Directory from its blob); a call names at most
//!   [`crate::MAX_BATCH`] digests or Directories.
//! - **A read is a query.** [`PackIndex::directories`] writes nothing, and a digest with no row
//!   is `None`: the caller reads the blob and may record it then.
//! - **Rows are not decoded.** The index never parses a body as a Directory: what a row holds
//!   is the blob's bytes, and what they mean is the reader's.
//!
//! [`PackIndex::directories`]: crate::PackIndex::directories
//! [`PackIndexError::Invalid`]: crate::PackIndexError::Invalid

use crate::BlobDigest;

/// The longest Directory body a row holds: 4 MiB, a Directory of tens of thousands of entries.
pub const MAX_DIRECTORY_BYTES: usize = 4 << 20;

/// A Directory to record: its digest and its encoded bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewDirectory {
    /// The Directory's digest: the SHA-256 and the length of `body`.
    pub digest: BlobDigest,
    /// The encoded Directory, as the client uploaded it.
    pub body: Vec<u8>,
}

/// The Directories of one call sorted by digest with repeats of a digest dropped, once each is
/// known to be its digest and within the bounds. Both indexes insert in this order, so two
/// calls never wait on each other in a cycle.
#[cfg(any(test, feature = "test-support", feature = "postgres"))]
pub(crate) fn checked(
    directories: &[NewDirectory],
) -> Result<Vec<&NewDirectory>, crate::PackIndexError> {
    use sha2::{Digest as _, Sha256};

    crate::check_batch(directories.len())?;
    for directory in directories {
        if directory.body.len() > MAX_DIRECTORY_BYTES {
            return Err(crate::PackIndexError::Invalid(
                "a Directory is longer than a directory row holds",
            ));
        }
        let length = u64::try_from(directory.body.len()).unwrap_or(u64::MAX);
        let hash: [u8; 32] = Sha256::digest(&directory.body).into();
        if length != directory.digest.size || hash != directory.digest.hash {
            return Err(crate::PackIndexError::Invalid(
                "a Directory's bytes are not its digest",
            ));
        }
    }
    let mut sorted: Vec<&NewDirectory> = directories.iter().collect();
    sorted.sort_by_key(|directory| directory.digest);
    sorted.dedup_by_key(|directory| directory.digest);
    Ok(sorted)
}
